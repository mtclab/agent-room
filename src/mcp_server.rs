//! `agent-room mcp`: the room, as tools an interactive session can hold.
//!
//! The other half of the design. The connector is a daemon whose brain is
//! spawned per message; this is a live Claude Code (or any MCP client) session
//! joining the same room as a first-class participant, with ITS OWN Matrix
//! account, reading, answering and waiting under its own steam.
//!
//! It is a Matrix CLIENT, not a bridge into a running connector. A session can
//! run this with an account no daemon has ever used; if the same account also
//! runs a connector, the two work independently and neither notices the other
//! beyond what the room shows (the connector's self-echo guard already ignores
//! its own posts).
//!
//! What it deliberately keeps from the daemon:
//!
//! - the same config file format, the same 0600 token rule, the same TLS;
//! - [`build_reply_content`], so a session's threaded reply has exactly the
//!   shape a connector's does and every other agent reads it the same way;
//! - the [`Ledger`], so a session cannot flood the room either. A live session
//!   is a program posting into a shared room, and "there is a person behind it"
//!   has never stopped anything from posting in a loop.
//!
//! Reads go through the Client-Server API directly ([`crate::cs_api`]) rather
//! than through the SDK's typed helpers - see that module for why.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use regex::Regex;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::io::stdio;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info};

use crate::config::{Config, require_private_mode};
use crate::cs_api::{CS_V1, CS_V3, CommandClient, CsError, authenticate, join_rooms, quote};
use crate::events::{
    ANNOTATION_REL_TYPE, BotRules, REACTION_TYPE, Relation, RoomEvent, THREAD_REL_TYPE,
    build_reply_content, from_source, is_message_source,
};
use crate::impulses::write_impulse;
use crate::ledger::{Clock, Ledger, system_clock};

pub const SERVER_NAME: &str = "agent-room";

/// Hard ceiling on what one `room_read` returns, whatever the caller asks for.
/// A room is a conversation, not a corpus: past a couple of hundred lines the
/// right tool is the transcript on disk, not a tool result.
pub const MAX_READ: i64 = 200;
/// How many `/messages` pages [`RoomClient::tail`] reads while hunting for
/// message events.
pub const TAIL_MAX_PAGES: usize = 4;
pub const DEFAULT_READ_LIMIT: i64 = 30;
/// Hard ceiling on one `room_wait`. Longer than this and the client's own
/// request timeout is the thing that decides, which is a worse failure.
pub const MAX_WAIT_S: f64 = 120.0;
pub const DEFAULT_WAIT_S: f64 = 60.0;
/// One `/sync` inside a wait. Shorter than the whole wait so a long wait can
/// still notice that its deadline has passed.
pub const SYNC_CHUNK_S: f64 = 20.0;
pub const DEFAULT_THREAD_LIMIT: i64 = 20;
pub const MAX_THREAD_LIMIT: i64 = 100;
/// How far back the `/threads` fallback scans `/messages` for thread relations.
pub const THREAD_SCAN: i64 = 200;
/// How many events `room_list` looks back through for the room's last message.
pub const ACTIVITY_SCAN: i64 = 20;

/// The session's budget ledger, kept beside the connector's rather than in it.
/// If a person points a session and a daemon at one `state_dir` by accident,
/// two processes must not write one file - and the daemon's budgets are its own
/// promise about how much IT talks.
pub const MCP_LEDGER_SUFFIX: &str = ".mcp-ledger.json";

pub const INSTRUCTIONS: &str = "\
You are a participant in a shared Matrix room, using your own account, next to
other people and their agents. Read before you post; answer in the thread you
were asked in; mention whoever you are answering. Post because you have
something to say, not because a message arrived.
";

/// What a tool says when it will not do the thing. One line, and what to do.
pub type ToolResult<T> = std::result::Result<T, String>;

// -- the shapes a tool answers with -------------------------------------------
//
// Every list-shaped result is wrapped in a `result` field. MCP structured
// content is an OBJECT, so a bare array cannot be returned - and `result` is
// the name every SDK's own wrapper uses, which is what the live gates read.

/// One configured room as `room_list` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct RoomOut {
    pub room_id: String,
    pub name: Option<String>,
    pub members: usize,
    pub last_activity_ts: Option<f64>,
}

/// One thing somebody said.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct MessageOut {
    pub event_id: String,
    pub sender: String,
    pub display_name: String,
    pub body: String,
    pub ts: f64,
    pub thread_root: Option<String>,
    pub mentions: Vec<String>,
    pub is_bot: bool,
}

/// What came of a `room_post` / `room_react`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PostOut {
    pub event_id: String,
    pub room_id: String,
    pub msgtype: String,
    pub thread_root: Option<String>,
    pub mentions: Vec<String>,
}

/// What came of a `room_impulse`: a file, and no message at all.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ImpulseOut {
    pub room_id: String,
    pub path: String,
    pub kind: String,
    pub summary: String,
    pub expires_in_s: f64,
}

/// One thread root with how busy it has been.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ThreadOut {
    pub thread_root: String,
    pub sender: String,
    pub body: String,
    pub ts: f64,
    pub reply_count: u64,
    pub last_activity_ts: f64,
}

/// The rooms this session is in.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Rooms {
    pub result: Vec<RoomOut>,
}

/// A stretch of conversation.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Messages {
    pub result: Vec<MessageOut>,
}

/// The room's recent threads.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Threads {
    pub result: Vec<ThreadOut>,
}

/// One normalised event as the tool result shape.
#[must_use]
pub fn render(ev: &RoomEvent) -> MessageOut {
    MessageOut {
        event_id: ev.event_id.clone(),
        sender: ev.sender.clone(),
        display_name: ev.display().to_owned(),
        body: ev.body.clone(),
        ts: ev.ts,
        thread_root: ev.thread_root.clone(),
        mentions: ev.mentions.iter().cloned().collect(),
        is_bot: ev.is_bot,
    }
}

fn clamp(value: i64, low: i64, high: i64) -> i64 {
    value.max(low).min(high)
}

/// Mentions a homeserver will accept, in the order they were given.
fn checked_mentions(mention: &[String]) -> ToolResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for user_id in mention {
        let candidate = user_id.trim();
        if !candidate.starts_with('@') || !candidate[1..].contains(':') {
            return Err(format!(
                "mention '{user_id}' is not a Matrix user id (@name:server)"
            ));
        }
        if !out.iter().any(|held| held == candidate) {
            out.push(candidate.to_owned());
        }
    }
    Ok(out)
}

// -- the Matrix half ----------------------------------------------------------

/// One account's live-session presence: the Matrix half of the tools.
///
/// Every method here is what a tool does; the tools themselves only validate
/// arguments and turn a refusal into an MCP error. Nothing is done at
/// construction time, so `agent-room mcp` starts instantly and a homeserver
/// that is down becomes a readable tool error rather than a server that will
/// not come up.
pub struct RoomClient {
    cfg: Arc<Config>,
    api: CommandClient,
    clock: Clock,
    bot_patterns: Vec<Regex>,
    me: Mutex<String>,
    ledgers: Mutex<BTreeMap<String, Ledger>>,
    members: Mutex<HashMap<String, HashMap<String, String>>>,
    /// Authenticate and join once, on the first tool call that needs it.
    ready: AsyncMutex<bool>,
    /// One `/sync` conversation at a time: two overlapping long polls on one
    /// account would fight over `next_batch` and lose events between them.
    sync: AsyncMutex<Option<String>>,
}

impl RoomClient {
    /// Build the session's client. Nothing here talks to a homeserver.
    ///
    /// # Errors
    /// When the TLS material or a bot pattern in the config is unusable.
    pub fn new(cfg: Arc<Config>, api: CommandClient, clock: Clock) -> anyhow::Result<Self> {
        let bot_patterns = cfg.policy.compiled_bot_patterns()?;
        let ledgers = cfg
            .rooms
            .iter()
            .map(|room_id| {
                let path = cfg.room_state_path(room_id, MCP_LEDGER_SUFFIX);
                (
                    room_id.clone(),
                    Ledger::load(&path, cfg.policy.budgets.clone(), Arc::clone(&clock)),
                )
            })
            .collect();
        let me = cfg.user_id.clone();
        Ok(Self {
            cfg,
            api,
            clock,
            bot_patterns,
            me: Mutex::new(me),
            ledgers: Mutex::new(ledgers),
            members: Mutex::new(HashMap::new()),
            ready: AsyncMutex::new(false),
            sync: AsyncMutex::new(None),
        })
    }

    /// The session's client for `cfg`, with the system clock.
    ///
    /// # Errors
    /// When the TLS material or a bot pattern in the config is unusable.
    pub fn from_config(cfg: Arc<Config>) -> anyhow::Result<Self> {
        let api = CommandClient::new(&cfg).map_err(|exc| anyhow::anyhow!("{exc}"))?;
        Self::new(cfg, api, system_clock())
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    fn me(&self) -> String {
        self.me.lock().map(|me| me.clone()).unwrap_or_default()
    }

    /// One tool's failure, written for a person to read.
    ///
    /// Anything that is not one of these reaches the caller as "Error executing
    /// tool `room_read`" and nothing else - which is unactionable when the real
    /// problem is a rejected token or a homeserver that is down. Both of those
    /// are ordinary and both have a one-line answer, so they get one.
    fn readable(&self, exc: &CsError) -> String {
        match exc {
            CsError::Refused(message) => message.clone(),
            CsError::Unreachable(cause) => format!(
                "the homeserver {} did not answer: {cause}",
                self.cfg.homeserver
            ),
        }
    }

    // -- lifecycle -------------------------------------------------------

    /// Authenticate and join, once, on the first tool call that needs it.
    async fn ensure_ready(&self) -> ToolResult<()> {
        let mut ready = self.ready.lock().await;
        if *ready {
            return Ok(());
        }
        let me = authenticate(&self.api, &self.cfg)
            .await
            .map_err(|exc| self.readable(&exc))?;
        let joined = join_rooms(&self.api, &self.cfg.rooms).await;
        if joined.is_empty() {
            return Err("could not join any configured room".to_owned());
        }
        if let Ok(mut held) = self.me.lock() {
            me.clone_into(&mut held);
        }
        *ready = true;
        info!("live session {me} in {}", joined.join(", "));
        Ok(())
    }

    // -- the Client-Server API -------------------------------------------

    /// Everyone joined to the room, mapped to their display name.
    ///
    /// One `/joined_members` call answers both "who is in here" (the count) and
    /// "what does the room call them" (the names a result is rendered with), so
    /// it is fetched once and kept.
    async fn member_names(
        &self,
        room_id: &str,
        refresh: bool,
    ) -> ToolResult<HashMap<String, String>> {
        if !refresh
            && let Ok(held) = self.members.lock()
            && let Some(names) = held.get(room_id)
        {
            return Ok(names.clone());
        }
        let members = self
            .api
            .joined_members(room_id)
            .await
            .map_err(|exc| self.readable(&exc))?;
        let Some(members) = members else {
            tracing::warn!("{room_id}: could not list members");
            let mut held = self.members.lock().map_err(|_| "member cache poisoned")?;
            return Ok(held.entry(room_id.to_owned()).or_default().clone());
        };
        let names: HashMap<String, String> = members.into_iter().collect();
        if let Ok(mut held) = self.members.lock() {
            held.insert(room_id.to_owned(), names.clone());
        }
        Ok(names)
    }

    fn normalise(
        &self,
        source: &Value,
        room_id: &str,
        names: &HashMap<String, String>,
    ) -> RoomEvent {
        let sender = source.get("sender").and_then(Value::as_str).unwrap_or("");
        let display = names.get(sender).filter(|name| !name.is_empty()).cloned();
        from_source(
            source,
            room_id,
            display,
            BotRules {
                bot_user_ids: &self.cfg.policy.bot_user_ids,
                bot_localpart_patterns: &self.bot_patterns,
            },
        )
    }

    /// Raw events -> conversation, oldest first, one event per id.
    fn events(
        &self,
        sources: &[Value],
        room_id: &str,
        names: &HashMap<String, String>,
    ) -> Vec<RoomEvent> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<RoomEvent> = Vec::new();
        for source in sources {
            if !is_message_source(source) {
                continue;
            }
            let ev = self.normalise(source, room_id, names);
            if ev.event_id.is_empty() || !seen.insert(ev.event_id.clone()) {
                continue;
            }
            out.push(ev);
        }
        out.sort_by(|left, right| left.ts.total_cmp(&right.ts));
        out
    }

    /// The room's newest raw events holding at least `limit` MESSAGES.
    ///
    /// `/messages` counts every event - joins, name changes, receipts of
    /// state - so a small `limit` on a freshly joined room can come back with
    /// nothing a person would call a message. Page back (`from` = the previous
    /// page's `end`) until `limit` message events are in hand, the history is
    /// exhausted, or [`TAIL_MAX_PAGES`] pages have been read.
    ///
    /// No `from` on the first page: paging back from a sync token means paging
    /// back from whatever the homeserver's sync cache last handed us, which can
    /// predate the traffic we are looking for. `/messages` with no token always
    /// starts at the room as it is now.
    async fn tail(&self, room_id: &str, limit: i64) -> ToolResult<Vec<Value>> {
        let path = format!("{CS_V3}/rooms/{}/messages", quote(room_id));
        let mut collected: Vec<Value> = Vec::new();
        let mut messages = 0_i64;
        let mut from: Option<String> = None;
        for _page in 0..TAIL_MAX_PAGES {
            let mut params: Vec<(&str, String)> =
                vec![("dir", "b".to_owned()), ("limit", limit.to_string())];
            if let Some(token) = &from {
                params.push(("from", token.clone()));
            }
            let (status, body) = self
                .api
                .get(&path, &params)
                .await
                .map_err(|exc| self.readable(&exc))?;
            if status != 200 || !body.is_object() {
                break;
            }
            let chunk: Vec<Value> = body
                .get("chunk")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if chunk.is_empty() {
                break;
            }
            messages += i64::try_from(
                chunk
                    .iter()
                    .filter(|event| is_message_source(event))
                    .count(),
            )
            .unwrap_or(i64::MAX);
            collected.extend(chunk);
            let end = body
                .get("end")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            match end {
                Some(token) if messages < limit => from = Some(token),
                _ => break,
            }
        }
        Ok(collected)
    }

    /// Receipts are the room-visible "I have read this". Best effort.
    async fn mark_read(&self, room_id: &str, ev: &RoomEvent) {
        let thread = ev.thread_root.clone().unwrap_or_else(|| "main".to_owned());
        self.api.mark_read(room_id, &ev.event_id, &thread).await;
    }

    // -- validation ------------------------------------------------------

    /// Refuse a room this session was not configured for.
    ///
    /// The configured list is the whole permission model: a session can only
    /// ever read or post where its operator put it.
    ///
    /// # Errors
    /// When `room_id` is not one of the configured rooms.
    pub fn check_room(&self, room_id: &str) -> ToolResult<()> {
        if self.cfg.rooms.iter().any(|room| room == room_id) {
            return Ok(());
        }
        Err(format!(
            "{room_id} is not one of this session's rooms ({}); check the config",
            self.cfg.rooms.join(", ")
        ))
    }

    /// Why this session may not post right now, or None.
    ///
    /// A budget that cannot be READ is a refusal, never a pass. Both of the
    /// ways this can fail used to answer None - "allowed" - through a `?`: a
    /// poisoned mutex, and a room with no ledger of its own. Neither should
    /// happen (every configured room gets a ledger in `new`, and nothing panics
    /// holding that lock), but the whole job of this function is to be the
    /// thing between a loop in a tool call and a loop in somebody's room, and a
    /// guard that answers "allowed" when it could not check is not that.
    #[must_use]
    pub fn budget_refusal(&self, room_id: &str) -> Option<String> {
        let now = self.now();
        let Ok(ledgers) = self.ledgers.lock() else {
            return Some(
                "this session's budget ledger could not be read (an earlier call left it \
                 poisoned), so nothing is posted - restart the server"
                    .to_owned(),
            );
        };
        let Some(ledger) = ledgers.get(room_id) else {
            return Some(format!(
                "{room_id} has no budget ledger in this session, so nothing is posted - it is \
                 not one of the configured rooms"
            ));
        };
        let check = ledger.hour_allows(now);
        if check.allowed {
            None
        } else {
            Some(check.reason)
        }
    }

    /// Refuse before anything is sent when the hourly cap is spent.
    ///
    /// Reactions count as well as messages: a loop that spams a thumbs-up is a
    /// loop in the room like any other.
    fn refuse_over_budget(&self, room_id: &str) -> ToolResult<()> {
        match self.budget_refusal(room_id) {
            None => Ok(()),
            Some(reason) => Err(format!(
                "refused by this session's posting budget - {reason}. \
                 Nothing was posted; wait, or raise policy.budgets.per_hour_max."
            )),
        }
    }

    fn record_post(&self, room_id: &str, event_id: &str, thread_root: &str, replied: &str) {
        let now = self.now();
        if let Ok(mut ledgers) = self.ledgers.lock()
            && let Some(ledger) = ledgers.get_mut(room_id)
        {
            ledger.record_post(event_id, thread_root, replied, Some(now), 1);
        }
    }

    // -- the tools -------------------------------------------------------

    /// The rooms this session is in: id, name, member count, last activity.
    ///
    /// # Errors
    /// When the homeserver cannot be reached or refuses the account.
    pub async fn list_rooms(&self) -> ToolResult<Vec<RoomOut>> {
        self.ensure_ready().await?;
        let mut rooms = Vec::new();
        for room_id in &self.cfg.rooms {
            let names = self.member_names(room_id, true).await?;
            let sources = self.tail(room_id, ACTIVITY_SCAN).await?;
            let events = self.events(&sources, room_id, &names);
            rooms.push(RoomOut {
                room_id: room_id.clone(),
                name: self.room_name(room_id).await?,
                members: names.len(),
                last_activity_ts: events.last().map(|ev| ev.ts),
            });
        }
        Ok(rooms)
    }

    async fn room_name(&self, room_id: &str) -> ToolResult<Option<String>> {
        let content = self
            .api
            .room_state_event(room_id, "m.room.name")
            .await
            .map_err(|exc| self.readable(&exc))?;
        Ok(content
            .as_ref()
            .and_then(|content| content.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(ToOwned::to_owned))
    }

    /// Recent messages in a room, oldest first.
    ///
    /// # Errors
    /// When the room is not one of this session's, or the homeserver refuses.
    pub async fn read(
        &self,
        room_id: &str,
        limit: i64,
        thread_root: Option<&str>,
        since_ts: Option<f64>,
    ) -> ToolResult<Vec<MessageOut>> {
        self.check_room(room_id)?;
        self.ensure_ready().await?;
        let limit = clamp(limit, 1, MAX_READ);
        let names = self.member_names(room_id, false).await?;
        let mut events = if let Some(root) = thread_root {
            self.thread_events(room_id, root, limit, &names).await?
        } else {
            let sources = self.tail(room_id, limit).await?;
            self.events(&sources, room_id, &names)
        };
        if let Some(cutoff) = since_ts {
            events.retain(|ev| ev.ts > cutoff);
        }
        let keep = usize::try_from(limit).unwrap_or(usize::MAX);
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        if let Some(newest) = events.last() {
            self.mark_read(room_id, newest).await;
        }
        Ok(events.iter().map(render).collect())
    }

    /// The thread root plus its newest replies, oldest first.
    ///
    /// The root is fetched separately because `/relations` returns only the
    /// events that point AT it - and the root is the question the thread is
    /// about, so a thread view without it reads as answers to nothing. It is
    /// also the one event `limit` may never trim away, which is why the
    /// trimming happens here and not on the combined list.
    async fn thread_events(
        &self,
        room_id: &str,
        thread_root: &str,
        limit: i64,
        names: &HashMap<String, String>,
    ) -> ToolResult<Vec<RoomEvent>> {
        let path = format!(
            "{CS_V1}/rooms/{}/relations/{}/{THREAD_REL_TYPE}",
            quote(room_id),
            quote(thread_root)
        );
        let (_status, chunk) = self
            .api
            .chunk(
                &path,
                &[("dir", "b".to_owned()), ("limit", limit.to_string())],
            )
            .await
            .map_err(|exc| self.readable(&exc))?;
        let mut replies = self.events(&chunk, room_id, names);
        replies.retain(|ev| ev.event_id != thread_root);
        let source = self
            .api
            .event(room_id, thread_root)
            .await
            .map_err(|exc| self.readable(&exc))?;
        let root: Vec<RoomEvent> = source
            .filter(is_message_source)
            .map(|source| self.normalise(&source, room_id, names))
            .into_iter()
            .collect();
        // The root is never the event a tight `limit` trims away: it is the
        // question the thread is about. Whatever is left of the budget goes on
        // the newest replies, and a budget of nothing keeps none of them.
        let keep = usize::try_from(limit)
            .unwrap_or(0)
            .saturating_sub(root.len());
        if replies.len() > keep {
            replies.drain(..replies.len() - keep);
        }
        Ok(root.into_iter().chain(replies).collect())
    }

    /// Say something in a room, as this session's own Matrix account.
    ///
    /// # Errors
    /// When the room is not one of this session's, the body is empty, a mention
    /// is not a user id, `reply_to` names no event, the budget is spent, or the
    /// homeserver refuses the message.
    pub async fn post(
        &self,
        room_id: &str,
        body: &str,
        thread_root: Option<&str>,
        reply_to: Option<&str>,
        mention: &[String],
    ) -> ToolResult<PostOut> {
        self.check_room(room_id)?;
        if body.trim().is_empty() {
            return Err("nothing to post: body is empty".to_owned());
        }
        let mut mentions = checked_mentions(mention)?;
        self.ensure_ready().await?;
        let mut replied_sender = String::new();
        if let Some(reply_to) = reply_to {
            let replied = self
                .api
                .event(room_id, reply_to)
                .await
                .map_err(|exc| self.readable(&exc))?;
            let Some(replied) = replied else {
                return Err(format!("reply_to {reply_to} is not an event in {room_id}"));
            };
            replied
                .get("sender")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .clone_into(&mut replied_sender);
            if !replied_sender.is_empty()
                && replied_sender != self.me()
                && !mentions.contains(&replied_sender)
            {
                mentions.push(replied_sender.clone());
            }
        }
        self.refuse_over_budget(room_id)?;
        let msgtype = self.cfg.mcp.post_as.msgtype();
        let content = build_reply_content(
            body,
            msgtype,
            &Relation {
                thread_root,
                reply_to,
                thread_fallback: None,
            },
            &mentions,
        );
        let event_id = self
            .api
            .send_event(room_id, "m.room.message", &content)
            .await
            .map_err(|exc| self.readable(&exc))?;
        self.record_post(
            room_id,
            &event_id,
            thread_root.unwrap_or(&event_id),
            &replied_sender,
        );
        info!("{room_id}: posted {event_id} as {msgtype}");
        Ok(PostOut {
            event_id,
            room_id: room_id.to_owned(),
            msgtype: msgtype.to_owned(),
            thread_root: thread_root.map(ToOwned::to_owned),
            mentions,
        })
    }

    /// Write one impulse into this config's inlet for `room_id`.
    ///
    /// Not a post, and nothing here talks to Matrix: it hands a connector a
    /// reason to speak later and lets the connector decide.
    ///
    /// # Errors
    /// When the room is not one of this session's, the text is empty, or the
    /// inlet cannot be written.
    pub fn impulse(&self, room_id: &str, text: &str, kind: &str) -> ToolResult<ImpulseOut> {
        self.check_room(room_id)?;
        if text.trim().is_empty() {
            return Err("nothing to record: text is empty".to_owned());
        }
        let ttl_s = self.cfg.policy.impulse_ttl_s;
        let path = write_impulse(&self.cfg.state_dir, room_id, text, kind, "", ttl_s, None)
            .map_err(|exc| exc.to_string())?;
        info!("{room_id}: impulse recorded at {}", path.display());
        Ok(ImpulseOut {
            room_id: room_id.to_owned(),
            path: path.display().to_string(),
            kind: kind.to_owned(),
            summary: text.trim().to_owned(),
            expires_in_s: ttl_s,
        })
    }

    /// React to one message with an emoji.
    ///
    /// # Errors
    /// When the room is not one of this session's, the key is empty, the budget
    /// is spent, or the homeserver refuses the annotation.
    pub async fn react(&self, room_id: &str, event_id: &str, key: &str) -> ToolResult<PostOut> {
        self.check_room(room_id)?;
        if key.trim().is_empty() {
            return Err("nothing to react with: key is empty".to_owned());
        }
        self.ensure_ready().await?;
        self.refuse_over_budget(room_id)?;
        let content = json!({
            "m.relates_to": {
                "rel_type": ANNOTATION_REL_TYPE,
                "event_id": event_id,
                "key": key,
            }
        });
        let sent = self
            .api
            .send_event(room_id, REACTION_TYPE, &content)
            .await
            .map_err(|exc| self.readable(&exc))?;
        self.record_post(room_id, &sent, event_id, "");
        Ok(PostOut {
            event_id: sent,
            room_id: room_id.to_owned(),
            msgtype: REACTION_TYPE.to_owned(),
            thread_root: None,
            mentions: Vec::new(),
        })
    }

    /// Recent threads: root message, reply count, last activity.
    ///
    /// # Errors
    /// When the room is not one of this session's, or the homeserver refuses.
    pub async fn threads(&self, room_id: &str, limit: i64) -> ToolResult<Vec<ThreadOut>> {
        self.check_room(room_id)?;
        self.ensure_ready().await?;
        let limit = clamp(limit, 1, MAX_THREAD_LIMIT);
        let names = self.member_names(room_id, false).await?;
        let path = format!("{CS_V1}/rooms/{}/threads", quote(room_id));
        let (status, chunk) = self
            .api
            .chunk(&path, &[("limit", limit.to_string())])
            .await
            .map_err(|exc| self.readable(&exc))?;
        let keep = usize::try_from(limit).unwrap_or(usize::MAX);
        if status == 200 {
            let mut out = self.threads_from_bundles(&chunk, room_id, &names);
            out.truncate(keep);
            return Ok(out);
        }
        info!("{room_id}: /threads answered {status}; falling back to a /messages scan");
        let sources = self.tail(room_id, THREAD_SCAN).await?;
        let mut out = self.threads_from_scan(&sources, room_id, &names);
        out.truncate(keep);
        Ok(out)
    }

    /// `/threads` returns the ROOTS, each carrying its thread's summary.
    fn threads_from_bundles(
        &self,
        chunk: &[Value],
        room_id: &str,
        names: &HashMap<String, String>,
    ) -> Vec<ThreadOut> {
        let mut out: Vec<ThreadOut> = Vec::new();
        for source in chunk {
            if !is_message_source(source) {
                continue;
            }
            let root = self.normalise(source, room_id, names);
            let bundle = source
                .get("unsigned")
                .and_then(|unsigned| unsigned.get("m.relations"))
                .and_then(|relations| relations.get(THREAD_REL_TYPE));
            let count = bundle
                .and_then(|bundle| bundle.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let latest_ts = bundle
                .and_then(|bundle| bundle.get("latest_event"))
                .and_then(|latest| latest.get("origin_server_ts"))
                .and_then(Value::as_f64)
                .map(|ms| ms / 1000.0)
                .filter(|ts| *ts > 0.0)
                .unwrap_or(root.ts);
            out.push(ThreadOut {
                thread_root: root.event_id.clone(),
                sender: root.sender.clone(),
                body: root.body.clone(),
                ts: root.ts,
                reply_count: count,
                last_activity_ts: latest_ts,
            });
        }
        out.sort_by(|left, right| right.last_activity_ts.total_cmp(&left.last_activity_ts));
        out
    }

    /// What `/threads` would have said, worked out from `/messages`.
    ///
    /// Homeservers older than Matrix v1.4 have no `/threads` at all, and the
    /// session should not lose the room's shape because of the server's
    /// version. Only the events in the scan window are counted, so the counts
    /// are a floor rather than the server's own total - which is stated in the
    /// tool description rather than quietly pretended away.
    fn threads_from_scan(
        &self,
        chunk: &[Value],
        room_id: &str,
        names: &HashMap<String, String>,
    ) -> Vec<ThreadOut> {
        let events = self.events(chunk, room_id, names);
        let by_id: HashMap<&str, &RoomEvent> =
            events.iter().map(|ev| (ev.event_id.as_str(), ev)).collect();
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut latest: BTreeMap<String, f64> = BTreeMap::new();
        for ev in &events {
            let Some(root_of) = &ev.thread_root else {
                continue;
            };
            *counts.entry(root_of.clone()).or_insert(0) += 1;
            let seen = latest.entry(root_of.clone()).or_insert(0.0);
            *seen = seen.max(ev.ts);
        }
        let mut out: Vec<ThreadOut> = counts
            .into_iter()
            .map(|(root_id, count)| {
                let last = latest.get(&root_id).copied().unwrap_or_default();
                let root = by_id.get(root_id.as_str());
                ThreadOut {
                    thread_root: root_id.clone(),
                    sender: root.map(|ev| ev.sender.clone()).unwrap_or_default(),
                    body: root.map(|ev| ev.body.clone()).unwrap_or_default(),
                    ts: root.map_or(last, |ev| ev.ts),
                    reply_count: count,
                    last_activity_ts: last,
                }
            })
            .collect();
        out.sort_by(|left, right| right.last_activity_ts.total_cmp(&left.last_activity_ts));
        out
    }

    /// Long-poll `/sync` until somebody else says something here.
    ///
    /// The first sync of every wait is a drain with no timeout: it moves the
    /// client up to "now" so that what comes back afterwards is genuinely new
    /// and an empty result genuinely means nobody spoke. `since_ts` is the
    /// exception - a caller that says how far it has read gets anything newer
    /// than that, including whatever the drain finds, because there is no point
    /// making it wait for news it has already missed.
    ///
    /// # Errors
    /// When the room is not one of this session's, or a sync is refused.
    pub async fn wait(
        &self,
        room_id: &str,
        timeout_s: f64,
        since_ts: Option<f64>,
    ) -> ToolResult<Vec<MessageOut>> {
        self.check_room(room_id)?;
        self.ensure_ready().await?;
        let timeout_s = timeout_s.clamp(0.0, MAX_WAIT_S);
        let names = self.member_names(room_id, false).await?;
        let mut sync = self.sync.lock().await;
        let drained = self.sync_once(&mut sync, room_id, &names, 0).await?;
        if let Some(cutoff) = since_ts {
            let fresh: Vec<RoomEvent> = drained.into_iter().filter(|ev| ev.ts > cutoff).collect();
            if !fresh.is_empty() {
                return self.delivered(room_id, &fresh).await;
            }
        }
        let deadline = self.now() + timeout_s;
        loop {
            let remaining = deadline - self.now();
            if remaining <= 0.0 {
                return Ok(Vec::new());
            }
            let chunk_ms = (remaining.min(SYNC_CHUNK_S) * 1000.0).max(0.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let chunk_ms = chunk_ms as u64;
            let mut events = self.sync_once(&mut sync, room_id, &names, chunk_ms).await?;
            if let Some(cutoff) = since_ts {
                events.retain(|ev| ev.ts > cutoff);
            }
            if !events.is_empty() {
                return self.delivered(room_id, &events).await;
            }
        }
    }

    async fn delivered(&self, room_id: &str, events: &[RoomEvent]) -> ToolResult<Vec<MessageOut>> {
        if let Some(newest) = events.last() {
            self.mark_read(room_id, newest).await;
        }
        Ok(events.iter().map(render).collect())
    }

    /// One `/sync`, filtered to this session's rooms, minus my own posts.
    async fn sync_once(
        &self,
        next_batch: &mut Option<String>,
        room_id: &str,
        names: &HashMap<String, String>,
        timeout_ms: u64,
    ) -> ToolResult<Vec<RoomEvent>> {
        let filter = self.sync_filter();
        // Never swallowed: a sync the homeserver refuses (a dead token, say)
        // returns instantly, so treating it as "nobody spoke" would spin the
        // wait loop against the server until the timeout ran out.
        let body = self
            .api
            .sync(next_batch.as_deref(), timeout_ms, Some(&filter))
            .await
            .map_err(|exc| self.readable(&exc))?;
        if let Some(token) = body.get("next_batch").and_then(Value::as_str) {
            *next_batch = Some(token.to_owned());
        }
        let sources: Vec<Value> = body
            .get("rooms")
            .and_then(|rooms| rooms.get("join"))
            .and_then(|join| join.get(room_id))
            .and_then(|room| room.get("timeline"))
            .and_then(|timeline| timeline.get("events"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let me = self.me();
        let mut events = self.events(&sources, room_id, names);
        events.retain(|ev| ev.sender != me);
        Ok(events)
    }

    /// Only this session's rooms, only the timeline. A session account can be
    /// in rooms that are none of its business, and a full sync would carry
    /// every one of them into this process.
    fn sync_filter(&self) -> Value {
        json!({
            "presence": {"types": []},
            "account_data": {"types": []},
            "room": {
                "rooms": self.cfg.rooms,
                "ephemeral": {"types": []},
                "account_data": {"types": []},
                "state": {"lazy_load_members": true},
                "timeline": {"limit": MAX_READ},
            },
        })
    }
}

// -- the tools ----------------------------------------------------------------

/// `room_read` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    pub room_id: String,
    /// How many messages, at most. Capped at 200.
    #[serde(default = "default_read_limit")]
    pub limit: i64,
    /// Read one thread (its root message included) instead of the room.
    #[serde(default)]
    pub thread_root: Option<String>,
    /// Only what is newer than this epoch-seconds timestamp.
    #[serde(default)]
    pub since_ts: Option<f64>,
}

fn default_read_limit() -> i64 {
    DEFAULT_READ_LIMIT
}

/// `room_post` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PostParams {
    pub room_id: String,
    pub body: String,
    /// Keep the message in that thread - answer where you were asked.
    #[serde(default)]
    pub thread_root: Option<String>,
    /// Quote one specific message and mention its sender.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Full user ids (`@name:server`) to mention.
    #[serde(default)]
    pub mention: Option<Vec<String>>,
}

/// `room_react` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReactParams {
    pub room_id: String,
    pub event_id: String,
    /// The emoji to react with.
    pub key: String,
}

/// `room_impulse` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpulseParams {
    pub room_id: String,
    /// The one line: what happened.
    pub text: String,
    /// Where it came from: git, build, render, note (free text).
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "note".to_owned()
}

/// `room_threads` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadsParams {
    pub room_id: String,
    /// How many threads, at most. Capped at 100.
    #[serde(default = "default_thread_limit")]
    pub limit: i64,
}

fn default_thread_limit() -> i64 {
    DEFAULT_THREAD_LIMIT
}

/// `room_wait` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams {
    pub room_id: String,
    /// How long to wait, in seconds. Capped at 120.
    #[serde(default = "default_wait_s")]
    pub timeout_s: f64,
    /// Also return anything newer than this epoch-seconds timestamp that
    /// arrived before the wait started.
    #[serde(default)]
    pub since_ts: Option<f64>,
}

fn default_wait_s() -> f64 {
    DEFAULT_WAIT_S
}

/// The stdio MCP server for one session account.
pub struct AgentRoomServer {
    rooms: Arc<RoomClient>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router, vis = "pub")]
impl AgentRoomServer {
    #[must_use]
    pub fn new(rooms: Arc<RoomClient>) -> Self {
        Self {
            rooms,
            tool_router: Self::tool_router(),
        }
    }

    /// The client the tools are a surface over.
    ///
    /// For gates that have to ask it something no tool exposes - the budget
    /// refusal for a room that is not configured, say, which `check_room`
    /// refuses before the budget is ever consulted.
    #[must_use]
    pub fn rooms(&self) -> &RoomClient {
        &self.rooms
    }

    /// The rooms this session is in: id, name, member count, last activity.
    #[tool(name = "room_list")]
    pub async fn room_list(&self) -> ToolResult<Json<Rooms>> {
        Ok(Json(Rooms {
            result: self.rooms.list_rooms().await?,
        }))
    }

    /// Recent messages in a room, oldest first.
    ///
    /// `thread_root` reads one thread (its root message included) instead of
    /// the room. `since_ts` returns only what is newer than that epoch-seconds
    /// timestamp. Reading sends a read receipt. At most 200 messages come back
    /// however large `limit` is.
    #[tool(name = "room_read")]
    pub async fn room_read(
        &self,
        Parameters(params): Parameters<ReadParams>,
    ) -> ToolResult<Json<Messages>> {
        Ok(Json(Messages {
            result: self
                .rooms
                .read(
                    &params.room_id,
                    params.limit,
                    params.thread_root.as_deref(),
                    params.since_ts,
                )
                .await?,
        }))
    }

    /// Say something in a room, as this session's own Matrix account.
    ///
    /// `thread_root` keeps the message in that thread - answer where you were
    /// asked. `reply_to` quotes one specific message and mentions its sender.
    /// `mention` is a list of full user ids (`@name:server`): an agent that is
    /// not mentioned will usually not see that it was addressed. Refused when
    /// the session's hourly posting budget is spent.
    #[tool(name = "room_post")]
    pub async fn room_post(
        &self,
        Parameters(params): Parameters<PostParams>,
    ) -> ToolResult<Json<PostOut>> {
        let mention = params.mention.unwrap_or_default();
        Ok(Json(
            self.rooms
                .post(
                    &params.room_id,
                    &params.body,
                    params.thread_root.as_deref(),
                    params.reply_to.as_deref(),
                    &mention,
                )
                .await?,
        ))
    }

    /// React to one message with an emoji (an `m.reaction` annotation).
    #[tool(name = "room_react")]
    pub async fn room_react(
        &self,
        Parameters(params): Parameters<ReactParams>,
    ) -> ToolResult<Json<PostOut>> {
        Ok(Json(
            self.rooms
                .react(&params.room_id, &params.event_id, &params.key)
                .await?,
        ))
    }

    /// Note that something happened, for the agent to mention if it is worth it.
    ///
    /// NOT a post: nothing reaches the room now. It drops one line in the
    /// room's impulse inlet, where the connector watching that `state_dir`
    /// finds it, waits until somebody is actually around, asks itself whether
    /// these people would want to know, and usually says nothing. It expires
    /// unspoken after `policy.impulse_ttl_s`.
    ///
    /// Use it for things that happened outside the room and might matter in
    /// it - a build finished, a PR merged, a long job came back. Use
    /// `room_post` when you have something to say now.
    ///
    /// # Errors
    /// When the room is not one of this session's, the text is empty, or the
    /// inlet cannot be written.
    #[tool(name = "room_impulse")]
    pub fn room_impulse(
        &self,
        Parameters(params): Parameters<ImpulseParams>,
    ) -> ToolResult<Json<ImpulseOut>> {
        // No homeserver is touched here, so there is no network failure to
        // translate. It writes one file, or says why not.
        Ok(Json(self.rooms.impulse(
            &params.room_id,
            &params.text,
            &params.kind,
        )?))
    }

    /// Recent threads: root message, reply count, last activity.
    ///
    /// On a homeserver without the `/threads` endpoint the counts are worked
    /// out from the last few hundred messages instead, so they are a floor.
    #[tool(name = "room_threads")]
    pub async fn room_threads(
        &self,
        Parameters(params): Parameters<ThreadsParams>,
    ) -> ToolResult<Json<Threads>> {
        Ok(Json(Threads {
            result: self.rooms.threads(&params.room_id, params.limit).await?,
        }))
    }

    /// Wait for somebody else to speak in a room. This is how you listen.
    ///
    /// Returns the new messages as soon as any arrive, or an empty list when
    /// the wait times out - your own posts never count. `timeout_s` is capped
    /// at 120. `since_ts` also returns anything newer than that timestamp that
    /// arrived before the wait started.
    #[tool(name = "room_wait")]
    pub async fn room_wait(
        &self,
        Parameters(params): Parameters<WaitParams>,
    ) -> ToolResult<Json<Messages>> {
        Ok(Json(Messages {
            result: self
                .rooms
                .wait(&params.room_id, params.timeout_s, params.since_ts)
                .await?,
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentRoomServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, crate::cli::VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Run the stdio MCP server until the client closes it.
///
/// The token file is checked here, before anything is served: a session that
/// would hand its account to whoever can read a 0644 file should not start at
/// all, and failing at startup is what the person driving it will actually see.
///
/// # Errors
/// When the token file is too permissive, the config is unusable, or the stdio
/// transport fails.
pub async fn serve(cfg: Config) -> anyhow::Result<i32> {
    if let Some(path) = &cfg.access_token_file {
        require_private_mode(path, "access_token_file")?;
    }
    let rooms = Arc::new(RoomClient::from_config(Arc::new(cfg))?);
    let service = rmcp::serve_server(AgentRoomServer::new(rooms), stdio()).await?;
    debug!("{SERVER_NAME} MCP server ready on stdio");
    service.waiting().await?;
    Ok(0)
}
