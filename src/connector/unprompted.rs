//! Speaking because something happened, not because somebody asked.
//!
//! Owner, 2026-09-02: *"isn't a random timer predestined, not organic?"* It is.
//! People speak unprompted because something happened to them, because they
//! left a loop open, or because the room is alive and they are in it. Four
//! sources, one path, one budget:
//!
//! 1. **impulses** - the inlet directory ([`crate::impulses`]);
//! 2. **open loops** - a question of mine nobody answered, or a
//!    `[[followup: ...]]` the brain promised ([`crate::loops`]);
//! 3. **inner thoughts** - the judge kept saying "no, but I do want to say
//!    something" until it crossed the threshold;
//! 4. **the heartbeat** - the same thing on a timer, kept as a hidden fallback
//!    because a timer is the least organic reason there is.
//!
//! Every one of them is a CANDIDATE, never a message: it waits for a human to
//! be present, takes the same back-off, re-reads the room, usually asks the
//! judge, and spends `tier2_per_hour_max`.

use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::ruma::RoomId;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::brain::{Occasion, python_bool};
use crate::events::{NOTICE_MSGTYPE, RoomEvent};
use crate::head;
use crate::impulses::{Impulse, drop_expired, impulse_dir, read_impulses};
use crate::loops::{STATE_CLOSED, extract_followup};
use crate::presence::UNKNOWN;

use super::turn::{Runner, uniform};
use super::{RoomWorker, WorkerState};

/// How often the unprompted loop looks at the impulse inlet, at the open loops
/// and at whether a human has turned up. A poll rather than inotify: five
/// seconds is nothing next to a back-off measured in tens of seconds, and it
/// needs no platform-specific dependency to watch one small directory.
pub const UNPROMPTED_POLL_S: f64 = 5.0;
/// How many unprompted candidates may wait at once. The inlet is a public
/// interface - a looping hook can write a thousand files - and a queue is a
/// queue: past this, the newest are left on disk to expire on their own rather
/// than read, held in memory and eventually said.
pub const MAX_QUEUED: usize = 20;

/// Timing hazard. Nobody starts a conversation at 3 a.m. in an empty room, and
/// everybody chips in while the talking is still warm - so the back-off range is
/// halved while a human has just posted and doubled in a room that has gone
/// quiet. The two clocks are deliberately different: "a human posted" is about
/// people, "the room is quiet" is about the room, bots included.
pub const HAZARD_LIVELY_S: f64 = 600.0;
pub const HAZARD_LIVELY_FACTOR: f64 = 0.5;
pub const HAZARD_QUIET_S: f64 = 3600.0;
pub const HAZARD_QUIET_FACTOR: f64 = 2.0;

/// How wide the back-off stays for a line that scored `policy.prescore_fast`.
/// Not zero: two agents that both answer at once answer on top of each other,
/// and a few seconds of jitter is what keeps one of them from having to.
pub const PRESCORE_FAST_WINDOW_S: f64 = 5.0;

/// An inner-thought accumulator is dropped after this long without a message in
/// that thread. Wanting to say something is about a conversation; when the
/// conversation is over, so is the urge.
pub const INNER_QUIET_S: f64 = 1800.0;
/// The accumulator key for messages that are not in a thread: the room's main
/// timeline is one conversation, and every unthreaded message being its own
/// "thread root" would mean nothing ever accumulated.
pub const MAIN_TIMELINE: &str = "";

/// One reason to speak that did not come from a message in the room.
///
/// Queued rather than acted on, because the first question is never "what do I
/// say" but "is there anybody here": a candidate waits in the room's queue until
/// a human is present, and gives up on itself after
/// `policy.unprompted_max_wait_min`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: Occasion,
    /// What it is about, in one line, for the judge and the brain.
    pub note: String,
    pub queued_ts: f64,
    /// Where it belongs. None posts unthreaded, addressed to nobody.
    pub thread_root: Option<String>,
    pub impulse: Option<Impulse>,
    /// The event id of the loop this follows up, as the ledger knows it.
    pub loop_event_id: Option<String>,
    pub needs_judge: bool,
}

impl Candidate {
    #[must_use]
    pub fn new(kind: Occasion, note: String, queued_ts: f64) -> Self {
        Self {
            kind,
            note,
            queued_ts,
            thread_root: None,
            impulse: None,
            loop_event_id: None,
            needs_judge: true,
        }
    }

    /// Stable identity, so nothing is queued twice.
    #[must_use]
    pub fn source_id(&self) -> String {
        if let Some(impulse) = &self.impulse {
            return format!("impulse:{}", impulse.id());
        }
        if let Some(event_id) = &self.loop_event_id {
            return format!("loop:{event_id}");
        }
        format!(
            "{}:{}",
            self.kind.as_str(),
            self.thread_root.as_deref().unwrap_or(MAIN_TIMELINE)
        )
    }
}

/// Put one reason to speak in the queue, unless it is already there.
pub fn queue_candidate(state: &mut WorkerState, room_id: &RoomId, candidate: Candidate) -> bool {
    let source_id = candidate.source_id();
    if state.queued.contains(&source_id) {
        return false;
    }
    info!(
        "{room_id}: {} queued, waiting for somebody to be here: {}",
        candidate.kind.as_str(),
        head(&candidate.note, 120)
    );
    state.queued.insert(source_id);
    state.queue.push(candidate);
    true
}

/// Drop a queued candidate whose reason has gone away.
pub fn forget_candidate(state: &mut WorkerState, source_id: &str) {
    if !state.queued.remove(source_id) {
        return;
    }
    state
        .queue
        .retain(|candidate| candidate.source_id() != source_id);
}

/// Whatever became of it, a candidate is spent. Loops close with it.
pub fn close_candidate(state: &mut WorkerState, candidate: &Candidate, reason: &str) {
    if let Some(event_id) = &candidate.loop_event_id {
        let open = state
            .ledger
            .loop_by_event(event_id)
            .is_some_and(|loop_| loop_.state != STATE_CLOSED);
        if open {
            state.ledger.close_loop(event_id, reason);
        }
    }
}

/// I said something here, so whatever I was holding in is out.
pub fn reset_inner_urgency(state: &mut WorkerState, thread_root: Option<&str>) {
    let key = thread_root.unwrap_or(MAIN_TIMELINE).to_owned();
    state.inner_urgency.remove(&key);
    state.inner_seen_ts.remove(&key);
}

/// How eager to speak this room is right now.
///
/// The back-off is a wait for a gap in the conversation, so how long a gap is
/// worth waiting for depends on the conversation. Right after a human has
/// spoken there is one; in a room nobody has touched for an hour there is not,
/// and being the thing that breaks an hour of silence deserves a longer look at
/// whether it is worth it.
#[must_use]
pub fn hazard_factor(now: f64, last_human_post_ts: f64, last_activity_ts: f64) -> f64 {
    if now - last_human_post_ts < HAZARD_LIVELY_S {
        return HAZARD_LIVELY_FACTOR;
    }
    if now - last_activity_ts > HAZARD_QUIET_S {
        return HAZARD_QUIET_FACTOR;
    }
    1.0
}

/// The range a TIER-2 back-off is drawn from, before anything is drawn.
///
/// Two shapes, and the second one is the whole point of the pre-score. Below
/// `prescore_fast` it is the configured range scaled by [`hazard_factor`],
/// exactly as it has always been. At or above it, the line is one somebody is
/// visibly waiting on - a question, a "you", my name, my subject - and the
/// range collapses to the configured floor plus [`PRESCORE_FAST_WINDOW_S`],
/// with NO hazard at all: the hazard is about the mood of a room, and a
/// question put to it is not a mood.
///
/// It still leaves the floor in place, because the floor is the collision
/// avoidance: two agents that both answer instantly answer on top of each
/// other. What it removes is the waiting somebody can feel.
#[must_use]
pub fn tier2_range(cfg: &crate::config::PolicyConfig, prescore: u8, hazard: f64) -> (f64, f64) {
    let (low, high) = cfg.backoff_s;
    if prescore >= cfg.prescore_fast {
        return (low, low + PRESCORE_FAST_WINDOW_S);
    }
    (low * hazard, high * hazard)
}

/// Read the inlet directory: fresh impulses in, stale ones deleted.
pub fn collect_impulses(
    state: &mut WorkerState,
    room_id: &RoomId,
    directory: &std::path::Path,
    ttl_s: f64,
    now: f64,
) {
    if !directory.is_dir() {
        return;
    }
    for impulse in drop_expired(read_impulses(directory, ttl_s), now) {
        if state.queue.len() >= MAX_QUEUED {
            warn!(
                "{room_id}: {} unprompted candidates already waiting; leaving the rest of the \
                 inlet on disk until somebody is here",
                state.queue.len()
            );
            return;
        }
        let mut candidate = Candidate::new(Occasion::Impulse, impulse.note(), now);
        candidate.impulse = Some(impulse);
        queue_candidate(state, room_id, candidate);
    }
}

/// Queue the follow-ups whose delay has run out.
pub fn collect_due_loops(state: &mut WorkerState, room_id: &RoomId, now: f64) {
    for loop_ in state.ledger.due_loops(now) {
        let mut candidate = Candidate::new(Occasion::Followup, loop_.text.clone(), now);
        candidate.thread_root = Some(loop_.thread_root.clone());
        candidate.loop_event_id = Some(loop_.event_id.clone());
        queue_candidate(state, room_id, candidate);
    }
}

/// Give up on candidates nobody turned up for.
///
/// Two clocks, because a candidate that is queued is still ageing: the wait
/// itself (`unprompted_max_wait_min` - a thought that has been waiting four
/// hours for company is not a thought any more, it is a notification), and, for
/// an impulse, its own `ttl_s`. Without the second one an impulse with a
/// five-minute lifetime could sit in the queue for hours and still be said,
/// which is exactly what a lifetime is for.
pub fn expire_waiting(state: &mut WorkerState, room_id: &RoomId, now: f64, limit_s: f64) {
    let mut keep: Vec<Candidate> = Vec::with_capacity(state.queue.len());
    for candidate in std::mem::take(&mut state.queue) {
        let waited = now - candidate.queued_ts;
        let reason = if candidate
            .impulse
            .as_ref()
            .is_some_and(|impulse| impulse.expired(now))
        {
            let ttl = candidate.impulse.as_ref().map_or(0.0, |i| i.ttl_s);
            format!("it expired ({ttl:.0} s ttl)")
        } else if limit_s > 0.0 && waited >= limit_s {
            format!("nobody was here for {:.0} min", waited / 60.0)
        } else {
            keep.push(candidate);
            continue;
        };
        info!(
            "{room_id}: giving up on {} - {reason}: {}",
            candidate.kind.as_str(),
            head(&candidate.note, 120)
        );
        state.queued.remove(&candidate.source_id());
        if let Some(impulse) = &candidate.impulse {
            impulse.forget();
        }
        close_candidate(state, &candidate, &reason);
    }
    state.queue = keep;
}

/// Forget every conversation nothing has said anything in for `INNER_QUIET_S`.
///
/// The rule is "the accumulator resets after 30 minutes of quiet", and it used
/// to be applied only to the ONE conversation being probed - so a thread that
/// went quiet and was never probed again kept its urgency, and its timestamp,
/// for the life of the process. In a busy room that is a map entry per thread
/// for ever, and a stale accumulator waiting to be topped up hours later by a
/// conversation that has moved on.
fn forget_quiet_conversations(state: &mut WorkerState, now: f64) {
    state
        .inner_seen_ts
        .retain(|_key, last| now - *last <= INNER_QUIET_S);
    let alive = &state.inner_seen_ts;
    state
        .inner_urgency
        .retain(|key, _total| alive.contains_key(key));
}

/// Add one judgement's urgency to this conversation. Some(candidate) when it
/// has just crossed the threshold and a thought wants raising.
pub fn note_urgency(
    state: &mut WorkerState,
    room_id: &RoomId,
    thread_root: Option<&str>,
    urgency: i32,
    why: &str,
    threshold: i64,
    now: f64,
) -> Option<Candidate> {
    if urgency <= 0 {
        return None;
    }
    let key = thread_root.unwrap_or(MAIN_TIMELINE).to_owned();
    if let Some(last) = state.inner_seen_ts.get(&key)
        && now - last > INNER_QUIET_S
    {
        info!(
            "{room_id}: dropping {} accumulated urgency, that conversation went quiet",
            state.inner_urgency.get(&key).copied().unwrap_or(0)
        );
        state.inner_urgency.remove(&key);
    }
    forget_quiet_conversations(state, now);
    state.inner_seen_ts.insert(key.clone(), now);
    let total = state.inner_urgency.get(&key).copied().unwrap_or(0) + i64::from(urgency);
    let where_ = if key.is_empty() {
        "the main timeline"
    } else {
        &key
    };
    if total < threshold {
        state.inner_urgency.insert(key.clone(), total);
        info!("{room_id}: inner thoughts at {total}/{threshold} in {where_}");
        return None;
    }
    // Over the line: the accumulator resets now, not when the candidate is
    // spoken, so a queue that waits all afternoon for a human cannot also be
    // building a second one behind it.
    state.inner_urgency.remove(&key);
    info!("{room_id}: inner thoughts reached {total}/{threshold} in {where_}; raising a candidate");
    let mut candidate = Candidate::new(Occasion::InnerThought, why.to_owned(), now);
    candidate.thread_root = thread_root.map(ToOwned::to_owned);
    candidate.needs_judge = false;
    Some(candidate)
}

impl Runner {
    // -- the room's slow clock --------------------------------------------

    /// Look at the inlet, the loops, and the door, until the connector stops.
    pub(crate) async fn unprompted_loop(
        self,
        worker: Arc<RoomWorker>,
        stop: &mut watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
                () = tokio::time::sleep(Duration::from_secs_f64(UNPROMPTED_POLL_S)) => {
                    self.unprompted_tick(&worker).await;
                }
            }
        }
    }

    /// One pass: collect what wants saying, then say at most one of it.
    pub(crate) async fn unprompted_tick(&self, worker: &Arc<RoomWorker>) {
        let now = self.now();
        let directory = impulse_dir(&self.cfg.state_dir, worker.room_id.as_str());
        #[allow(clippy::cast_precision_loss)]
        let limit_s = self.cfg.policy.unprompted_max_wait_min as f64 * 60.0;
        let (waiting, last_human_post_ts) = {
            let mut state = worker.state.lock().await;
            collect_impulses(
                &mut state,
                &worker.room_id,
                &directory,
                self.cfg.policy.impulse_ttl_s,
                now,
            );
            collect_due_loops(&mut state, &worker.room_id, now);
            expire_waiting(&mut state, &worker.room_id, now, limit_s);
            if state.queue.is_empty() {
                return;
            }
            if state.busy || state.deliberating {
                debug!(
                    "{}: {} unprompted waiting, the room is busy",
                    worker.room_id,
                    state.queue.len()
                );
                return;
            }
            (state.queue.len(), state.last_human_post_ts)
        };
        // Presence needs the member list and the presence book, so it is asked
        // with no room lock held.
        let (present, why) = self.humans_present(worker, last_human_post_ts).await;
        if !present {
            debug!(
                "{}: {waiting} unprompted waiting for somebody to be here ({why})",
                worker.room_id
            );
            return;
        }
        let candidate = {
            let mut state = worker.state.lock().await;
            if state.busy || state.deliberating || state.queue.is_empty() {
                return;
            }
            let candidate = state.queue.remove(0);
            state.queued.remove(&candidate.source_id());
            if let Some(impulse) = &candidate.impulse {
                // One chance per impulse. It has been waited for, a human is
                // here, and re-judging the same line every five seconds would
                // be a bill.
                impulse.forget();
            }
            if let Some(event_id) = &candidate.loop_event_id {
                state.ledger.raise_loop(event_id);
            }
            state.deliberating = true;
            candidate
        };
        info!(
            "{}: taking up {} ({why}) - {}",
            worker.room_id,
            candidate.kind.as_str(),
            head(&candidate.note, 120)
        );
        self.speak_unprompted(worker, candidate).await;
        worker.state.lock().await.deliberating = false;
    }

    // -- who is in the room ------------------------------------------------

    /// The humans this room has, by membership, or by who has spoken.
    ///
    /// Membership is the right answer and is what makes presence work before
    /// anybody has said a word. The transcript is the fallback for a homeserver
    /// that lazy-loads members: somebody who has posted here is a member of this
    /// room whatever the member list has got round to saying.
    pub(crate) async fn human_members(&self, worker: &RoomWorker) -> Vec<String> {
        let members: Vec<String> = match self.client.get_room(&worker.room_id) {
            Some(room) => room
                .joined_user_ids()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|user| user.to_string())
                .collect(),
            None => Vec::new(),
        };
        let humans: Vec<String> = members
            .into_iter()
            .filter(|user_id| *user_id != self.me && !self.is_bot_user(user_id))
            .collect();
        if !humans.is_empty() {
            return humans;
        }
        let mut spoken: Vec<String> = worker
            .transcript
            .recent(self.cfg.history_limit)
            .into_iter()
            .filter(|ev| !ev.is_bot && ev.sender != self.me)
            .map(|ev| ev.sender)
            .collect();
        spoken.sort();
        spoken.dedup();
        spoken
    }

    pub(crate) fn is_bot_user(&self, user_id: &str) -> bool {
        crate::events::is_bot_user(user_id, &self.bot_user_ids, &self.bot_patterns)
    }

    /// Is there anybody to say this to? With the reason, for the log.
    ///
    /// Two ways of being here, because neither is enough on its own: the
    /// homeserver's `m.presence` (which a phone that went to sleep gets wrong)
    /// and "a human posted here recently" (which a lurker gets wrong). Either
    /// one counts.
    pub(crate) async fn humans_present(
        &self,
        worker: &RoomWorker,
        last_human_post_ts: f64,
    ) -> (bool, String) {
        let humans = self.human_members(worker).await;
        if humans.is_empty() {
            return (false, "the room has no human members I know of".to_owned());
        }
        {
            let book = self.presence.lock().await;
            if let Some(online) = book.online_among(humans.iter().map(String::as_str)) {
                return (true, format!("{online} is online"));
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let window = self.cfg.policy.presence_window_min as f64 * 60.0;
        let since = self.now() - last_human_post_ts;
        if window > 0.0 && since < window {
            return (
                true,
                format!("a human posted here {:.0} min ago", since / 60.0),
            );
        }
        let book = self.presence.lock().await;
        let mut states: Vec<String> = humans
            .iter()
            .map(|user_id| format!("{user_id}={}", book.state_of(user_id)))
            .collect();
        states.sort();
        drop(book);
        let _ = UNKNOWN;
        (
            false,
            format!(
                "no human is here ({}), and none has posted within {:.0} min",
                states.join(", "),
                window / 60.0
            ),
        )
    }

    // -- the back-off ------------------------------------------------------

    /// A random moment to wait before speaking uninvited.
    ///
    /// The point is not politeness, it is collision avoidance: several agents
    /// watching the same message must not all start typing at once. Whoever
    /// draws the shortest wait speaks; the others re-read the room and stand
    /// down. It is a probabilistic mechanism - two draws inside the time it
    /// takes a message to reach the others will still both answer, exactly as
    /// two people can start talking at once - which is why the shipped default
    /// is a wide 5-40 s and why the gates use disjoint ranges.
    ///
    /// The range is scaled by [`hazard_factor`], so the same configuration means
    /// "quickly" in a live conversation and "think about it" in a dead room.
    #[must_use]
    pub(crate) fn backoff_delay(&self, state: &WorkerState) -> f64 {
        let (low, high) = self.cfg.policy.backoff_s;
        let factor = hazard_factor(self.now(), state.last_human_post_ts, state.last_activity_ts);
        uniform(low * factor, high * factor)
    }

    /// The same wait, for a tier-2 line that carries a pre-score.
    #[must_use]
    pub(crate) fn tier2_delay(&self, state: &WorkerState, prescore: u8) -> f64 {
        let factor = hazard_factor(self.now(), state.last_human_post_ts, state.last_activity_ts);
        let (low, high) = tier2_range(&self.cfg.policy, prescore, factor);
        uniform(low, high)
    }

    // -- one unprompted turn ------------------------------------------------

    /// Back off, re-read, maybe ask the judge, and maybe say the thing.
    ///
    /// The same shape as tier 2 and for the same reasons - the back-off is
    /// collision avoidance, the re-read is standing down when the room has moved
    /// on - with one difference: an inner thought is not judged again. The judge
    /// already answered, several times, with the urgency that added up to this;
    /// asking it "should I speak?" once more would only give it a chance to talk
    /// itself out of what it kept saying it wanted.
    pub(crate) async fn speak_unprompted(&self, worker: &Arc<RoomWorker>, candidate: Candidate) {
        let started = self.now();
        let room_id = worker.room_id.clone();
        let anchor = self.anchor(worker, &candidate);
        let delay = {
            let state = worker.state.lock().await;
            self.backoff_delay(&state)
        };
        info!(
            "{room_id}: {}: waiting {delay:.1} s before speaking uninvited",
            candidate.kind.as_str()
        );
        self.brain
            .warm(&format!(
                "an unprompted {} in {room_id}",
                candidate.kind.as_str()
            ))
            .await;
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;

        if let Some(reason) = self
            .unprompted_stand_down(worker, &candidate, started)
            .await
        {
            info!("{room_id}: {} dropped: {reason}", candidate.kind.as_str());
            close_candidate(&mut *worker.state.lock().await, &candidate, &reason);
            return;
        }

        let ctx = self.context(
            worker,
            &anchor,
            candidate.kind,
            candidate.note.clone(),
            candidate.thread_root.as_deref(),
        );
        if candidate.needs_judge {
            let judgement = self.brain.judge(&ctx).await;
            info!(
                "{room_id}: judge on this {} says speak={} ({})",
                candidate.kind.as_str(),
                python_bool(judgement.speak),
                judgement.why
            );
            if !judgement.speak {
                let reason = format!("the judge said no: {}", judgement.why);
                close_candidate(&mut *worker.state.lock().await, &candidate, &reason);
                return;
            }
        }

        {
            let mut state = worker.state.lock().await;
            if let Some(refusal) = Self::unprompted_budget_refusal(&state) {
                info!(
                    "{room_id}: {} judged yes but {refusal}",
                    candidate.kind.as_str()
                );
                close_candidate(&mut state, &candidate, &refusal);
                return;
            }
            // `busy` stays up across the post, not only across the brain call: a
            // tier-1 turn starting in between would put two messages from me in
            // the room a second apart, about different things.
            state.busy = true;
        }
        self.finish_unprompted(worker, &candidate, &ctx).await;
        worker.state.lock().await.busy = false;
    }

    /// Ask the brain, and post what it says. `busy` is already up.
    async fn finish_unprompted(
        &self,
        worker: &Arc<RoomWorker>,
        candidate: &Candidate,
        ctx: &crate::brain::BrainContext,
    ) {
        let Some(answer) = self.ask_for_a_message(&worker.room_id, ctx).await else {
            close_candidate(
                &mut *worker.state.lock().await,
                candidate,
                "the brain stayed quiet",
            );
            return;
        };
        let (text, followup) = extract_followup(&answer);
        if text.is_empty() {
            close_candidate(
                &mut *worker.state.lock().await,
                candidate,
                "the answer was only a followup marker",
            );
            return;
        }
        self.post_unprompted(worker, candidate, &text, &followup)
            .await;
    }

    /// The room event an unprompted turn is rendered around.
    ///
    /// Not a trigger: nobody said this to me. It is where the conversation
    /// stands, so the brain knows what it is walking into - and when the room
    /// has said nothing at all, a stand-in carrying the candidate itself, which
    /// the rendering never shows as a line (see `brain::rendering`).
    fn anchor(&self, worker: &Arc<RoomWorker>, candidate: &Candidate) -> RoomEvent {
        let events = match &candidate.thread_root {
            Some(root) => worker.transcript.thread(root),
            None => worker.transcript.recent(self.cfg.history_limit),
        };
        if let Some(last) = events.last() {
            // The anchor belongs to the candidate's thread, not to its own: an
            // impulse is about the room, and if the last thing said happened to
            // be inside somebody's thread, the brain would otherwise be shown
            // that thread instead of the room it is about to speak into.
            let mut anchor = last.clone();
            anchor.thread_root.clone_from(&candidate.thread_root);
            return anchor;
        }
        RoomEvent {
            event_id: String::new(),
            room_id: worker.room_id.to_string(),
            sender: self.me.clone(),
            sender_display: None,
            body: candidate.note.clone(),
            formatted_body: None,
            msgtype: NOTICE_MSGTYPE.to_owned(),
            ts: self.now(),
            thread_root: None,
            reply_to: None,
            reply_is_fallback: false,
            mentions: std::collections::BTreeSet::new(),
            is_bot: true,
        }
    }

    /// Why this unprompted turn should be dropped, or None to carry on.
    async fn unprompted_stand_down(
        &self,
        worker: &Arc<RoomWorker>,
        candidate: &Candidate,
        since: f64,
    ) -> Option<String> {
        {
            let state = worker.state.lock().await;
            if state.busy || !state.pending.is_empty() {
                return Some("a turn I was addressed in is already running".to_owned());
            }
            if state.ledger.posts.iter().any(|post| post.ts >= since) {
                return Some(
                    "I have already spoken in this room since the back-off started".to_owned(),
                );
            }
            if let Some(event_id) = &candidate.loop_event_id
                && let Some(loop_) = state.ledger.loop_by_event(event_id)
                && loop_.state == STATE_CLOSED
            {
                return Some(format!(
                    "the loop closed while I was waiting ({})",
                    loop_.reason
                ));
            }
        }
        let thread_root = candidate.thread_root.as_deref()?;
        // A follow-up belongs to a thread, so the question is the same one tier
        // 2 asks, and it is asked of `/messages` for the same reason: the sync
        // loop is a long poll, and an answer may be in the room without having
        // reached this process yet.
        let newest = self
            .room_tail(&worker.room_id)
            .await
            .into_iter()
            .filter(|ev| ev.sender != self.me && ev.thread_root_or_self() == thread_root)
            .map(|ev| ev.ts)
            .fold(0.0_f64, f64::max);
        if newest >= since {
            return Some("somebody else spoke in that thread while I was waiting".to_owned());
        }
        None
    }

    // -- inner thoughts -----------------------------------------------------

    /// Ask what I would have said, on a message the guards already refused.
    ///
    /// The answer is never posted here. Only the urgency is kept, and only so
    /// that the fourth time in a row the judge says "no, but I do want to say
    /// something" it stops being a no.
    pub(crate) async fn inner_thought_probe(self, worker: Arc<RoomWorker>, ev: RoomEvent) {
        let ctx = self.context(
            &worker,
            &ev,
            Occasion::Unaddressed,
            String::new(),
            ev.thread_root.as_deref(),
        );
        let judgement = self.brain.judge(&ctx).await;
        info!(
            "{}: inner-thought probe on {}: speak={} urgency={} ({})",
            worker.room_id,
            ev.event_id,
            python_bool(judgement.speak),
            judgement.urgency,
            judgement.why
        );
        self.note_urgency(&worker, &ev, judgement.urgency, &judgement.why)
            .await;
        worker.state.lock().await.probing = false;
    }

    /// Add one judgement's urgency to this conversation, and maybe queue.
    pub(crate) async fn note_urgency(
        &self,
        worker: &Arc<RoomWorker>,
        ev: &RoomEvent,
        urgency: i32,
        why: &str,
    ) {
        if !self.cfg.policy.inner_thoughts {
            return;
        }
        let now = self.now();
        let mut state = worker.state.lock().await;
        let raised = note_urgency(
            &mut state,
            &worker.room_id,
            ev.thread_root.as_deref(),
            urgency,
            why,
            self.cfg.policy.inner_thoughts_threshold,
            now,
        );
        if let Some(candidate) = raised {
            queue_candidate(&mut state, &worker.room_id, candidate);
        }
    }

    // -- the heartbeat (the fallback timer) ---------------------------------

    /// Wake every `period_s` until the connector stops.
    pub(crate) async fn heartbeat_loop(
        self,
        worker: Arc<RoomWorker>,
        period_s: f64,
        stop: &mut watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
                () = tokio::time::sleep(Duration::from_secs_f64(period_s)) => {
                    self.heartbeat_once(&worker, period_s).await;
                }
            }
        }
    }

    /// Speak unprompted into a room that has gone quiet, if there is a point.
    ///
    /// Guarded five ways before it costs anything: the room must actually be
    /// quiet, somebody must be there to hear it, nothing else may be running
    /// here, the unprompted budgets must allow it, and the judge - whose
    /// standing instruction is "usually no" - must agree.
    pub(crate) async fn heartbeat_once(&self, worker: &Arc<RoomWorker>, period_s: f64) {
        let room_id = worker.room_id.clone();
        let last_human_post_ts = {
            let state = worker.state.lock().await;
            let quiet_for = self.now() - state.last_activity_ts;
            if quiet_for < period_s {
                debug!(
                    "{room_id}: heartbeat skipped, only {quiet_for:.0} s of quiet \
                     (need {period_s:.0})"
                );
                return;
            }
            if state.busy || state.deliberating {
                debug!("{room_id}: heartbeat skipped, the room is already busy");
                return;
            }
            state.last_human_post_ts
        };
        let (present, why) = self.humans_present(worker, last_human_post_ts).await;
        if !present {
            info!("{room_id}: heartbeat skipped, nobody is here: {why}");
            return;
        }
        let history = worker.transcript.recent(self.cfg.history_limit);
        let Some(anchor) = history.last() else {
            debug!("{room_id}: heartbeat skipped, the room has said nothing yet");
            return;
        };
        {
            let state = worker.state.lock().await;
            if let Some(refusal) = Self::unprompted_budget_refusal(&state) {
                info!("{room_id}: heartbeat skipped: {refusal}");
                return;
            }
        }

        // The last thing said is the anchor: a heartbeat is a thought about the
        // room as it stands, not an answer to anybody.
        let ctx = self.context(
            worker,
            anchor,
            Occasion::Heartbeat,
            String::new(),
            anchor.thread_root.as_deref(),
        );
        let judgement = self.brain.judge(&ctx).await;
        info!(
            "{room_id}: heartbeat judge says speak={} ({})",
            python_bool(judgement.speak),
            judgement.why
        );
        if !judgement.speak {
            return;
        }
        let candidate = {
            let mut state = worker.state.lock().await;
            if state.busy {
                return;
            }
            state.busy = true;
            Candidate::new(Occasion::Heartbeat, String::new(), self.now())
        };
        self.finish_unprompted(worker, &candidate, &ctx).await;
        worker.state.lock().await.busy = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::testkit::{FakeClock, ROOM_ID, state};
    use crate::impulses::write_impulse;
    use matrix_sdk::ruma::RoomId;

    fn room() -> matrix_sdk::ruma::OwnedRoomId {
        RoomId::parse("!room:example.com").expect("a room id")
    }

    #[test]
    fn the_hazard_halves_the_wait_while_a_human_is_still_talking() {
        let now = 10_000.0;
        assert!(
            (hazard_factor(now, now - 60.0, now - 60.0) - HAZARD_LIVELY_FACTOR).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn a_pre_scored_line_collapses_the_back_off_and_skips_the_hazard() {
        let cfg = crate::config::PolicyConfig::default();
        assert_eq!(cfg.prescore_fast, 4, "the shipped threshold");

        // Below the threshold: the configured range, scaled by the hazard,
        // exactly as it was before the pre-score existed.
        let (low, high) = tier2_range(&cfg, 3, HAZARD_QUIET_FACTOR);
        assert!((low - 10.0).abs() < f64::EPSILON, "{low}");
        assert!((high - 80.0).abs() < f64::EPSILON, "{high}");

        // At it: the floor plus a few seconds of jitter, and the hazard does
        // not get a say - a question with "you" in it is not a mood.
        for hazard in [HAZARD_LIVELY_FACTOR, 1.0, HAZARD_QUIET_FACTOR] {
            let (low, high) = tier2_range(&cfg, 4, hazard);
            assert!((low - cfg.backoff_s.0).abs() < f64::EPSILON, "{low}");
            assert!(
                (high - (cfg.backoff_s.0 + PRESCORE_FAST_WINDOW_S)).abs() < f64::EPSILON,
                "{high}"
            );
        }
    }

    #[test]
    fn the_hazard_doubles_the_wait_in_a_room_nobody_has_touched() {
        let now = 10_000.0;
        // No human for hours, and nothing at all for over an hour.
        assert!(
            (hazard_factor(now, now - 7_200.0, now - 4_000.0) - HAZARD_QUIET_FACTOR).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn the_hazard_is_one_in_an_ordinary_room() {
        let now = 10_000.0;
        // A human twenty minutes ago, a bot a minute ago: neither lively nor
        // dead, so the configured range is the range.
        assert!((hazard_factor(now, now - 1_200.0, now - 60.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_impulse_becomes_a_candidate_once_and_the_inlet_cannot_flood_the_queue() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        let state_dir = dir.path().join("state");
        for index in 0..(MAX_QUEUED + 5) {
            write_impulse(
                &state_dir,
                ROOM_ID,
                &format!("thing {index}"),
                "note",
                "",
                600.0,
                Some(clock.now()),
            )
            .expect("the impulse is written");
        }
        let directory = impulse_dir(&state_dir, ROOM_ID);
        collect_impulses(&mut state, &room(), &directory, 600.0, clock.now());
        assert_eq!(
            state.queue.len(),
            MAX_QUEUED,
            "the inlet queued more than the room can ever say"
        );
        // The rest stay on disk to expire there rather than in memory.
        assert_eq!(read_impulses(&directory, 600.0).len(), MAX_QUEUED + 5);

        // A second pass queues nothing new: identity is the file name.
        collect_impulses(&mut state, &room(), &directory, 600.0, clock.now());
        assert_eq!(state.queue.len(), MAX_QUEUED);
    }

    #[test]
    fn a_queued_impulse_ages_by_its_own_ttl_and_not_only_by_the_wait() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        let state_dir = dir.path().join("state");
        write_impulse(
            &state_dir,
            ROOM_ID,
            "the render finished",
            "render",
            "",
            60.0,
            Some(clock.now()),
        )
        .expect("the impulse is written");
        let directory = impulse_dir(&state_dir, ROOM_ID);
        collect_impulses(&mut state, &room(), &directory, 60.0, clock.now());
        assert_eq!(state.queue.len(), 1);

        // Nobody turned up for two minutes: the wait limit is hours away, but
        // the impulse's own lifetime has run out.
        clock.advance(120.0);
        expire_waiting(&mut state, &room(), clock.now(), 4.0 * 3600.0);
        assert!(
            state.queue.is_empty(),
            "an impulse that expired while it waited was still going to be said"
        );
        assert!(state.queued.is_empty());
    }

    #[test]
    fn a_candidate_nobody_turned_up_for_gives_up_on_itself() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        let candidate = Candidate::new(Occasion::InnerThought, "something".to_owned(), clock.now());
        assert!(queue_candidate(&mut state, &room(), candidate.clone()));
        assert!(
            !queue_candidate(&mut state, &room(), candidate),
            "the same thought was queued twice"
        );

        clock.advance(3_600.0);
        expire_waiting(&mut state, &room(), clock.now(), 1_800.0);
        assert!(state.queue.is_empty());
    }

    #[test]
    fn a_due_loop_becomes_a_followup_candidate_in_its_own_thread() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        state
            .ledger
            .open_loop("$mine", "$root", "did anyone try it?", clock.now() + 10.0);

        collect_due_loops(&mut state, &room(), clock.now());
        assert!(
            state.queue.is_empty(),
            "a loop was raised before it was due"
        );

        clock.advance(20.0);
        collect_due_loops(&mut state, &room(), clock.now());
        assert_eq!(state.queue.len(), 1);
        let candidate = state.queue[0].clone();
        assert_eq!(candidate.kind, Occasion::Followup);
        assert_eq!(candidate.thread_root.as_deref(), Some("$root"));
        assert_eq!(candidate.note, "did anyone try it?");
        assert!(candidate.needs_judge);

        // Closing it is what a follow-up does, whatever the judge said.
        close_candidate(&mut state, &candidate, "followed up");
        assert!(
            state
                .ledger
                .loop_by_event("$mine")
                .expect("the loop")
                .is_closed()
        );
        collect_due_loops(&mut state, &room(), clock.now());
        assert_eq!(state.queue.len(), 1, "a closed loop came back as due");
    }

    #[test]
    fn a_closed_loops_candidate_is_forgotten_when_somebody_answers() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        state
            .ledger
            .open_loop("$mine", "$root", "well?", clock.now());
        collect_due_loops(&mut state, &room(), clock.now());
        assert_eq!(state.queue.len(), 1);

        state
            .ledger
            .close_loops_in_thread("$root", "@human:example.com posted in the thread");
        forget_candidate(&mut state, "loop:$mine");
        assert!(state.queue.is_empty());
        assert!(state.queued.is_empty());
    }

    #[test]
    fn urgency_adds_up_until_it_stops_being_a_no() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);

        assert!(note_urgency(&mut state, &room(), None, 2, "wanted to", 4, clock.now()).is_none());
        assert_eq!(state.inner_urgency.get(MAIN_TIMELINE), Some(&2));
        let raised = note_urgency(
            &mut state,
            &room(),
            None,
            2,
            "still wanted to",
            4,
            clock.now(),
        )
        .expect("the threshold was crossed");
        assert_eq!(raised.kind, Occasion::InnerThought);
        assert_eq!(raised.note, "still wanted to");
        assert!(
            !raised.needs_judge,
            "the judge already answered, several times"
        );
        assert!(
            !state.inner_urgency.contains_key(MAIN_TIMELINE),
            "the accumulator has to empty as the candidate is raised"
        );
    }

    #[test]
    fn a_zero_urgency_never_accumulates_and_threads_count_separately() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        assert!(note_urgency(&mut state, &room(), None, 0, "nothing", 4, clock.now()).is_none());
        assert!(state.inner_urgency.is_empty());

        note_urgency(&mut state, &room(), Some("$a"), 3, "in a", 4, clock.now());
        note_urgency(&mut state, &room(), Some("$b"), 3, "in b", 4, clock.now());
        assert_eq!(state.inner_urgency.get("$a"), Some(&3));
        assert_eq!(state.inner_urgency.get("$b"), Some(&3));
    }

    #[test]
    fn the_accumulator_is_dropped_when_the_conversation_goes_quiet() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        note_urgency(&mut state, &room(), None, 3, "wanted to", 4, clock.now());
        clock.advance(INNER_QUIET_S + 1.0);
        // Wanting to say something is about a conversation; when the
        // conversation is over, so is the urge.
        assert!(note_urgency(&mut state, &room(), None, 3, "again", 4, clock.now()).is_none());
        assert_eq!(state.inner_urgency.get(MAIN_TIMELINE), Some(&3));
    }

    /// A conversation nobody comes back to must not stay in memory for ever.
    ///
    /// The quiet rule used to be applied only to the thread being probed, so a
    /// thread that went quiet and was never probed again kept both its urgency
    /// and its timestamp for the life of the process - an unbounded map in a
    /// daemon meant to run for months, and a stale accumulator waiting to be
    /// topped up by a conversation that had moved on.
    #[test]
    fn a_conversation_that_goes_quiet_is_forgotten_even_if_nobody_asks_about_it_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        note_urgency(
            &mut state,
            &room(),
            Some("$old"),
            3,
            "wanted to",
            4,
            clock.now(),
        );
        assert_eq!(state.inner_urgency.get("$old"), Some(&3));

        clock.advance(INNER_QUIET_S + 1.0);
        // A different conversation entirely: nothing ever mentions $old again.
        note_urgency(
            &mut state,
            &room(),
            Some("$new"),
            1,
            "elsewhere",
            4,
            clock.now(),
        );
        assert!(
            !state.inner_seen_ts.contains_key("$old"),
            "a thread nobody has spoken in for half an hour is still remembered"
        );
        assert!(
            !state.inner_urgency.contains_key("$old"),
            "a stale accumulator is still waiting to be topped up"
        );
        assert_eq!(state.inner_urgency.get("$new"), Some(&1));
    }

    #[test]
    fn speaking_resets_what_i_was_holding_in() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut state = state(dir.path(), &clock);
        note_urgency(
            &mut state,
            &room(),
            Some("$a"),
            3,
            "wanted to",
            4,
            clock.now(),
        );
        reset_inner_urgency(&mut state, Some("$a"));
        assert!(state.inner_urgency.is_empty());
        assert!(state.inner_seen_ts.is_empty());
    }
}
