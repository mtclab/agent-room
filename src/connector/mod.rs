//! The connector: one Matrix account, one process, N rooms.
//!
//! Push, not poll: a long-lived `/sync` loop wakes the process, the policy
//! decides whether to speak, the ledger decides whether it is allowed to, and
//! the brain decides what to say. Everything the connector learns is written to
//! disk, so a kill -9 costs nothing and a restart never answers old traffic.
//!
//! Ways a message gets posted, and they are deliberately different:
//!
//! - **tier 1** - somebody addressed me: answer as soon as the brain has an
//!   answer.
//! - **tier 2** - nobody addressed me ([`turn::Runner::deliberate`]): draw a
//!   random back-off, keep receiving events while it runs, then re-read the room
//!   and stand down if anyone covered it; only then ask the cheap judge, and
//!   only then speak.
//! - **unprompted** ([`turn::Runner::speak_unprompted`]) - nothing in the room
//!   made me speak at all. Three reasons, one path: an IMPULSE (something
//!   happened to me, dropped in the inlet directory), an OPEN LOOP (a question I
//!   asked that nobody answered, or a `[[followup: ...]]` the brain promised), or
//!   an INNER THOUGHT (the judge kept saying "no, but I do want to say
//!   something" until it added up). All of them wait for a human to be present,
//!   take the same back-off, and spend the same `tier2_per_hour_max` budget.
//! - **the heartbeat** ([`turn::Runner::heartbeat_once`]) - the same thing on a
//!   timer. Off unless `policy.heartbeat_minutes` is set, and a timer is the
//!   least organic reason there is: it is kept as a fallback, not as the story.

pub mod turn;
pub mod unprompted;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UInt, UserId};
use matrix_sdk::sync::{JoinedRoomUpdate, State};
use matrix_sdk::{Client, Room, RoomMemberships};
use regex::Regex;
use serde_json::Value;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::addressing::{Names, names_for};
use crate::brain::Brain;
use crate::config::{BotToBot, Config, PolicyConfig};
use crate::events::{BotRules, RoomEvent, from_source, is_bot_user, localpart};
use crate::ledger::{Clock, Ledger};
use crate::matrix;
use crate::policy::{Cues, Decision, LastSpeaker, Verdict, should_reply};
use crate::presence::PresenceBook;
use crate::testing::maybe_start_spam;
use crate::transcript::Transcript;

pub use turn::Runner;
pub use unprompted::{Candidate, MAX_QUEUED, UNPROMPTED_POLL_S, hazard_factor};

/// How long the homeserver may hold a long-poll open.
const SYNC_TIMEOUT_S: u64 = 30;
/// How long to let in-flight brain turns finish on shutdown.
const SHUTDOWN_GRACE_S: u64 = 10;
/// How many events the startup `/messages` snapshot reads per room.
const BACKLOG_SNAPSHOT: u32 = 100;
/// How far back the follow-up arm looks for who spoke last. Eight, because the
/// answer is the NEWEST line that is not the one that just arrived: any more is
/// read and thrown away, and any fewer would miss a conversation whose last few
/// lines all came from one burst.
pub(crate) const LAST_SPEAKER_TAIL: usize = 8;

/// One room's mutable state. Everything that decides whether to speak is behind
/// this lock, so two events arriving at once cannot both find the room idle.
pub struct WorkerState {
    pub ledger: Ledger,
    /// True while a brain turn is running for this room.
    pub busy: bool,
    /// Events that arrived mid-turn, to be coalesced into the next one.
    pub pending: Vec<RoomEvent>,
    /// Newest homeserver timestamp that already existed at startup. Anything
    /// older than this is backlog, whichever sync finally delivers it.
    pub backlog_cutoff_ts: f64,
    /// True while a deliberation (tier 2, a judged mention, or an unprompted
    /// candidate) is sleeping off its back-off or asking the judge. ONE at a
    /// time per room: without it a burst of chat would arm a judge call per
    /// message and every one of them costs money to reach the same answer about
    /// one room.
    pub deliberating: bool,
    /// True while an inner-thoughts probe is asking the judge. ONE at a time per
    /// room, for the same reason `deliberating` exists.
    pub probing: bool,
    /// Local clock of the last thing that happened here (a message seen or a
    /// message posted). Local, not the homeserver's, because it only ever
    /// answers "how long have I been sitting here quietly?".
    pub last_activity_ts: f64,
    /// Homeserver timestamp of the newest message from a human. The other half
    /// of "is anybody here", and the reason the presence window survives a
    /// restart: it is read off the room, not off a local clock.
    pub last_human_post_ts: f64,
    /// What everybody in this room is called, compiled. Rebuilt from the
    /// member store at startup and whenever somebody joins, leaves or renames -
    /// never per message, because these are compiled regexes.
    pub names: Names,
    /// How many people and agents are joined here, read from the member store
    /// beside the names and rebuilt with them. 0 until the first sync has
    /// filled the store, which is the "I do not know" the back-off treats as a
    /// full-sized room.
    pub participants: usize,
    /// Unprompted candidates waiting for somebody to be around.
    pub queue: Vec<Candidate>,
    pub queued: HashSet<String>,
    /// Accumulated urgency per conversation (thread root, or the main
    /// timeline), and when that conversation was last spoken in. In memory
    /// only: a restart forgetting a half-formed thought is right, and
    /// persisting one would mean an agent that comes back from a crash with
    /// something to say about a conversation that has moved on.
    pub inner_urgency: HashMap<String, i64>,
    pub inner_seen_ts: HashMap<String, f64>,
}

impl WorkerState {
    fn new(ledger: Ledger, now: f64) -> Self {
        Self {
            ledger,
            busy: false,
            pending: Vec::new(),
            backlog_cutoff_ts: 0.0,
            deliberating: false,
            probing: false,
            last_activity_ts: now,
            last_human_post_ts: 0.0,
            names: Names::empty(),
            participants: 0,
            queue: Vec::new(),
            queued: HashSet::new(),
            inner_urgency: HashMap::new(),
            inner_seen_ts: HashMap::new(),
        }
    }
}

/// Serialises brain turns for one room and coalesces what arrives mid-run.
pub struct RoomWorker {
    pub room_id: OwnedRoomId,
    pub transcript: Transcript,
    pub state: Mutex<WorkerState>,
}

/// One account's presence in the configured rooms.
pub struct Connector {
    cfg: Arc<Config>,
    client: Client,
    brain: Arc<dyn Brain>,
    me: String,
    persona: String,
    /// My display name on the account, read once at login. The per-room member
    /// display name is better and is preferred; this is the fallback for a room
    /// whose member list has not arrived yet.
    account_display: Option<String>,
    bot_user_ids: Vec<String>,
    bot_patterns: Vec<Regex>,
    workers: HashMap<OwnedRoomId, Arc<RoomWorker>>,
    /// Who the homeserver says is around. Fed by `m.presence` in every sync.
    presence: Arc<Mutex<PresenceBook>>,
    /// False until the startup backlog sweep is done: nothing seen before that
    /// may ever reach the policy or the brain.
    live: Arc<AtomicBool>,
    clock: Clock,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The per-room clocks (the unprompted poll, and maybe the heartbeat).
    room_loops: Mutex<Vec<JoinHandle<()>>>,
}

impl Connector {
    /// Build a connector for `cfg`, with the client and brain already made.
    ///
    /// # Errors
    /// When a bot-localpart pattern does not compile or the persona file cannot
    /// be read - both of which `load_config` has already checked, so this is
    /// belt and braces rather than a second gate.
    pub fn new(
        cfg: Arc<Config>,
        client: Client,
        brain: Arc<dyn Brain>,
        clock: Clock,
    ) -> Result<Self> {
        let persona = cfg.read_persona().map_err(|exc| anyhow!("{exc}"))?;
        let bot_patterns = cfg
            .policy
            .compiled_bot_patterns()
            .map_err(|exc| anyhow!("{exc}"))?;
        let now = clock();
        let mut workers = HashMap::new();
        for room_id in &cfg.rooms {
            let parsed = RoomId::parse(room_id)
                .map_err(|exc| anyhow!("rooms: {room_id} is not a room id: {exc}"))?;
            let ledger = Ledger::load(
                &cfg.room_state_path(room_id, ".ledger.json"),
                cfg.policy.budgets.clone(),
                Arc::clone(&clock),
            );
            workers.insert(
                parsed.clone(),
                Arc::new(RoomWorker {
                    room_id: parsed,
                    transcript: Transcript::with_rotation(
                        cfg.room_state_path(room_id, ".jsonl"),
                        cfg.transcript_keep,
                        cfg.transcript_archives,
                    ),
                    state: Mutex::new(WorkerState::new(ledger, now)),
                }),
            );
        }
        Ok(Self {
            me: cfg.user_id.clone(),
            cfg,
            client,
            brain,
            persona,
            account_display: None,
            bot_user_ids: Vec::new(),
            bot_patterns,
            workers,
            presence: Arc::new(Mutex::new(PresenceBook::new())),
            live: Arc::new(AtomicBool::new(false)),
            clock,
            tasks: Mutex::new(Vec::new()),
            room_loops: Mutex::new(Vec::new()),
        })
    }

    fn rules(&self) -> BotRules<'_> {
        BotRules {
            bot_user_ids: &self.bot_user_ids,
            bot_localpart_patterns: &self.bot_patterns,
        }
    }

    // -- lifecycle -------------------------------------------------------

    /// Authenticate, join, swallow the backlog, then sync until `stop` fires.
    ///
    /// # Errors
    /// When authentication fails, no configured room can be joined, or the sync
    /// loop gives up on the homeserver.
    pub async fn run(
        mut self,
        http: reqwest::Client,
        mut stop: watch::Receiver<bool>,
    ) -> Result<()> {
        self.me = matrix::authenticate(&self.client, &self.cfg, &http).await?;
        info!(
            "authenticated as {} (device {})",
            self.me,
            self.client
                .device_id()
                .map_or_else(|| "?".to_owned(), ToString::to_string)
        );
        // The SDK reports a duplicate one-time-key upload exactly once per
        // store. It means this store is not the one that published the
        // device's keys - stop, instead of failing every sync for ever.
        let mut wedge = self.client.subscribe_to_duplicate_key_upload_errors();
        self.bot_user_ids = self.cfg.policy.bot_user_ids.clone();
        let joined = matrix::join_rooms(&self.client, &self.cfg.rooms).await;
        if joined.is_empty() {
            bail!("could not join any configured room");
        }
        info!(
            "joined {} of {} configured rooms",
            joined.len(),
            self.cfg.rooms.len()
        );
        matrix::bootstrap_encryption(&self.client, &self.cfg).await;
        info!("encryption ready");
        // One request, at startup, for the name I am known by everywhere. The
        // per-room display name is better and comes free with the sync below;
        // this is what answers when a room has not told us one.
        self.account_display = self
            .client
            .account()
            .get_display_name()
            .await
            .unwrap_or_else(|exc| {
                warn!("could not read my own display name ({exc}); using my localpart");
                None
            })
            .filter(|name| !name.trim().is_empty());
        self.consume_backlog().await?;
        for room_id in self.workers.keys() {
            self.refresh_names(room_id).await;
        }
        let spam = maybe_start_spam(&self.client, &self.cfg.rooms);
        self.start_room_loops(&stop).await;
        info!(
            "connector {} watching {}",
            self.me,
            self.cfg.rooms.join(", ")
        );

        loop {
            let settings = SyncSettings::new().timeout(Duration::from_secs(SYNC_TIMEOUT_S));
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                dup = wedge.recv() => {
                    if dup.is_ok() && !self.cfg.allow_wedged_device {
                        self.shutdown().await;
                        return Err(matrix::DeviceWedged {
                            device_id: self
                                .client
                                .device_id()
                                .map_or_else(|| "?".to_owned(), ToString::to_string),
                        }
                        .into());
                    }
                }
                response = self.client.sync_once(settings) => match response {
                    Ok(response) => self.handle_sync(&response).await,
                    Err(exc) => {
                        // A homeserver that blinks is not a reason to give up
                        // the room; a homeserver that is gone is the operator's
                        // problem and says so in the log every time it retries.
                        warn!("sync failed ({exc}); retrying");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                },
            }
        }
        if let Some(spam) = spam {
            spam.abort();
        }
        self.shutdown().await;
        Ok(())
    }

    /// The per-room clocks: the unprompted poll, and maybe the heartbeat.
    ///
    /// The unprompted loop always runs - it is what notices an impulse, a due
    /// follow-up and a human turning up, none of which any sync will tell us
    /// about. The heartbeat only runs when somebody asked for a timer.
    async fn start_room_loops(&self, stop: &watch::Receiver<bool>) {
        let minutes = self.cfg.policy.heartbeat_minutes;
        if minutes > 0 {
            info!("heartbeat on: every {minutes} min in a room quiet that long");
        }
        let mut loops = self.room_loops.lock().await;
        for worker in self.workers.values() {
            let runner = self.runner();
            let polled = Arc::clone(worker);
            let mut stop_rx = stop.clone();
            loops.push(tokio::spawn(async move {
                runner.unprompted_loop(polled, &mut stop_rx).await;
            }));
            if minutes > 0 {
                #[allow(clippy::cast_precision_loss)]
                let period_s = minutes as f64 * 60.0;
                let runner = self.runner();
                let worker = Arc::clone(worker);
                let mut stop_rx = stop.clone();
                loops.push(tokio::spawn(async move {
                    runner.heartbeat_loop(worker, period_s, &mut stop_rx).await;
                }));
            }
        }
    }

    /// Let in-flight turns finish, then flush every room's state.
    async fn shutdown(&self) {
        // The room loops watch the same stop signal, so they are already on
        // their way out; give them the same grace as a turn rather than
        // aborting one in the middle of posting, and only then cut them off.
        let grace = Duration::from_secs(SHUTDOWN_GRACE_S);
        for handle in std::mem::take(&mut *self.room_loops.lock().await) {
            if tokio::time::timeout(grace, &mut { handle }).await.is_err() {
                warn!("a room loop did not stop in time");
            }
        }
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock().await);
        let running: Vec<JoinHandle<()>> =
            handles.into_iter().filter(|h| !h.is_finished()).collect();
        if !running.is_empty() {
            info!(
                "waiting up to {SHUTDOWN_GRACE_S} s for {} in-flight turns",
                running.len()
            );
            let grace = Duration::from_secs(SHUTDOWN_GRACE_S);
            for handle in running {
                match tokio::time::timeout(grace, handle).await {
                    Ok(Ok(())) => {}
                    Ok(Err(exc)) => warn!("an in-flight turn ended badly: {exc}"),
                    Err(_elapsed) => warn!("an in-flight turn did not finish in time"),
                }
            }
        }
        self.brain.close().await;
        for worker in self.workers.values() {
            worker.state.lock().await.ledger.save();
        }
        info!("connector {} stopped", self.me);
    }

    async fn track(&self, handle: JoinHandle<()>) {
        let mut tasks = self.tasks.lock().await;
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
    }

    // -- the startup sweep -----------------------------------------------

    /// Swallow everything the room already holds, then go live.
    ///
    /// A restart must never answer old traffic, and one `/sync` cannot tell us
    /// what old traffic is: Synapse caches sync responses per (user, device,
    /// since, timeout), so a connector that restarts inside the cache window is
    /// handed the previous process's responses - stale tokens, empty timelines
    /// - while the traffic it missed only appears several syncs later.
    ///
    /// `/messages` is not behind that cache, so the snapshot is authoritative:
    ///
    /// 1. one sync to establish the token `sync_once` will carry on from,
    ///    consumed;
    /// 2. `/messages` per room (newest first, no `from`), all consumed;
    /// 3. the newest timestamp in that snapshot becomes the room's backlog
    ///    cutoff, so a deep backlog arriving later in a gappy sync is still
    ///    recognised as old - both timestamps come from the homeserver, so no
    ///    local clock is involved.
    ///
    /// Presence is NOT backlog and is recorded from this sync as well: the
    /// first response carries the current state of everyone we share a room
    /// with, which is exactly what a connector that has just started needs to
    /// know before it considers speaking.
    async fn consume_backlog(&self) -> Result<()> {
        let response = self
            .client
            .sync_once(SyncSettings::new().timeout(Duration::from_secs(0)))
            .await?;
        self.record_presence(&response).await;
        let mut total = self.record_backlog(&response).await;
        for room_id in self.workers.keys() {
            total += self.snapshot_room(room_id).await;
        }
        self.live.store(true, Ordering::SeqCst);
        info!("backlog swallowed: {total} events consumed without replying");
        Ok(())
    }

    /// Record one sync response as backlog: transcript yes, policy never.
    async fn record_backlog(&self, response: &matrix_sdk::sync::SyncResponse) -> usize {
        let mut total = 0;
        for (room_id, update) in &response.rooms.joined {
            let Some(worker) = self.workers.get(room_id) else {
                continue;
            };
            let room = self.client.get_room(room_id);
            let mut fresh: Vec<String> = Vec::new();
            let mut state = worker.state.lock().await;
            for raw in &update.timeline.events {
                let Some(ev) = self
                    .normalise(room.as_ref(), room_id, raw.raw().json().get())
                    .await
                else {
                    continue;
                };
                if state.ledger.is_consumed(&ev.event_id) {
                    continue;
                }
                worker.transcript.append_seen(&ev);
                fresh.push(ev.event_id.clone());
            }
            let count = fresh.len();
            if count > 0 {
                state.ledger.mark_many_consumed(&fresh);
                total += count;
            }
            info!("{room_id}: backlog sweep recorded {count} events as consumed (no replies)");
        }
        total
    }

    /// Consume what `/messages` says the room already contains.
    async fn snapshot_room(&self, room_id: &RoomId) -> usize {
        let Some(worker) = self.workers.get(room_id) else {
            return 0;
        };
        let Some(room) = self.client.get_room(room_id) else {
            error!("{room_id}: backlog snapshot failed: the client does not know the room");
            return 0;
        };
        let mut options = MessagesOptions::backward();
        options.limit = UInt::from(BACKLOG_SNAPSHOT);
        let chunk = match room.messages(options).await {
            Ok(messages) => messages.chunk,
            Err(exc) => {
                error!("{room_id}: backlog snapshot failed: {exc}");
                return 0;
            }
        };
        let seen = chunk.len();
        let mut state = worker.state.lock().await;
        let mut fresh: Vec<String> = Vec::new();
        let mut newest = state.backlog_cutoff_ts;
        // `/messages` returns newest first.
        for raw in chunk.iter().rev() {
            let Some(ev) = self
                .normalise(Some(&room), room_id, raw.raw().json().get())
                .await
            else {
                continue;
            };
            newest = newest.max(ev.ts);
            if !ev.is_bot && ev.sender != self.me {
                // Backlog is never answered, but it does say when a human was
                // last here - which is half of "is anybody around" and would
                // otherwise be forgotten by every restart.
                state.last_human_post_ts = state.last_human_post_ts.max(ev.ts);
            }
            if state.ledger.is_consumed(&ev.event_id) {
                continue;
            }
            worker.transcript.append_seen(&ev);
            fresh.push(ev.event_id.clone());
        }
        let count = fresh.len();
        if count > 0 {
            state.ledger.mark_many_consumed(&fresh);
        }
        state.backlog_cutoff_ts = newest;
        info!(
            "{room_id}: backlog snapshot consumed {count} new of {seen} events (cutoff ts {newest:.3})"
        );
        count
    }

    // -- event handling --------------------------------------------------

    /// One sync response: presence, the messages, and the typing notices.
    async fn handle_sync(&self, response: &matrix_sdk::sync::SyncResponse) {
        self.record_presence(response).await;
        for (room_id, update) in &response.rooms.joined {
            if !self.workers.contains_key(room_id) {
                continue;
            }
            // Before the messages, not after: a line that arrives in the same
            // sync as the join that explains a name must be read with it.
            if touches_membership(update) {
                self.refresh_names(room_id).await;
            }
            for raw in &update.ephemeral {
                self.handle_ephemeral(room_id, raw.json().get()).await;
            }
            for raw in &update.timeline.events {
                self.on_message(room_id, raw.raw().json().get()).await;
            }
        }
    }

    /// `m.presence` for everyone we share a room with. Cheap, and the whole
    /// point of the unprompted feature: nobody announces things to an empty
    /// room.
    async fn record_presence(&self, response: &matrix_sdk::sync::SyncResponse) {
        if response.presence.is_empty() {
            return;
        }
        let mut book = self.presence.lock().await;
        for raw in &response.presence {
            let Ok(event) = serde_json::from_str::<Value>(raw.json().get()) else {
                continue;
            };
            let (Some(user_id), Some(state)) = (
                event.get("sender").and_then(Value::as_str),
                event.pointer("/content/presence").and_then(Value::as_str),
            ) else {
                continue;
            };
            book.note(user_id, state);
        }
    }

    /// A typing notice: somebody is ABOUT to speak.
    ///
    /// Not a reason to say anything - it is a reason to make sure the model
    /// exists by the time they finish their sentence. On an always-on endpoint
    /// `warm` does nothing at all.
    async fn handle_ephemeral(&self, room_id: &RoomId, raw: &str) {
        let Ok(event) = serde_json::from_str::<Value>(raw) else {
            return;
        };
        if event.get("type").and_then(Value::as_str) != Some("m.typing") {
            return;
        }
        let Some(users) = event.pointer("/content/user_ids").and_then(Value::as_array) else {
            return;
        };
        let human = users
            .iter()
            .filter_map(Value::as_str)
            .find(|user_id| *user_id != self.me && !self.is_bot_user(user_id));
        if let Some(human) = human {
            self.brain
                .warm(&format!("{human} is typing in {room_id}"))
                .await;
        }
    }

    fn is_bot_user(&self, user_id: &str) -> bool {
        is_bot_user(user_id, &self.bot_user_ids, &self.bot_patterns)
    }

    // -- who this room can call whom ---------------------------------------

    /// Rebuild one room's names from the member store.
    ///
    /// Cheap and rare: it reads the store the sync has already filled - no
    /// HTTP - and only runs at startup and when a membership event says the
    /// answer may have changed.
    async fn refresh_names(&self, room_id: &RoomId) {
        let Some(worker) = self.workers.get(room_id) else {
            return;
        };
        let Some(room) = self.client.get_room(room_id) else {
            return;
        };
        let (names, participants) = self.names_for_room(&room).await;
        info!(
            "{room_id}: I answer to [{}]; {} other name(s) known: [{}]; {participants} here",
            names.mine().join(", "),
            names.theirs().len(),
            crate::head(&names.theirs().join(", "), 200)
        );
        let mut state = worker.state.lock().await;
        state.names = names;
        state.participants = participants;
    }

    /// Every name this room can call somebody by.
    ///
    /// Mine: the display name this room knows me by, the account's display name
    /// when the room has none yet, the first word of either, my localpart, and
    /// `policy.addressed_names`. Everybody else's: the joined members' display
    /// names, their first words and their localparts, plus the localparts of
    /// `policy.bot_user_ids` - which are configured rather than discovered, so
    /// they are known before anybody has spoken or even joined.
    ///
    /// Returns the room's size with them, because it is the same read of the
    /// same store and the tier-2 back-off wants it: in a room of three there is
    /// nobody else a question could have been meant for.
    async fn names_for_room(&self, room: &Room) -> (Names, usize) {
        let policy = &self.cfg.policy;
        let mut mine: Vec<String> = Vec::new();
        if let Ok(user) = UserId::parse(&self.me)
            && let Some(display) = room
                .get_member_no_sync(&user)
                .await
                .ok()
                .flatten()
                .and_then(|member| member.display_name().map(ToOwned::to_owned))
        {
            mine.extend(crate::addressing::names_from_display(&display));
        }
        if mine.is_empty()
            && let Some(display) = &self.account_display
        {
            mine.extend(crate::addressing::names_from_display(display));
        }
        mine.push(localpart(&self.me).to_owned());
        mine.extend(policy.addressed_names.iter().cloned());

        let members: Vec<(String, Option<String>)> =
            match room.members_no_sync(RoomMemberships::JOIN).await {
                Ok(joined) => joined
                    .iter()
                    .map(|member| {
                        (
                            member.user_id().as_str().to_owned(),
                            member.display_name().map(ToOwned::to_owned),
                        )
                    })
                    .collect(),
                Err(exc) => {
                    warn!(
                        "{}: cannot read the member list ({exc}); other names come from the \
                         config alone",
                        room.room_id()
                    );
                    Vec::new()
                }
            };
        (
            Names::new(
                &mine,
                other_names(policy, &self.me, &members, &self.bot_user_ids),
            ),
            members.len(),
        )
    }

    /// Normalise one raw timeline event, or None when it is not a message.
    ///
    /// The SDK has already decrypted it when the room is encrypted, so an
    /// encrypted room and a plain one reach the policy as the same event.
    async fn normalise(
        &self,
        room: Option<&Room>,
        room_id: &RoomId,
        raw: &str,
    ) -> Option<RoomEvent> {
        normalise_event(room, room_id, raw, self.rules()).await
    }
}

/// Whether this room update can have changed who is called what.
///
/// A join, a leave and a display-name change are all `m.room.member`, and for a
/// joined room they arrive in the timeline, in the state block, or in both.
fn touches_membership(update: &JoinedRoomUpdate) -> bool {
    let state = match &update.state {
        State::Before(events) | State::After(events) => events,
    };
    state.iter().any(is_member_event)
        || update
            .timeline
            .events
            .iter()
            .any(|event| is_member_event(event.raw()))
}

fn is_member_event<T>(raw: &Raw<T>) -> bool {
    raw.get_field::<String>("type")
        .ok()
        .flatten()
        .is_some_and(|kind| kind == "m.room.member")
}

/// Who spoke last in the conversation `ev` belongs to, out of the transcript.
///
/// The transcript is the room's own record of what happened in what order, so
/// this is a read of it rather than a second piece of bookkeeping to keep in
/// step with it. `ev` itself has already been appended by the time this runs,
/// so it is skipped by event id.
///
/// Two shapes, and the asymmetry is deliberate. A THREADED line's conversation
/// is its thread root, so only that thread can answer the question "who spoke
/// last here". An UNTHREADED one's conversation is the room, so the newest
/// message anywhere in it counts - because my answer to an unthreaded question
/// is itself threaded ON that question (`turn::post_reply`), and the human who
/// types "and why is that?" underneath it is answering me in the room, not
/// opening a new subject.
fn last_speaker(recent: &[RoomEvent], ev: &RoomEvent) -> Option<LastSpeaker> {
    let thread = ev.thread_root.as_deref();
    recent
        .iter()
        .rev()
        .find(|other| {
            other.event_id != ev.event_id
                && thread.is_none_or(|root| other.thread_root_or_self() == root)
        })
        .map(|other| LastSpeaker {
            sender: other.sender.clone(),
            ts: other.ts,
            conversation: other.thread_root_or_self().to_owned(),
        })
}

/// Whether this decision means a model call is coming.
///
/// Both judged verdicts: tier 2 (`consider`) waits out a back-off first and a
/// judged mention (`judge`) does not, and in both cases the next thing that
/// happens to the endpoint is a request. A tier-1 `reply` is not here - it goes
/// straight to the brain, and warming ahead of a request that is already on its
/// way buys nothing and costs one more.
fn wants_warm(decision: &Decision) -> bool {
    decision.needs_judge()
}

/// Get an on-demand model out of bed the moment a line lands that may need it.
///
/// The third warm-up, and the earliest: the typing notice fires before a line
/// exists and the back-off fires after the policy has decided, so this one -
/// on the human's finished sentence - is the one that runs when somebody types
/// a whole question and hits enter. All three are the same fire-and-forget call
/// behind the same cooldown, so the extra ones cost nothing.
pub(crate) async fn warm_for(brain: &dyn Brain, room_id: &RoomId, decision: &Decision) {
    if !wants_warm(decision) {
        return;
    }
    brain
        .warm(&format!(
            "a line in {room_id} may need an answer: {}",
            decision.reason
        ))
        .await;
}

/// The bot rules as the policy sees them.
pub(crate) fn rules_from<'a>(ids: &'a [String], patterns: &'a [Regex]) -> BotRules<'a> {
    BotRules {
        bot_user_ids: ids,
        bot_localpart_patterns: patterns,
    }
}

/// Normalise one raw timeline event, or None when it is not a message.
///
/// The SDK has already decrypted it when the room is encrypted, so an encrypted
/// room and a plain one reach the policy as the same event.
pub(crate) async fn normalise_event(
    room: Option<&Room>,
    room_id: &RoomId,
    raw: &str,
    rules: BotRules<'_>,
) -> Option<RoomEvent> {
    let source: Value = serde_json::from_str(raw).ok()?;
    if source.get("type").and_then(Value::as_str) != Some("m.room.message") {
        return None;
    }
    // A redacted or state-shaped event has no msgtype; it is not a line of
    // conversation and the policy has nothing to say about it.
    source.pointer("/content/msgtype").and_then(Value::as_str)?;
    let sender = source
        .get("sender")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The display name is the room's, not the event's, and it is read from
    // the store the sync already filled - never by fetching the member list
    // in the middle of a turn.
    let display = match (room, matrix_sdk::ruma::UserId::parse(sender)) {
        (Some(room), Ok(user)) => room
            .get_member_no_sync(&user)
            .await
            .ok()
            .flatten()
            .and_then(|member| member.display_name().map(ToOwned::to_owned)),
        _ => None,
    };
    Some(from_source(&source, room_id.as_str(), display, rules))
}

impl Connector {
    async fn on_message(&self, room_id: &RoomId, raw: &str) {
        let Some(worker) = self.workers.get(room_id) else {
            return;
        };
        let room = self.client.get_room(room_id);
        let Some(ev) = self.normalise(room.as_ref(), room_id, raw).await else {
            return;
        };
        let decision = {
            let mut state = worker.state.lock().await;
            if state.ledger.is_consumed(&ev.event_id) {
                debug!("{room_id}: {} already consumed", ev.event_id);
                return;
            }
            worker.transcript.append_seen(&ev);
            if !self.live.load(Ordering::SeqCst) || ev.ts < state.backlog_cutoff_ts {
                // Startup backlog: recorded as context, never answered.
                state.ledger.mark_consumed(&ev.event_id);
                info!(
                    "{room_id} {} from {}: verdict=silent (startup backlog, older than the \
                     snapshot)",
                    ev.event_id, ev.sender
                );
                return;
            }
            state.last_activity_ts = (self.clock)();
            if !ev.is_bot && ev.sender != self.me {
                state.last_human_post_ts = state.last_human_post_ts.max(ev.ts);
            }
            self.note_answered(&worker.room_id, &mut state, &ev);
            // Conversation energy is about the ROOM, not about me: every
            // message counts, including my own echo and the ones I will not
            // answer.
            let thread = ev.thread_root_or_self().to_owned();
            state.ledger.note_event(&thread, ev.is_bot);
            self.decide(worker, &mut state, &ev)
        };
        // Before the back-off, not after it: the whole cost of an on-demand
        // model is the loading, and the one moment we know a turn may be coming
        // is now.
        warm_for(self.brain.as_ref(), room_id, &decision).await;
        self.route(worker, decision, ev).await;
    }

    /// Somebody else spoke here, so anything I left open here is closed.
    ///
    /// This is what keeps a follow-up from being a nag: the loop exists because
    /// nobody came back to the question, and the moment anybody does - answer,
    /// change of subject, a joke - there is nothing left to follow up.
    fn note_answered(&self, room_id: &RoomId, state: &mut WorkerState, ev: &RoomEvent) {
        if ev.sender == self.me {
            return;
        }
        let reason = format!("{} posted in the thread", ev.sender);
        let closed = state
            .ledger
            .close_loops_in_thread(ev.thread_root_or_self(), &reason);
        for loop_ in closed {
            info!(
                "{room_id}: open loop closed by {}: {}",
                ev.sender,
                crate::head(&loop_.text, 80)
            );
            unprompted::forget_candidate(state, &format!("loop:{}", loop_.event_id));
        }
    }

    fn decide(&self, worker: &RoomWorker, state: &mut WorkerState, ev: &RoomEvent) -> Decision {
        let roots = state.ledger.thread_roots();
        let recent = worker.transcript.recent(LAST_SPEAKER_TAIL);
        let cues = Cues {
            names: &state.names,
            last_speaker: last_speaker(&recent, ev),
        };
        let decision = should_reply(ev, &self.me, &state.ledger, &roots, &self.cfg.policy, &cues);
        info!(
            "{} {} from {}: verdict={} ({})",
            ev.room_id,
            ev.event_id,
            ev.sender,
            decision.verdict.as_str(),
            decision.reason
        );
        decision
    }

    /// Send one decided event down the path its verdict names.
    async fn route(&self, worker: &Arc<RoomWorker>, decision: Decision, ev: RoomEvent) {
        if decision.verdict == Verdict::Reply {
            self.enqueue(worker, ev, 1).await;
            return;
        }
        if decision.needs_judge() && self.start_deliberation(worker, &ev, &decision).await {
            return;
        }
        if self.wants_inner_probe(worker, &decision, &ev).await {
            // Nobody addressed me and the guards said no. With inner thoughts
            // on I still ask what I would have wanted to say - not to say it,
            // but so that wanting to can add up. See `Runner::note_urgency`.
            let runner = self.runner();
            let worker_for_task = Arc::clone(worker);
            let ev = ev.clone();
            let handle =
                tokio::spawn(async move { runner.inner_thought_probe(worker_for_task, ev).await });
            self.track(handle).await;
        }
        worker.state.lock().await.ledger.mark_consumed(&ev.event_id);
    }

    /// Arm one judged reply. False when this room is already thinking.
    async fn start_deliberation(
        &self,
        worker: &Arc<RoomWorker>,
        ev: &RoomEvent,
        decision: &Decision,
    ) -> bool {
        {
            let mut state = worker.state.lock().await;
            if state.deliberating {
                info!(
                    "{}: already deliberating, dropping {} ({})",
                    worker.room_id, ev.event_id, decision.reason
                );
                return false;
            }
            state.deliberating = true;
        }
        let runner = self.runner();
        let worker_for_task = Arc::clone(worker);
        let ev = ev.clone();
        let decision = decision.clone();
        let handle = tokio::spawn(async move {
            runner.deliberate(worker_for_task, ev, decision).await;
        });
        self.track(handle).await;
        true
    }

    /// Whether to ask the judge what I would have said about this line.
    ///
    /// One probe at a time per room, exactly like one deliberation: ten people
    /// typing at once is not ten reasons to pay for ten judge calls about the
    /// same room.
    async fn wants_inner_probe(
        &self,
        worker: &Arc<RoomWorker>,
        decision: &Decision,
        ev: &RoomEvent,
    ) -> bool {
        if !self.cfg.policy.inner_thoughts
            || !decision.unaddressed
            || decision.verdict != Verdict::Silent
            || ev.is_bot
            || ev.sender == self.me
        {
            return false;
        }
        let mut state = worker.state.lock().await;
        if state.probing || state.deliberating {
            return false;
        }
        state.probing = true;
        true
    }

    async fn enqueue(&self, worker: &Arc<RoomWorker>, ev: RoomEvent, tier: u8) {
        {
            let mut state = worker.state.lock().await;
            if state.busy {
                info!(
                    "{}: a turn is already running, queued {} for coalescing",
                    worker.room_id, ev.event_id
                );
                state.pending.push(ev);
                return;
            }
            state.busy = true;
        }
        let runner = self.runner();
        let worker = Arc::clone(worker);
        let handle = tokio::spawn(async move { runner.drain(worker, ev, tier).await });
        self.track(handle).await;
    }

    /// A cheap handle onto everything a spawned turn needs.
    fn runner(&self) -> Runner {
        Runner {
            cfg: Arc::clone(&self.cfg),
            client: self.client.clone(),
            brain: Arc::clone(&self.brain),
            me: self.me.clone(),
            persona: self.persona.clone(),
            clock: Arc::clone(&self.clock),
            presence: Arc::clone(&self.presence),
            bot_user_ids: self.bot_user_ids.clone(),
            bot_patterns: self.bot_patterns.clone(),
        }
    }
}

/// Everybody ELSE's names: one entry per user id, with every name that user
/// answers to.
///
/// `members` is the room's member list, and `policy.other_names_from_members`
/// decides whether it is read at all: an agent whose display name is a common
/// word turns it off, and then only what the config NAMES is somebody else's.
/// The configured `bot_user_ids` are added either way - they are known before
/// anybody has joined or spoken, which is what makes "that line was addressed
/// to the other agent" decidable on the first message in a room.
#[must_use]
fn other_names(
    policy: &PolicyConfig,
    me: &str,
    members: &[(String, Option<String>)],
    bot_user_ids: &[String],
) -> Vec<(String, Vec<String>)> {
    let mut others: Vec<(String, Vec<String>)> = Vec::new();
    if policy.other_names_from_members {
        for (user_id, display) in members {
            if user_id == me {
                continue;
            }
            others.push((user_id.clone(), names_for(user_id, display.as_deref())));
        }
    }
    for user_id in bot_user_ids {
        if user_id != me && !others.iter().any(|(known, _)| known == user_id) {
            others.push((user_id.clone(), names_for(user_id, None)));
        }
    }
    others
}

/// Whether this config's policy would ever answer a bot at all. Used by the
/// startup log so an operator can see the shape of the agent they started.
#[must_use]
pub fn describes_bot_policy(cfg: &Config) -> &'static str {
    match cfg.policy.bot_to_bot {
        BotToBot::None => "never answers other bots",
        BotToBot::Mentions => "answers other bots when they mention or name it",
        BotToBot::All => "answers other bots",
        BotToBot::Conversational => "answers other bots, and may join in on what they say",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use crate::brain::{BrainContext, Judgement};
    use crate::policy::Verdict;

    const ME: &str = "@bot-a:example.com";
    const HUMAN: &str = "@human:example.com";

    #[test]
    fn the_member_list_is_read_for_names_only_while_the_policy_says_so() {
        // What the knob is FOR: an agent whose display name is a common word
        // turns it off, and then a member called "Alex" is no longer somebody
        // this agent will stand down for - only what `bot_user_ids` names is.
        let members = vec![(HUMAN.to_owned(), Some("Alex".to_owned()))];
        let bots = vec!["@bot-b:example.com".to_owned()];
        let mine = vec!["bot-a".to_owned()];

        let learning = PolicyConfig::default();
        let names =
            crate::addressing::Names::new(&mine, other_names(&learning, ME, &members, &bots));
        let (whose, _) = names
            .addresses_other("alex, what do you think?", false)
            .expect("the member list is where the human's name comes from");
        assert_eq!(whose, HUMAN);

        let deaf = PolicyConfig {
            other_names_from_members: false,
            ..PolicyConfig::default()
        };
        let names = crate::addressing::Names::new(&mine, other_names(&deaf, ME, &members, &bots));
        assert!(
            names
                .addresses_other("alex, what do you think?", false)
                .is_none(),
            "the member list was read with other_names_from_members off"
        );
        // The configured bots are known either way, which is what N2 rests on.
        let (whose, _) = names
            .addresses_other("bot-b, what do you think?", false)
            .expect("a configured bot id is a name whatever the member list says");
        assert_eq!(whose, "@bot-b:example.com");
    }

    /// One event as the homeserver sends it, threaded or not.
    fn event(event_id: &str, sender: &str, ts: f64, thread_root: Option<&str>) -> RoomEvent {
        let mut content = serde_json::json!({ "msgtype": "m.text", "body": "hello" });
        if let Some(root) = thread_root {
            content["m.relates_to"] = serde_json::json!({
                "rel_type": "m.thread",
                "event_id": root,
            });
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let origin_server_ts = (ts * 1000.0) as u64;
        from_source(
            &serde_json::json!({
                "type": "m.room.message",
                "event_id": event_id,
                "sender": sender,
                "origin_server_ts": origin_server_ts,
                "room_id": testkit::ROOM_ID,
                "content": content,
            }),
            testkit::ROOM_ID,
            None,
            rules_from(&[], &[]),
        )
    }

    #[test]
    fn the_last_speaker_of_an_unthreaded_line_is_the_newest_message_anywhere() {
        // The shape the follow-up arm exists for: the human asked, I answered
        // IN A THREAD on their question (which is what `post_reply` does), and
        // the human typed the next line straight into the room.
        let asked = event("$asked", HUMAN, 100.0, None);
        let mine = event("$mine", ME, 110.0, Some("$asked"));
        let next = event("$next", HUMAN, 115.0, None);
        let last = last_speaker(&[asked, mine, next.clone()], &next).expect("somebody spoke");
        assert_eq!(last.sender, ME);
        assert!((last.ts - 110.0).abs() < f64::EPSILON);
        assert_eq!(
            last.conversation, "$asked",
            "my answer's conversation is the thread it was posted in"
        );
    }

    #[test]
    fn the_event_that_just_arrived_is_never_its_own_last_speaker() {
        // `on_message` appends before it decides, so the trigger is in there.
        let only = event("$only", HUMAN, 100.0, None);
        assert_eq!(last_speaker(std::slice::from_ref(&only), &only), None);
        assert_eq!(last_speaker(&[], &only), None);
    }

    #[test]
    fn a_threaded_line_only_hears_its_own_thread() {
        let mine_here = event("$mine-here", ME, 100.0, Some("$root"));
        let elsewhere = event("$elsewhere", HUMAN, 110.0, Some("$other"));
        let threaded = event("$threaded", HUMAN, 120.0, Some("$root"));
        let recent = [mine_here, elsewhere, threaded.clone()];
        let last = last_speaker(&recent, &threaded).expect("somebody spoke in this thread");
        assert_eq!(last.sender, ME);
        assert_eq!(last.conversation, "$root");

        // ... and a thread nobody has spoken in yet has no last speaker, even
        // though the room does.
        let fresh = event("$fresh", HUMAN, 130.0, Some("$brand-new"));
        assert_eq!(last_speaker(&recent, &fresh), None);
    }

    /// A brain that answers nothing and counts what it was asked to prepare
    /// for. The counting is the gate: a warm-up is fire and forget, so the only
    /// thing that can be asserted about it is that it was asked for.
    struct CountingBrain(AtomicUsize);

    impl CountingBrain {
        fn new() -> Self {
            Self(AtomicUsize::new(0))
        }
        fn warms(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Brain for CountingBrain {
        async fn reply(&self, _ctx: &BrainContext) -> Option<String> {
            None
        }
        async fn judge(&self, _ctx: &BrainContext) -> Judgement {
            Judgement::no("counting only")
        }
        async fn warm(&self, _reason: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn decided(verdict: Verdict) -> Decision {
        Decision {
            verdict,
            reason: "for the test".to_owned(),
            unaddressed: verdict == Verdict::Consider,
            prescore: crate::addressing::PreScore::default(),
        }
    }

    #[tokio::test]
    async fn a_line_that_will_cost_a_model_call_warms_it_and_nothing_else_does() {
        let room = RoomId::parse(testkit::ROOM_ID).expect("a room id");
        for (verdict, warms) in [
            // A judged verdict means a request to the endpoint is coming, and
            // for tier 2 it is coming after a back-off nobody has to wait
            // through cold.
            (Verdict::Consider, 1),
            (Verdict::Judge, 1),
            // Tier 1 goes straight to the brain: warming ahead of a request
            // that is already on its way is one more request for nothing.
            (Verdict::Reply, 0),
            (Verdict::Silent, 0),
        ] {
            let brain = CountingBrain::new();
            warm_for(&brain, &room, &decided(verdict)).await;
            assert_eq!(brain.warms(), warms, "verdict={}", verdict.as_str());
        }
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    //! What the connector's own tests build a room out of.
    //!
    //! The parts that decide things - the queue, the hazard, the accumulator -
    //! are behind [`WorkerState`] and take no client, so they are unit-testable
    //! without a homeserver. Everything that posts is a live gate's job.

    use super::WorkerState;
    use crate::config::BudgetsConfig;
    use crate::ledger::{Clock, Ledger};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    pub const ROOM_ID: &str = "!room:example.com";

    #[derive(Clone)]
    pub struct FakeClock(Arc<Mutex<f64>>);

    impl FakeClock {
        pub fn new() -> Self {
            Self(Arc::new(Mutex::new(1_000.0)))
        }
        pub fn now(&self) -> f64 {
            *self.0.lock().expect("the clock is not poisoned")
        }
        pub fn advance(&self, seconds: f64) {
            *self.0.lock().expect("the clock is not poisoned") += seconds;
        }
        pub fn as_clock(&self) -> Clock {
            let inner = Arc::clone(&self.0);
            Arc::new(move || *inner.lock().expect("the clock is not poisoned"))
        }
    }

    /// One room's state, on a fake clock, with nothing to post to.
    pub fn state(dir: &Path, clock: &FakeClock) -> WorkerState {
        let ledger = Ledger::load(
            &dir.join("room.ledger.json"),
            BudgetsConfig::default(),
            clock.as_clock(),
        );
        WorkerState::new(ledger, clock.now())
    }
}
