//! One turn, from the decision to the message in the room.
//!
//! Everything here runs in a task of its own, off the sync loop, so a back-off
//! measured in tens of seconds and a brain that thinks for a minute never stop
//! the connector receiving events - for this room or for any other. That is
//! also what makes the stand-down re-read meaningful: while a tier-2 back-off
//! sleeps, the room keeps arriving, and somebody else may answer first.
//!
//! One decision lives here rather than in [`crate::policy`], and only because
//! it is about the SHAPE of the turn: whether the judge is asked at all
//! ([`Runner::room_invitation`]). A line a person handed to the room and asked
//! something on is turn allocation, so the agent that survives the back-off
//! and the re-read answers it - and the policy is left saying what it always
//! said about that line, which is that nobody addressed anybody.

use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
use matrix_sdk::ruma::events::receipt::ReceiptThread;
use matrix_sdk::ruma::{EventId, RoomId, UInt};
use matrix_sdk::{Client, Room};
use regex::Regex;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::addressing::invites_an_answer;
use crate::brain::{Brain, BrainContext, Occasion};
use crate::config::Config;
use crate::events::{NOTICE_MSGTYPE, Relation, RoomEvent, build_reply_content, mentioned_user_ids};
use crate::head;
use crate::ledger::Clock;
use crate::loops::{extract_followup, is_open_question, loop_text};
use crate::policy::{Cues, Decision, Verdict, should_reply};
use crate::presence::PresenceBook;

use super::unprompted::{self, Candidate};
use super::{
    LAST_SPEAKER_TAIL, RoomWorker, WorkerState, last_speaker, normalise_event, rules_from,
};

/// How often the typing indicator is refreshed while a brain is thinking. The
/// server-side notice expires, so a slow turn has to say so more than once.
const TYPING_REFRESH_S: u64 = 20;
/// How many events the stand-down re-read pulls from `/messages`.
const STANDDOWN_TAIL: u32 = 30;

/// Everything one turn needs, owned, so it can run in a task of its own.
#[derive(Clone)]
pub struct Runner {
    pub(crate) cfg: Arc<Config>,
    pub(crate) client: Client,
    pub(crate) brain: Arc<dyn Brain>,
    pub(crate) me: String,
    pub(crate) persona: String,
    pub(crate) clock: Clock,
    pub(crate) presence: Arc<Mutex<PresenceBook>>,
    pub(crate) bot_user_ids: Vec<String>,
    pub(crate) bot_patterns: Vec<Regex>,
}

impl Runner {
    pub(crate) fn now(&self) -> f64 {
        (self.clock)()
    }

    /// How many people and agents are joined to this room, as the member store
    /// had it at the last membership change. 0 before the first sync.
    pub(crate) async fn participants(&self, worker: &RoomWorker) -> usize {
        worker.state.lock().await.participants
    }

    // -- the ordinary turn ------------------------------------------------

    /// Run turns for this room one at a time until nothing is pending.
    pub(crate) async fn drain(self, worker: Arc<RoomWorker>, ev: RoomEvent, tier: u8) {
        let mut trigger = Some(ev);
        let mut tier = tier;
        while let Some(current) = trigger {
            self.run_turn(&worker, &current, tier).await;
            trigger = self.next_trigger(&worker).await;
            // Only an addressed event survives `next_trigger`, and being
            // addressed is tier 1 whatever started this drain.
            tier = 1;
        }
        // No await between the last pending check and this line, so nothing can
        // slip into `pending` and be forgotten.
        worker.state.lock().await.busy = false;
    }

    /// Coalesce what arrived during the last turn into one follow-up turn.
    ///
    /// Only an addressed event survives. Anything that would merely have been
    /// CONSIDERED is consumed instead: it arrived while I was already writing
    /// about this room, and the answer I just posted is the room's answer.
    async fn next_trigger(&self, worker: &Arc<RoomWorker>) -> Option<RoomEvent> {
        loop {
            let mut state = worker.state.lock().await;
            if state.pending.is_empty() {
                return None;
            }
            let batch: Vec<RoomEvent> = std::mem::take(&mut state.pending);
            let (earlier, latest) = batch.split_at(batch.len() - 1);
            let latest = latest.first()?.clone();
            for skipped in earlier {
                state.ledger.mark_consumed(&skipped.event_id);
                info!(
                    "{}: coalesced {} into the turn triggered by {}",
                    worker.room_id, skipped.event_id, latest.event_id
                );
            }
            let roots = state.ledger.thread_roots();
            let recent = worker.transcript.recent(LAST_SPEAKER_TAIL);
            let cues = Cues {
                names: &state.names,
                last_speaker: last_speaker(&recent, &latest),
            };
            let decision = should_reply(
                &latest,
                &self.me,
                &state.ledger,
                &roots,
                &self.cfg.policy,
                &cues,
            );
            info!(
                "{} {} from {}: verdict={} ({})",
                worker.room_id,
                latest.event_id,
                latest.sender,
                decision.verdict.as_str(),
                decision.reason
            );
            if decision.reply() {
                return Some(latest);
            }
            state.ledger.mark_consumed(&latest.event_id);
        }
    }

    async fn run_turn(&self, worker: &Arc<RoomWorker>, trigger: &RoomEvent, tier: u8) {
        let occasion = if tier == 2 {
            Occasion::Unaddressed
        } else {
            Occasion::Reply
        };
        let ctx = self.context(
            worker,
            trigger,
            occasion,
            String::new(),
            trigger.thread_root.as_deref(),
            self.participants(worker).await,
        );
        let answer = self.ask_for_a_message(&worker.room_id, &ctx).await;
        let Some(answer) = answer.filter(|text| !text.is_empty()) else {
            info!(
                "{}: no reply to {} (brain stayed quiet)",
                worker.room_id, trigger.event_id
            );
            worker
                .state
                .lock()
                .await
                .ledger
                .mark_consumed(&trigger.event_id);
            return;
        };
        // The marker is the brain's only channel back to the connector, and the
        // room must never see it.
        let (text, followup) = extract_followup(&answer);
        if text.is_empty() {
            info!(
                "{}: the brain's whole answer to {} was a followup marker; nothing to post",
                worker.room_id, trigger.event_id
            );
            worker
                .state
                .lock()
                .await
                .ledger
                .mark_consumed(&trigger.event_id);
            return;
        }
        self.post_reply(worker, trigger, &text, tier, &followup)
            .await;
    }

    /// Everything the brain is given for one question about this room.
    ///
    /// `note` is what the occasion is about when the room is not: the impulse
    /// that arrived, the loop I left open. `thread_root` overrides the
    /// trigger's own thread, which is what a follow-up needs - it belongs to the
    /// thread the loop was opened in, not to whatever was said last.
    pub(crate) fn context(
        &self,
        worker: &RoomWorker,
        trigger: &RoomEvent,
        occasion: Occasion,
        note: String,
        thread_root: Option<&str>,
        participants: usize,
    ) -> BrainContext {
        let thread = thread_root
            .map(|root| worker.transcript.thread(root))
            .unwrap_or_default();
        BrainContext {
            persona: self.persona.clone(),
            me: self.me.clone(),
            room_id: worker.room_id.to_string(),
            trigger: trigger.clone(),
            history: worker.transcript.recent(self.cfg.history_limit),
            thread,
            occasion,
            note,
            want_urgency: self.cfg.policy.inner_thoughts,
            speak_threshold: self.cfg.policy.effective_speak_threshold(),
            participants,
        }
    }

    /// Run the brain with the typing indicator up. Never panics on the brain.
    pub(crate) async fn ask_for_a_message(
        &self,
        room_id: &RoomId,
        ctx: &BrainContext,
    ) -> Option<String> {
        // Raise the indicator before the brain starts, so the room sees the
        // agent pick the message up even when the answer is instant.
        let room = self.client.get_room(room_id);
        if let Some(room) = &room {
            Self::set_typing(room, true).await;
        }
        let keep_typing = room.clone().map(|room| {
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(TYPING_REFRESH_S)).await;
                    Self::set_typing(&room, true).await;
                }
            })
        });
        let answer = self.brain.reply(ctx).await;
        if let Some(handle) = keep_typing {
            handle.abort();
        }
        if let Some(room) = &room {
            Self::set_typing(room, false).await;
        }
        answer
    }

    async fn set_typing(room: &Room, typing: bool) {
        // Typing is cosmetic; never let it break a turn.
        if let Err(exc) = room.typing_notice(typing).await {
            debug!("{}: typing={typing} failed: {exc}", room.room_id());
        }
    }

    /// Who this reply addresses: whoever I answer, plus anyone I named.
    ///
    /// A brain answers with text, so writing "@bot-b:server, what do you
    /// think?" is the only way one agent can address another - and without an
    /// `m.mentions` to match, every connector running `bot_to_bot: mentions`
    /// would ignore it.
    ///
    /// Anyone the TRIGGER already mentioned is left out, and that exclusion is
    /// not a detail: quoting a message must not re-ping the people it was
    /// already addressed to, or two agents answering one human sentence keep
    /// answering each other until a budget stops them. Naming somebody the
    /// message did NOT mention still reaches them, which is what makes "please
    /// say hello to @bot-b" work.
    #[must_use]
    pub fn reply_mentions(&self, trigger: &RoomEvent, text: &str) -> Vec<String> {
        let mut mentions = vec![trigger.sender.clone()];
        for user_id in mentioned_user_ids(text) {
            if user_id != self.me
                && !mentions.contains(&user_id)
                && !trigger.mentions.contains(&user_id)
            {
                mentions.push(user_id);
            }
        }
        mentions
    }

    async fn post_reply(
        &self,
        worker: &Arc<RoomWorker>,
        trigger: &RoomEvent,
        text: &str,
        tier: u8,
        followup: &str,
    ) {
        let room_id = &worker.room_id;
        let Some(room) = self.client.get_room(room_id) else {
            error!("{room_id}: cannot post: the client does not know the room");
            return;
        };
        let thread = trigger.thread_root_or_self().to_owned();
        let mentions = self.reply_mentions(trigger, text);
        // `thread_fallback`, never `reply_to`: this is a threaded message, and
        // the pointer at the trigger is the fallback clients without thread
        // support render. Marking it a real reply would make every other
        // connector read it as "you replied to me" (tier 1) instead of "a
        // message in a thread".
        let content = build_reply_content(
            text,
            NOTICE_MSGTYPE,
            &Relation {
                thread_root: Some(&thread),
                reply_to: None,
                thread_fallback: Some(&trigger.event_id),
            },
            &mentions,
        );
        let event_id = match room.send_raw("m.room.message", content).await {
            Ok(sent) => sent.response.event_id,
            Err(exc) => {
                error!(
                    "{room_id}: sending the reply to {} failed: {exc}",
                    trigger.event_id
                );
                return;
            }
        };
        info!(
            "{room_id}: replied (tier {tier}) to {} ({}) as {event_id} in thread {thread}",
            trigger.event_id, trigger.sender
        );
        self.mark_read(&room, trigger).await;
        let now = self.now();
        {
            let mut state = worker.state.lock().await;
            state
                .ledger
                .record_post(event_id.as_str(), &thread, &trigger.sender, Some(now), tier);
            state.ledger.mark_consumed(&trigger.event_id);
            self.note_open_loop(
                &mut state,
                room_id,
                event_id.as_str(),
                &thread,
                text,
                followup,
            );
            unprompted::reset_inner_urgency(&mut state, trigger.thread_root.as_deref());
            state.last_activity_ts = now;
        }
        worker.transcript.append_reply(&RoomEvent {
            event_id: event_id.to_string(),
            room_id: room_id.to_string(),
            sender: self.me.clone(),
            sender_display: None,
            body: text.to_owned(),
            formatted_body: None,
            msgtype: NOTICE_MSGTYPE.to_owned(),
            ts: now,
            thread_root: Some(thread),
            reply_to: Some(trigger.event_id.clone()),
            reply_is_fallback: true,
            mentions: mentions.into_iter().collect(),
            is_bot: true,
        });
    }

    /// Receipts are the room-visible "I consumed this". Best effort.
    async fn mark_read(&self, room: &Room, trigger: &RoomEvent) {
        let Ok(event_id) = EventId::parse(&trigger.event_id) else {
            return;
        };
        let receipt_thread = trigger
            .thread_root
            .as_deref()
            .and_then(|thread_root| EventId::parse(thread_root).ok())
            .map_or(ReceiptThread::Main, ReceiptThread::Thread);
        if let Err(exc) = room
            .send_single_receipt(
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                event_id.clone(),
            )
            .await
        {
            warn!(
                "{}: read marker for {event_id} failed: {exc}",
                room.room_id()
            );
        }
        if let Err(exc) = room
            .send_single_receipt(ReceiptType::Read, receipt_thread, event_id.clone())
            .await
        {
            debug!(
                "{}: threaded receipt for {event_id} failed: {exc}",
                room.room_id()
            );
        }
    }

    // -- tier 2: nobody addressed me --------------------------------------

    /// Whether this turn may answer without asking the judge.
    ///
    /// Four things at once, and every one of them is load-bearing:
    ///
    /// - the operator left `policy.room_invitations` on;
    /// - the verdict is `consider`, so the back-off and the STAND-DOWN re-read
    ///   have both run - which is what keeps two agents from both answering;
    /// - a PERSON wrote it. Agents inviting each other to speak is a loop with
    ///   no human in it, and a bot's line keeps the judge in every mode;
    /// - and the line handed the turn to the room and asked it something
    ///   ([`invites_an_answer`]).
    ///
    /// It is the same rule tier 1 already runs on a typed name, applied to the
    /// other way people hand over the turn: a name selects one speaker, the
    /// open floor selects whoever gets there first.
    fn room_invitation(&self, decision: &Decision, ev: &RoomEvent) -> bool {
        self.cfg.policy.room_invitations
            && decision.verdict == Verdict::Consider
            && !ev.is_bot
            && invites_an_answer(&ev.body)
    }

    /// Back off (tier 2 only), re-read the room, ask the judge, maybe speak.
    ///
    /// The back-off is awaited here, in a task of its own, so the sync loop
    /// keeps delivering events for this room and every other room while it
    /// runs. That is the whole reason the re-read afterwards can find that
    /// somebody else already answered.
    pub(crate) async fn deliberate(
        self,
        worker: Arc<RoomWorker>,
        ev: RoomEvent,
        decision: Decision,
    ) {
        let started = self.now();
        let room_id = worker.room_id.clone();
        if decision.verdict == Verdict::Consider {
            let delay = {
                let state = worker.state.lock().await;
                self.tier2_delay(&state, decision.prescore.score)
            };
            info!(
                "{room_id}: tier 2 on {}: waiting {delay:.1} s before re-reading the room",
                ev.event_id
            );
            // A back-off is the one moment we know a turn may be coming and
            // have time to prepare for it.
            self.brain
                .warm(&format!("tier-2 back-off started in {room_id}"))
                .await;
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            if let Some(reason) = self.stand_down_reason(&worker, &ev, started).await {
                info!("{room_id}: standing down on {}: {reason}", ev.event_id);
                let mut state = worker.state.lock().await;
                state.ledger.mark_consumed(&ev.event_id);
                state.deliberating = false;
                return;
            }
        }

        // A line the room was handed AND asked something on is not a borderline
        // case for a judge to weigh: the turn was allocated to whoever wants
        // it, and I am the one still standing here after the back-off and the
        // re-read. Asking anyway is what put "it's a general opinion question
        // not directed at me" between a person and an answer.
        let speak = if self.room_invitation(&decision, &ev) {
            info!(
                "{room_id}: room invitation on {}: answering without the judge (pre-score {}: {})",
                ev.event_id,
                decision.prescore.score,
                decision.prescore.listed()
            );
            true
        } else {
            let ctx = self.context(
                &worker,
                &ev,
                Occasion::Unaddressed,
                String::new(),
                ev.thread_root.as_deref(),
                self.participants(&worker).await,
            );
            let judgement = self.brain.judge(&ctx).await;
            info!(
                "{room_id}: judge on {} says {}: {}",
                ev.event_id,
                judgement.says(ctx.speak_threshold),
                judgement.why
            );
            self.note_urgency(&worker, &ev, judgement.urgency, &judgement.why)
                .await;
            judgement.speak
        };

        let go = {
            let mut state = worker.state.lock().await;
            if !speak {
                state.ledger.mark_consumed(&ev.event_id);
                state.deliberating = false;
                false
            } else if let Some(refusal) = Self::unprompted_budget_refusal(&state) {
                info!("{room_id}: judged yes but {refusal}");
                state.ledger.mark_consumed(&ev.event_id);
                state.deliberating = false;
                false
            } else if state.busy {
                info!(
                    "{room_id}: judged yes on {} but a turn is already running; standing down",
                    ev.event_id
                );
                state.ledger.mark_consumed(&ev.event_id);
                state.deliberating = false;
                false
            } else {
                state.busy = true;
                true
            }
        };
        if go {
            let tier = u8::from(decision.verdict == Verdict::Consider) + 1;
            self.clone().drain(Arc::clone(&worker), ev, tier).await;
            worker.state.lock().await.deliberating = false;
        }
    }

    /// Why this tier-2 attempt should be dropped, or None to carry on.
    ///
    /// Three ways somebody else has already covered it:
    ///
    /// 1. a turn of my own is running or queued - I was addressed meanwhile,
    ///    and tier 1 is handling it;
    /// 2. I have posted since the back-off started, anywhere in the room;
    /// 3. anyone has posted in the trigger's thread since the trigger.
    ///
    /// (3) is checked against `/messages` as well as the local transcript. The
    /// sync loop is a long poll and the whole question is what the room looked
    /// like a fraction of a second ago, so the authoritative read is worth one
    /// HTTP round-trip - and `/messages` is not behind the sync cache that gate
    /// G4 was written for.
    pub(crate) async fn stand_down_reason(
        &self,
        worker: &Arc<RoomWorker>,
        ev: &RoomEvent,
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
        }
        let root = ev.thread_root_or_self();
        if Self::later_in_thread(&worker.transcript.thread(root), ev) {
            return Some("someone answered first (seen in the transcript)".to_owned());
        }
        if Self::later_in_thread(&self.room_tail(&worker.room_id).await, ev) {
            return Some("someone answered first (seen in a fresh read of the room)".to_owned());
        }
        None
    }

    fn later_in_thread(events: &[RoomEvent], trigger: &RoomEvent) -> bool {
        let root = trigger.thread_root_or_self();
        events.iter().any(|other| {
            other.event_id != trigger.event_id
                && other.ts >= trigger.ts
                && other.thread_root_or_self() == root
        })
    }

    /// The room's newest messages straight from `/messages` (never cached).
    pub(crate) async fn room_tail(&self, room_id: &RoomId) -> Vec<RoomEvent> {
        let Some(room) = self.client.get_room(room_id) else {
            warn!("{room_id}: could not re-read the room: the client does not know it");
            return Vec::new();
        };
        let mut options = MessagesOptions::backward();
        options.limit = UInt::from(STANDDOWN_TAIL);
        let chunk = match room.messages(options).await {
            Ok(messages) => messages.chunk,
            Err(exc) => {
                // A failed re-read must not become a reply.
                warn!("{room_id}: could not re-read the room: {exc}");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(chunk.len());
        for raw in &chunk {
            if let Some(ev) = normalise_event(
                Some(&room),
                room_id,
                raw.raw().json().get(),
                rules_from(&self.bot_user_ids, &self.bot_patterns),
            )
            .await
            {
                out.push(ev);
            }
        }
        out
    }

    /// Re-check the budgets that time can have eaten while I deliberated.
    pub(crate) fn unprompted_budget_refusal(state: &WorkerState) -> Option<String> {
        let now = state.ledger.now();
        for check in [
            state.ledger.hour_allows(now),
            state.ledger.tier2_hour_allows(now),
        ] {
            if !check.allowed {
                return Some(check.reason);
            }
        }
        None
    }

    // -- open loops --------------------------------------------------------

    /// Remember a question I left hanging, or a follow-up I promised.
    fn note_open_loop(
        &self,
        state: &mut WorkerState,
        room_id: &RoomId,
        event_id: &str,
        thread_root: &str,
        text: &str,
        followup: &str,
    ) {
        if followup.is_empty() && !is_open_question(text) {
            return;
        }
        let now = self.now();
        let (low, high) = self.cfg.policy.followup_delay_s;
        let due = now + uniform(low, high);
        let text = loop_text(text, followup);
        state.ledger.open_loop(event_id, thread_root, &text, due);
        info!(
            "{room_id}: left a loop open, due in {:.0} s: {}",
            due - now,
            head(&text, 120)
        );
    }

    // -- the unprompted paths ---------------------------------------------

    /// Post an `m.notice` nobody asked for: no mentions, and no reply pointer.
    ///
    /// Threaded only when the candidate belongs to a thread (a follow-up does,
    /// an impulse does not). Mentions nobody either way: this is somebody
    /// saying something out loud in a room, not a ping.
    pub(crate) async fn post_unprompted(
        &self,
        worker: &Arc<RoomWorker>,
        candidate: &Candidate,
        text: &str,
        followup: &str,
    ) {
        let room_id = &worker.room_id;
        let Some(room) = self.client.get_room(room_id) else {
            error!("{room_id}: cannot post: the client does not know the room");
            return;
        };
        let content = build_reply_content(
            text,
            NOTICE_MSGTYPE,
            &Relation {
                thread_root: candidate.thread_root.as_deref(),
                reply_to: None,
                thread_fallback: candidate.thread_root.as_deref(),
            },
            &[],
        );
        let event_id = match room.send_raw("m.room.message", content).await {
            Ok(sent) => sent.response.event_id,
            Err(exc) => {
                error!(
                    "{room_id}: sending the {} failed: {exc}",
                    candidate.kind.as_str()
                );
                let mut state = worker.state.lock().await;
                unprompted::close_candidate(&mut state, candidate, "the homeserver refused it");
                return;
            }
        };
        let thread = candidate
            .thread_root
            .clone()
            .unwrap_or_else(|| event_id.to_string());
        info!(
            "{room_id}: {} posted as {event_id} in thread {thread}",
            candidate.kind.as_str()
        );
        let now = self.now();
        {
            let mut state = worker.state.lock().await;
            state
                .ledger
                .record_post(event_id.as_str(), &thread, "", Some(now), 3);
            unprompted::close_candidate(&mut state, candidate, "followed up");
            // A follow-up never opens a loop of its own: an agent that follows
            // up on its follow-up is a cron job with a persona.
            if candidate.kind != Occasion::Followup {
                self.note_open_loop(
                    &mut state,
                    room_id,
                    event_id.as_str(),
                    &thread,
                    text,
                    followup,
                );
            }
            unprompted::reset_inner_urgency(&mut state, candidate.thread_root.as_deref());
            state.last_activity_ts = now;
        }
        worker.transcript.append_reply(&RoomEvent {
            event_id: event_id.to_string(),
            room_id: room_id.to_string(),
            sender: self.me.clone(),
            sender_display: None,
            body: text.to_owned(),
            formatted_body: None,
            msgtype: NOTICE_MSGTYPE.to_owned(),
            ts: now,
            thread_root: candidate.thread_root.clone(),
            reply_to: None,
            reply_is_fallback: false,
            mentions: std::collections::BTreeSet::new(),
            is_bot: true,
        });
    }
}

/// A number drawn from `[low, high]`, and `low` when there is no range at all.
///
/// `rand` panics on an empty range, and a configuration of `[30, 30]` is a
/// perfectly reasonable "always thirty seconds".
#[must_use]
pub fn uniform(low: f64, high: f64) -> f64 {
    if high <= low {
        return low.max(0.0);
    }
    rand::random_range(low.max(0.0)..high)
}

#[cfg(test)]
mod tests {
    //! The one decision in this file that is not about posting: whether a
    //! tier-2 turn asks the judge at all.
    //!
    //! Driven through the real [`Runner::deliberate`] with a brain that counts
    //! what it was asked, because the assertion is a JOURNEY - "the room got an
    //! answer and nobody paid a judge for it" - and a test of the predicate
    //! alone would pass just as happily with the branch wired to nothing. The
    //! client is a real one pointed at a port nothing listens on: everything
    //! that posts fails and says so, which is exactly the part a live gate
    //! owns, and every brain call still happens.

    use super::*;
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::brain::Judgement;
    use crate::config::PolicyConfig;
    use crate::connector::testkit::{self, FakeClock};
    use crate::events::{RoomEvent, from_source};
    use crate::policy::{Cues, Decision, should_reply};
    use crate::transcript::Transcript;

    const ME: &str = "@bot-a:example.com";
    const HUMAN: &str = "@human:example.com";
    /// The line from the room log this whole path exists for: it selects
    /// nobody, hands the turn to the room, and asks it something.
    const ROOM_QUESTION: &str =
        "So, anyone here got an opinion on whether weekends should be three days long?";

    /// A brain that says one thing and counts everything it was asked.
    ///
    /// Its judge REFUSES, so a turn that reaches it cannot post: "reply once"
    /// and "judge never" are then two readings of the same run rather than two
    /// hopes about it.
    struct CountingBrain {
        judged: AtomicUsize,
        replied: AtomicUsize,
    }

    impl CountingBrain {
        fn new() -> Self {
            Self {
                judged: AtomicUsize::new(0),
                replied: AtomicUsize::new(0),
            }
        }
        fn judgements(&self) -> usize {
            self.judged.load(Ordering::SeqCst)
        }
        fn replies(&self) -> usize {
            self.replied.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Brain for CountingBrain {
        async fn reply(&self, _ctx: &BrainContext) -> Option<String> {
            self.replied.fetch_add(1, Ordering::SeqCst);
            Some("three days sounds right to me".to_owned())
        }
        async fn judge(&self, _ctx: &BrainContext) -> Judgement {
            self.judged.fetch_add(1, Ordering::SeqCst);
            Judgement::no("this judge refuses everything")
        }
    }

    fn config(policy: PolicyConfig, state_dir: &Path) -> Config {
        Config {
            homeserver: "http://127.0.0.1:1".to_owned(),
            user_id: ME.to_owned(),
            access_token_file: None,
            password: None,
            rooms: vec![testkit::ROOM_ID.to_owned()],
            persona_file: None,
            state_dir: state_dir.to_path_buf(),
            brain: None,
            policy,
            mcp: crate::config::McpConfig::default(),
            tls: crate::config::TlsConfig::default(),
            history_limit: crate::config::default_history_limit(),
            transcript_keep: crate::transcript::DEFAULT_KEEP,
            transcript_archives: crate::transcript::DEFAULT_ARCHIVES,
            allow_wedged_device: false,
        }
    }

    /// One line in the room, from a person or from another agent.
    fn event(body: &str, sender: &str, is_bot: bool) -> RoomEvent {
        let msgtype = if is_bot { NOTICE_MSGTYPE } else { "m.text" };
        from_source(
            &serde_json::json!({
                "type": "m.room.message",
                "event_id": "$trigger",
                "sender": sender,
                "origin_server_ts": 1_700_000_000_000_u64,
                "room_id": testkit::ROOM_ID,
                "content": { "msgtype": msgtype, "body": body },
            }),
            testkit::ROOM_ID,
            None,
            rules_from(&[], &[]),
        )
    }

    /// Run one tier-2 turn on `ev` and report what the brain was asked.
    ///
    /// The verdict is the PRODUCT's, out of `should_reply`, so a change that
    /// stopped these lines reaching tier 2 at all would break this test rather
    /// than sail past it on a hand-written decision.
    async fn deliberate_on(policy: PolicyConfig, ev: &RoomEvent) -> (Decision, Arc<CountingBrain>) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let clock = FakeClock::new();
        let state = testkit::state(dir.path(), &clock);
        let cfg = Arc::new(config(policy, dir.path()));
        let decision = should_reply(
            ev,
            ME,
            &state.ledger,
            &HashSet::new(),
            &cfg.policy,
            &Cues::default(),
        );
        let brain = Arc::new(CountingBrain::new());
        let client = Client::builder()
            .homeserver_url(&cfg.homeserver)
            .build()
            .await
            .expect("a client that has never spoken to a homeserver");
        let runner = Runner {
            cfg,
            client,
            brain: Arc::clone(&brain) as Arc<dyn Brain>,
            me: ME.to_owned(),
            persona: String::new(),
            clock: clock.as_clock(),
            presence: Arc::new(Mutex::new(PresenceBook::new())),
            bot_user_ids: Vec::new(),
            bot_patterns: Vec::new(),
        };
        let worker = Arc::new(RoomWorker {
            room_id: RoomId::parse(testkit::ROOM_ID).expect("a room id"),
            transcript: Transcript::new(dir.path().join("room.jsonl")),
            state: Mutex::new(state),
        });
        runner
            .deliberate(Arc::clone(&worker), ev.clone(), decision.clone())
            .await;
        (decision, brain)
    }

    /// `conversational`, so a bot's unaddressed line reaches tier 2 at all -
    /// which is the only way "a bot still gets the judge" can be measured.
    fn conversational() -> PolicyConfig {
        PolicyConfig {
            bot_to_bot: crate::config::BotToBot::Conversational,
            ..PolicyConfig::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_room_question_from_a_human_is_answered_without_the_judge() {
        let (decision, brain) =
            deliberate_on(PolicyConfig::default(), &event(ROOM_QUESTION, HUMAN, false)).await;
        assert_eq!(decision.verdict, Verdict::Consider, "{}", decision.reason);
        assert_eq!(
            brain.judgements(),
            0,
            "the judge was asked about a line the room threw open: {}",
            decision.reason
        );
        assert_eq!(
            brain.replies(),
            1,
            "nobody answered the room's own question (this judge refuses everything)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_room_question_from_a_bot_still_goes_through_the_judge() {
        // Two agents inviting each other to speak is a loop with no human in
        // it. The line is word for word the one above.
        let (decision, brain) = deliberate_on(
            conversational(),
            &event(ROOM_QUESTION, "@bot-b:example.com", true),
        )
        .await;
        assert_eq!(decision.verdict, Verdict::Consider, "{}", decision.reason);
        assert_eq!(brain.judgements(), 1, "a bot's line skipped the judge");
        assert_eq!(brain.replies(), 0, "and the refusal did not stop it");
    }

    #[tokio::test(start_paused = true)]
    async fn room_invitations_false_restores_the_judge_path() {
        let off = PolicyConfig {
            room_invitations: false,
            ..PolicyConfig::default()
        };
        let (_, brain) = deliberate_on(off, &event(ROOM_QUESTION, HUMAN, false)).await;
        assert_eq!(brain.judgements(), 1, "the knob did not put the judge back");
        assert_eq!(brain.replies(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_unaddressed_line_still_needs_the_judge() {
        // G7's own line, offline: unaddressed, nobody waiting, judge says no,
        // the room hears nothing. Everything this slice does has to leave that
        // exactly as it was.
        let (decision, brain) = deliberate_on(
            PolicyConfig::default(),
            &event("just thinking aloud about the weather", HUMAN, false),
        )
        .await;
        assert_eq!(decision.verdict, Verdict::Consider, "{}", decision.reason);
        assert_eq!(
            brain.judgements(),
            1,
            "a line nobody was waiting on skipped the judge"
        );
        assert_eq!(brain.replies(), 0, "and it was answered anyway");
    }
}
