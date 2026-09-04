//! The MCP server, driven the way a session drives it: through the tools.
//!
//! Every test calls the real tool functions and turns what they return into a
//! `CallToolResult` with rmcp's own [`IntoCallToolResult`] - the same
//! conversion the tool router does - so what is asserted is what an MCP client
//! sees: the structured JSON, and a refusal arriving as a TOOL error (the
//! model can read it and act) rather than a protocol error (the model gets
//! "Error executing tool `room_read`" and nothing else).
//!
//! Only Matrix is faked, and only at the socket: [`FakeHomeserver`] answers
//! `/messages`, `/relations`, `/threads`, `/event`, `/sync` and `/send` over
//! real HTTP.

use std::sync::Arc;

use agent_room::config::{
    Config, McpConfig, PolicyConfig, PostAs, TlsConfig, default_history_limit,
    default_transcript_archives, default_transcript_keep,
};
use agent_room::cs_api::CommandClient;
use agent_room::impulses::{impulse_dir, read_impulses};
use agent_room::mcp_server::{
    AgentRoomServer, ImpulseParams, MAX_READ, MAX_WAIT_S, PostParams, ReactParams, ReadParams,
    RoomClient, ThreadsParams, WaitParams,
};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResponse, CallToolResult};
use serde_json::{Value, json};

use super::fake_homeserver::{FakeClock, FakeHomeserver};

/// The wait cap in milliseconds, which is what `/sync` is asked for.
const MAX_WAIT_MS: u64 = 120_000;

pub const ME: &str = "@bot-a:example.com";
pub const HUMAN: &str = "@human:example.com";
pub const OTHER_BOT: &str = "@bot-b:example.com";
pub const ROOM_ID: &str = "!room:example.com";
pub const OTHER_ROOM: &str = "!elsewhere:example.com";

/// One live session: a fake homeserver, a config pointed at it, and the tools.
pub struct Session {
    pub dir: tempfile::TempDir,
    pub home: FakeHomeserver,
    pub clock: FakeClock,
    pub tools: AgentRoomServer,
}

impl Session {
    pub async fn new() -> Self {
        Self::with(PostAs::Notice, PolicyConfig::default(), None).await
    }

    /// A session on a state directory that already exists, so a "restart" test
    /// can build a second one over the first one's ledger.
    pub async fn with(
        post_as: PostAs,
        policy: PolicyConfig,
        reuse: Option<(tempfile::TempDir, FakeHomeserver)>,
    ) -> Self {
        let (dir, home) = match reuse {
            Some(held) => held,
            None => (
                tempfile::tempdir().expect("tmpdir"),
                FakeHomeserver::start(ROOM_ID, ME).await,
            ),
        };
        home.with(|state| {
            state.members.insert(ME.to_owned(), "me".to_owned());
            state.members.insert(HUMAN.to_owned(), "Alex".to_owned());
            state
                .members
                .insert(OTHER_BOT.to_owned(), "bot-b".to_owned());
        });
        let clock = FakeClock::new();
        let cfg = Arc::new(config(&dir, &home.base_url, post_as, policy));
        let api = CommandClient::new(&cfg).expect("a plain HTTP client");
        let rooms = RoomClient::new(cfg, api, clock.as_clock()).expect("the session client builds");
        Self {
            dir,
            home,
            clock,
            tools: AgentRoomServer::new(Arc::new(rooms)),
        }
    }

    /// Hand the temp dir and the homeserver on to a second session.
    pub fn into_parts(self) -> (tempfile::TempDir, FakeHomeserver) {
        (self.dir, self.home)
    }
}

fn config(
    dir: &tempfile::TempDir,
    homeserver: &str,
    post_as: PostAs,
    policy: PolicyConfig,
) -> Config {
    let token = dir.path().join("access");
    if !token.exists() {
        agent_room::config::write_secret_file(&token, "syt_fake").expect("the token is written");
    }
    Config {
        homeserver: homeserver.to_owned(),
        user_id: ME.to_owned(),
        access_token_file: Some(token),
        password: None,
        rooms: vec![ROOM_ID.to_owned()],
        persona_file: None,
        state_dir: dir.path().join("state"),
        brain: None,
        policy,
        mcp: McpConfig { post_as },
        tls: TlsConfig::default(),
        history_limit: default_history_limit(),
        transcript_keep: default_transcript_keep(),
        transcript_archives: default_transcript_archives(),
        allow_wedged_device: false,
    }
}

fn budgets(per_hour_max: u32) -> PolicyConfig {
    let mut policy = PolicyConfig::default();
    policy.budgets.per_hour_max = per_hour_max;
    policy
}

// -- what an MCP client sees --------------------------------------------------

/// One tool return value, as the tool router would hand it to a client.
pub fn call<T: IntoCallToolResult>(value: T) -> CallToolResult {
    match value.into_call_tool_result() {
        Ok(CallToolResponse::Complete(result)) => result,
        Ok(other) => panic!("a room tool answered with {other:?}"),
        Err(exc) => panic!(
            "a refusal reached the client as a PROTOCOL error, which the model cannot read: {exc}"
        ),
    }
}

/// The JSON a tool returned, or an assertion naming the error it returned.
pub fn payload(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{}", error_text(result));
    let structured = result
        .structured_content
        .clone()
        .expect("every tool answers with structured JSON");
    structured.get("result").cloned().unwrap_or(structured)
}

pub fn error_text(result: &CallToolResult) -> String {
    serde_json::to_value(&result.content)
        .ok()
        .and_then(|blocks| {
            blocks.as_array().map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        })
        .unwrap_or_default()
}

fn bodies(messages: &Value) -> Vec<String> {
    messages
        .as_array()
        .expect("a list of messages")
        .iter()
        .map(|message| message["body"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn read_params(room_id: &str) -> ReadParams {
    ReadParams {
        room_id: room_id.to_owned(),
        limit: 30,
        thread_root: None,
        since_ts: None,
    }
}

fn post_params(room_id: &str, body: &str) -> PostParams {
    PostParams {
        room_id: room_id.to_owned(),
        body: body.to_owned(),
        thread_root: None,
        reply_to: None,
        mention: None,
    }
}

// -- the tools exist ----------------------------------------------------------

#[test]
fn the_server_offers_exactly_the_room_tools() {
    let names: std::collections::BTreeSet<String> = AgentRoomServer::tool_router()
        .list_all()
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert_eq!(
        names,
        [
            "room_impulse",
            "room_list",
            "room_post",
            "room_react",
            "room_read",
            "room_threads",
            "room_wait",
        ]
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<std::collections::BTreeSet<String>>()
    );
}

// -- argument validation ------------------------------------------------------

#[tokio::test]
async fn a_room_this_session_is_not_in_is_refused_by_every_tool() {
    // The configured room list IS the permission model, on every tool.
    let session = Session::new().await;
    let mut refusals = Vec::new();
    refusals.push(call(
        session
            .tools
            .room_read(Parameters(read_params(OTHER_ROOM)))
            .await,
    ));
    refusals.push(call(
        session
            .tools
            .room_post(Parameters(post_params(OTHER_ROOM, "hello")))
            .await,
    ));
    refusals.push(call(
        session
            .tools
            .room_react(Parameters(ReactParams {
                room_id: OTHER_ROOM.to_owned(),
                event_id: "$e1".to_owned(),
                key: "\u{1f44d}".to_owned(),
            }))
            .await,
    ));
    refusals.push(call(
        session
            .tools
            .room_threads(Parameters(ThreadsParams {
                room_id: OTHER_ROOM.to_owned(),
                limit: 20,
            }))
            .await,
    ));
    refusals.push(call(
        session
            .tools
            .room_wait(Parameters(WaitParams {
                room_id: OTHER_ROOM.to_owned(),
                timeout_s: 1.0,
                since_ts: None,
            }))
            .await,
    ));
    refusals.push(call(session.tools.room_impulse(Parameters(
        ImpulseParams {
            room_id: OTHER_ROOM.to_owned(),
            text: "something".to_owned(),
            kind: "note".to_owned(),
        },
    ))));
    for refused in &refusals {
        assert_eq!(refused.is_error, Some(true));
        let text = error_text(refused);
        assert!(
            text.contains(OTHER_ROOM) && text.contains(ROOM_ID),
            "{text}"
        );
    }
    assert!(session.home.with(|state| state.sent.is_empty()));
}

#[tokio::test]
async fn an_empty_body_is_refused_before_anything_is_sent() {
    let session = Session::new().await;
    let refused = call(
        session
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "   ")))
            .await,
    );
    assert_eq!(refused.is_error, Some(true));
    assert!(error_text(&refused).contains("empty"));
    assert!(session.home.with(|state| state.sent.is_empty()));
}

#[tokio::test]
async fn a_mention_that_is_not_a_user_id_is_refused() {
    let session = Session::new().await;
    let mut params = post_params(ROOM_ID, "hi");
    params.mention = Some(vec!["bot-b".to_owned()]);
    let refused = call(session.tools.room_post(Parameters(params)).await);
    assert_eq!(refused.is_error, Some(true));
    assert!(error_text(&refused).contains("not a Matrix user id"));
    assert!(session.home.with(|state| state.sent.is_empty()));
}

#[tokio::test]
async fn an_empty_reaction_key_is_refused() {
    let session = Session::new().await;
    let refused = call(
        session
            .tools
            .room_react(Parameters(ReactParams {
                room_id: ROOM_ID.to_owned(),
                event_id: "$e1".to_owned(),
                key: String::new(),
            }))
            .await,
    );
    assert_eq!(refused.is_error, Some(true));
    assert!(session.home.with(|state| state.sent.is_empty()));
}

#[tokio::test]
async fn a_reply_to_an_event_that_is_not_there_is_refused() {
    let session = Session::new().await;
    let mut params = post_params(ROOM_ID, "hi");
    params.reply_to = Some("$nope".to_owned());
    let refused = call(session.tools.room_post(Parameters(params)).await);
    assert_eq!(refused.is_error, Some(true));
    assert!(error_text(&refused).contains("$nope"));
    assert!(session.home.with(|state| state.sent.is_empty()));
}

// -- reading ------------------------------------------------------------------

#[tokio::test]
async fn room_read_returns_the_conversation_oldest_first() {
    let session = Session::new().await;
    session.home.with(|state| {
        state.add("first", HUMAN);
        state.add_notice("second", OTHER_BOT);
        state.add_state("m.room.member");
    });
    let messages = payload(&call(
        session
            .tools
            .room_read(Parameters(read_params(ROOM_ID)))
            .await,
    ));
    assert_eq!(
        bodies(&messages),
        vec!["first", "second"],
        "state events are not conversation"
    );
    assert_eq!(messages[0]["sender"], json!(HUMAN));
    assert_eq!(messages[0]["display_name"], json!("Alex"));
    assert_eq!(messages[0]["is_bot"], json!(false));
    assert_eq!(
        messages[1]["is_bot"],
        json!(true),
        "an m.notice is another agent talking"
    );
    assert!(messages[0]["ts"].as_f64() < messages[1]["ts"].as_f64());
}

#[tokio::test]
async fn room_read_pages_past_state_events_to_find_messages() {
    // A fresh join puts member/name events on top of the room; limit=2 must
    // still return two MESSAGES, not two joins filtered down to nothing.
    let session = Session::new().await;
    session.home.with(|state| {
        state.add("first", HUMAN);
        state.add("second", HUMAN);
        for _ in 0..5 {
            state.add_state("m.room.member");
        }
    });
    let mut params = read_params(ROOM_ID);
    params.limit = 2;
    let messages = payload(&call(session.tools.room_read(Parameters(params)).await));
    assert_eq!(bodies(&messages), vec!["first", "second"]);
    let pages = session.home.with(|state| state.message_pages());
    assert!(pages.len() > 1, "it must have paged: {pages:?}");
    assert!(
        !pages[0].contains_key("from"),
        "the first page never starts from a sync token"
    );
    assert!(pages.iter().all(|page| page["limit"] == "2"));
}

#[tokio::test]
async fn room_read_never_asks_for_more_than_the_ceiling() {
    // A caller asking for the whole history gets the cap, not the history.
    let session = Session::new().await;
    let mut params = read_params(ROOM_ID);
    params.limit = 9999;
    let messages = payload(&call(session.tools.room_read(Parameters(params)).await));
    assert_eq!(messages, json!([]));
    let limits: Vec<String> = session
        .home
        .with(|state| state.message_pages())
        .iter()
        .map(|page| page["limit"].clone())
        .collect();
    assert_eq!(limits, vec![MAX_READ.to_string()]);
}

#[tokio::test]
async fn room_read_since_ts_only_returns_what_is_newer() {
    let session = Session::new().await;
    let old = session.home.with(|state| {
        let old = state.add("old news", HUMAN);
        state.add("fresh news", HUMAN);
        old
    });
    let mut params = read_params(ROOM_ID);
    params.since_ts = Some(old["origin_server_ts"].as_f64().expect("a ts") / 1000.0);
    let messages = payload(&call(session.tools.room_read(Parameters(params)).await));
    assert_eq!(bodies(&messages), vec!["fresh news"]);
}

#[tokio::test]
async fn a_thread_read_starts_with_the_message_the_thread_is_about() {
    let session = Session::new().await;
    let root = session.home.with(|state| {
        let root = state.add("what do you think?", HUMAN);
        let root_id = root["event_id"].as_str().expect("an id").to_owned();
        state.add_threaded("I think yes", OTHER_BOT, "m.notice", &root_id);
        state.add("unrelated chatter", HUMAN);
        root_id
    });
    let mut params = read_params(ROOM_ID);
    params.thread_root = Some(root.clone());
    let messages = payload(&call(session.tools.room_read(Parameters(params)).await));
    assert_eq!(bodies(&messages), vec!["what do you think?", "I think yes"]);
    assert_eq!(messages[1]["thread_root"], json!(root));
}

#[tokio::test]
async fn a_thread_read_keeps_the_root_even_at_limit_one() {
    // Spending `limit` on the newest replies first would return the whole
    // thread for the tightest possible request. The root comes first.
    let session = Session::new().await;
    let root = session.home.with(|state| {
        let root = state.add("the question", HUMAN);
        let root_id = root["event_id"].as_str().expect("an id").to_owned();
        for index in 0..4 {
            state.add_threaded(&format!("answer {index}"), OTHER_BOT, "m.notice", &root_id);
        }
        root_id
    });
    let mut one = read_params(ROOM_ID);
    one.thread_root = Some(root.clone());
    one.limit = 1;
    let mut three = read_params(ROOM_ID);
    three.thread_root = Some(root);
    three.limit = 3;
    let at_one = payload(&call(session.tools.room_read(Parameters(one)).await));
    let at_three = payload(&call(session.tools.room_read(Parameters(three)).await));
    assert_eq!(bodies(&at_one), vec!["the question"]);
    assert_eq!(
        bodies(&at_three),
        vec!["the question", "answer 2", "answer 3"]
    );
}

#[tokio::test]
async fn reading_sends_a_read_receipt_but_survives_one_failing() {
    let session = Session::new().await;
    let newest = session.home.with(|state| {
        state.add("hello", HUMAN)["event_id"]
            .as_str()
            .unwrap()
            .to_owned()
    });
    let messages = payload(&call(
        session
            .tools
            .room_read(Parameters(read_params(ROOM_ID)))
            .await,
    ));
    assert_eq!(bodies(&messages), vec!["hello"]);
    assert_eq!(
        session.home.with(|state| state.receipts.clone()),
        vec![newest]
    );

    session.home.with(|state| state.receipts_fail = true);
    let again = payload(&call(
        session
            .tools
            .room_read(Parameters(read_params(ROOM_ID)))
            .await,
    ));
    assert_eq!(
        bodies(&again),
        vec!["hello"],
        "a receipt the homeserver refused must not lose the read"
    );
}

#[tokio::test]
async fn room_list_reports_the_room_its_name_and_its_last_word() {
    let session = Session::new().await;
    let last = session
        .home
        .with(|state| state.add("the last thing anybody said", HUMAN));
    let rooms = payload(&call(session.tools.room_list().await));
    assert_eq!(
        rooms,
        json!([{
            "room_id": ROOM_ID,
            "name": "the room",
            "members": 3,
            "last_activity_ts": last["origin_server_ts"].as_f64().expect("a ts") / 1000.0,
        }])
    );
}

// -- posting ------------------------------------------------------------------

#[tokio::test]
async fn a_post_is_a_notice_by_default_and_text_when_configured() {
    // A session is still a program in the room, so `notice` is the default.
    let session = Session::new().await;
    let result = payload(&call(
        session
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "hello")))
            .await,
    ));
    assert_eq!(result["msgtype"], json!("m.notice"));
    assert_eq!(
        session
            .home
            .with(|state| state.sent[0].2["msgtype"].clone()),
        json!("m.notice")
    );

    let as_text = Session::with(PostAs::Text, PolicyConfig::default(), None).await;
    let result = payload(&call(
        as_text
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "hello")))
            .await,
    ));
    assert_eq!(result["msgtype"], json!("m.text"));
    assert_eq!(
        as_text
            .home
            .with(|state| state.sent[0].2["msgtype"].clone()),
        json!("m.text")
    );
}

#[tokio::test]
async fn a_threaded_post_carries_the_thread_relation_and_the_mentions() {
    let session = Session::new().await;
    let root = session.home.with(|state| {
        state.add("question?", HUMAN)["event_id"]
            .as_str()
            .unwrap()
            .to_owned()
    });
    let mut params = post_params(ROOM_ID, "an answer");
    params.thread_root = Some(root.clone());
    params.mention = Some(vec![HUMAN.to_owned()]);
    let result = payload(&call(session.tools.room_post(Parameters(params)).await));
    let (_room, event_type, content) = session.home.with(|state| state.sent[0].clone());
    assert_eq!(event_type, "m.room.message");
    assert_eq!(content["m.relates_to"]["rel_type"], json!("m.thread"));
    assert_eq!(content["m.relates_to"]["event_id"], json!(root));
    assert_eq!(content["m.mentions"], json!({"user_ids": [HUMAN]}));
    assert_eq!(result["thread_root"], json!(root));
}

#[tokio::test]
async fn a_reply_mentions_the_person_it_answers() {
    let session = Session::new().await;
    let target = session.home.with(|state| {
        state.add("what about this?", HUMAN)["event_id"]
            .as_str()
            .unwrap()
            .to_owned()
    });
    let mut params = post_params(ROOM_ID, "about that");
    params.reply_to = Some(target.clone());
    payload(&call(session.tools.room_post(Parameters(params)).await));
    let content = session.home.with(|state| state.sent[0].2.clone());
    assert_eq!(content["m.mentions"], json!({"user_ids": [HUMAN]}));
    assert_eq!(
        content["m.relates_to"]["m.in_reply_to"],
        json!({"event_id": target})
    );
}

#[tokio::test]
async fn a_reaction_is_an_annotation_on_the_event() {
    let session = Session::new().await;
    payload(&call(
        session
            .tools
            .room_react(Parameters(ReactParams {
                room_id: ROOM_ID.to_owned(),
                event_id: "$e1".to_owned(),
                key: "\u{1f44d}".to_owned(),
            }))
            .await,
    ));
    let (_room, event_type, content) = session.home.with(|state| state.sent[0].clone());
    assert_eq!(event_type, "m.reaction");
    assert_eq!(
        content["m.relates_to"],
        json!({"rel_type": "m.annotation", "event_id": "$e1", "key": "\u{1f44d}"})
    );
}

// -- the budget ---------------------------------------------------------------

#[tokio::test]
async fn the_hourly_budget_refuses_the_post_and_says_so() {
    // A session is not exempt from the thing that stops a room being flooded.
    let session = Session::with(PostAs::Notice, budgets(2), None).await;
    for body in ["one", "two"] {
        payload(&call(
            session
                .tools
                .room_post(Parameters(post_params(ROOM_ID, body)))
                .await,
        ));
    }
    let refused = call(
        session
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "three")))
            .await,
    );
    assert_eq!(refused.is_error, Some(true));
    assert!(
        error_text(&refused).contains("budget"),
        "{}",
        error_text(&refused)
    );
    assert_eq!(
        session.home.with(|state| state.sent_bodies()),
        vec!["one", "two"]
    );
}

/// A budget that cannot be READ refuses; it never passes.
///
/// Both ways of failing to read one used to answer "allowed" through a `?`: a
/// poisoned mutex, and a room the session has no ledger for. Neither should
/// happen - but this function is the only thing between a loop in a tool call
/// and a loop in somebody's room, and the safe direction is silence.
#[tokio::test]
async fn a_budget_that_cannot_be_read_refuses_rather_than_passes() {
    let session = Session::new().await;
    let refusal = session
        .tools
        .rooms()
        .budget_refusal("!no-ledger-here:example.com")
        .expect("a room with no ledger is refused, not allowed");
    assert!(refusal.contains("no budget ledger"), "{refusal}");
    assert!(refusal.contains("nothing is posted"), "{refusal}");
    // ... and the configured room, which does have one, still passes.
    assert!(session.tools.rooms().budget_refusal(ROOM_ID).is_none());
}

#[tokio::test]
async fn the_budget_survives_a_restart_of_the_session() {
    // The ledger is on disk, so closing the client does not reset the cap.
    let session = Session::with(PostAs::Notice, budgets(1), None).await;
    payload(&call(
        session
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "one")))
            .await,
    ));
    let parts = session.into_parts();
    let restarted = Session::with(PostAs::Notice, budgets(1), Some(parts)).await;
    let refused = call(
        restarted
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "two")))
            .await,
    );
    assert_eq!(refused.is_error, Some(true));
    assert!(error_text(&refused).contains("budget"));
    assert_eq!(restarted.home.with(|state| state.sent.len()), 1);
}

#[tokio::test]
async fn a_reaction_is_traffic_too_and_the_budget_counts_it() {
    let session = Session::with(PostAs::Notice, budgets(1), None).await;
    payload(&call(
        session
            .tools
            .room_react(Parameters(ReactParams {
                room_id: ROOM_ID.to_owned(),
                event_id: "$e1".to_owned(),
                key: "+".to_owned(),
            }))
            .await,
    ));
    let refused = call(
        session
            .tools
            .room_post(Parameters(post_params(ROOM_ID, "and a word")))
            .await,
    );
    assert_eq!(refused.is_error, Some(true));
    assert!(error_text(&refused).contains("budget"));
    assert_eq!(session.home.with(|state| state.sent.len()), 1);
}

// -- threads ------------------------------------------------------------------

#[tokio::test]
async fn room_threads_uses_the_servers_own_summary_when_it_has_one() {
    let session = Session::new().await;
    let root = session.home.with(|state| {
        let mut root = state.add("the big question", HUMAN);
        let ts = root["origin_server_ts"].as_u64().expect("a ts");
        root["unsigned"] = json!({
            "m.relations": {
                "m.thread": {
                    "count": 7,
                    "latest_event": {"origin_server_ts": ts + 60_000},
                }
            }
        });
        state.thread_bundles = vec![root.clone()];
        root
    });
    let threads = payload(&call(
        session
            .tools
            .room_threads(Parameters(ThreadsParams {
                room_id: ROOM_ID.to_owned(),
                limit: 20,
            }))
            .await,
    ));
    let ts = root["origin_server_ts"].as_f64().expect("a ts");
    assert_eq!(
        threads,
        json!([{
            "thread_root": root["event_id"],
            "sender": HUMAN,
            "body": "the big question",
            "ts": ts / 1000.0,
            "reply_count": 7,
            "last_activity_ts": (ts + 60_000.0) / 1000.0,
        }])
    );
}

#[tokio::test]
async fn room_threads_falls_back_to_a_scan_when_the_endpoint_is_not_there() {
    // A homeserver older than Matrix v1.4 has no /threads at all.
    let session = Session::new().await;
    let (quiet, busy, last) = session.home.with(|state| {
        state.threads_status = 404;
        let quiet = state.add("an old question", HUMAN)["event_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let busy = state.add("a newer question", HUMAN)["event_id"]
            .as_str()
            .unwrap()
            .to_owned();
        state.add_threaded("one", OTHER_BOT, "m.notice", &quiet);
        state.add_threaded("two", OTHER_BOT, "m.notice", &busy);
        let last = state.add_threaded("three", HUMAN, "m.text", &busy);
        (quiet, busy, last)
    });
    let threads = payload(&call(
        session
            .tools
            .room_threads(Parameters(ThreadsParams {
                room_id: ROOM_ID.to_owned(),
                limit: 20,
            }))
            .await,
    ));
    let roots: Vec<&str> = threads
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["thread_root"].as_str().unwrap())
        .collect();
    assert_eq!(
        roots,
        vec![busy.as_str(), quiet.as_str()],
        "the busiest-most-recent thread comes first"
    );
    assert_eq!(threads[0]["reply_count"], json!(2));
    assert_eq!(threads[0]["body"], json!("a newer question"));
    assert_eq!(
        threads[0]["last_activity_ts"],
        json!(last["origin_server_ts"].as_f64().unwrap() / 1000.0)
    );
}

// -- waiting ------------------------------------------------------------------

/// A homeserver whose long polls take exactly as long as they were asked to.
async fn waiting_session() -> Session {
    let session = Session::new().await;
    let clock = session.clock.clone();
    session.home.with(|state| state.clock = Some(clock));
    session
}

fn wait_params(timeout_s: f64) -> WaitParams {
    WaitParams {
        room_id: ROOM_ID.to_owned(),
        timeout_s,
        since_ts: None,
    }
}

#[tokio::test]
async fn room_wait_returns_nothing_when_nobody_speaks() {
    let session = waiting_session().await;
    let heard = payload(&call(
        session.tools.room_wait(Parameters(wait_params(30.0))).await,
    ));
    assert_eq!(heard, json!([]));
    let waited: u64 = session.home.with(|state| state.syncs.iter().sum());
    assert_eq!(
        waited, 30_000,
        "it waited {waited} ms, not the 30 s it was given"
    );
}

#[tokio::test]
async fn room_wait_caps_a_ridiculous_timeout() {
    let session = waiting_session().await;
    let heard = payload(&call(
        session
            .tools
            .room_wait(Parameters(wait_params(9999.0)))
            .await,
    ));
    assert_eq!(heard, json!([]));
    let waited: u64 = session.home.with(|state| state.syncs.iter().sum());
    assert_eq!(
        waited, MAX_WAIT_MS,
        "it waited {waited} ms, past the {MAX_WAIT_S} s cap"
    );
}

#[tokio::test]
async fn room_wait_returns_what_somebody_else_said() {
    let session = waiting_session().await;
    let spoken = json!({
        "type": "m.room.message",
        "event_id": "$spoken",
        "sender": HUMAN,
        "origin_server_ts": 1_700_000_100_000_u64,
        "room_id": ROOM_ID,
        "content": {"msgtype": "m.text", "body": "are you there?"},
    });
    let mine = json!({
        "type": "m.room.message",
        "event_id": "$mine",
        "sender": ME,
        "origin_server_ts": 1_700_000_100_000_u64,
        "room_id": ROOM_ID,
        "content": {"msgtype": "m.notice", "body": "me"},
    });
    session.home.with(|state| {
        state.sync_queue.push_back(Vec::new());
        state.sync_queue.push_back(vec![mine]);
        state.sync_queue.push_back(vec![spoken]);
    });
    let heard = payload(&call(
        session.tools.room_wait(Parameters(wait_params(60.0))).await,
    ));
    assert_eq!(
        bodies(&heard),
        vec!["are you there?"],
        "my own posts never wake me"
    );
    session.home.with(|state| {
        assert_eq!(state.syncs[0], 0, "the first sync of a wait is a drain");
        assert_eq!(state.receipts, vec!["$spoken".to_owned()]);
    });
}

#[tokio::test]
async fn a_sync_the_homeserver_refuses_stops_the_wait_instead_of_spinning() {
    // A refused sync returns instantly, so "nobody spoke" would be a hot loop.
    let session = waiting_session().await;
    session.home.with(|state| state.sync_refuses = true);
    let refused = call(session.tools.room_wait(Parameters(wait_params(60.0))).await);
    assert_eq!(refused.is_error, Some(true));
    assert!(
        error_text(&refused).contains("sync"),
        "{}",
        error_text(&refused)
    );
    assert_eq!(
        session.home.with(|state| state.syncs.len()),
        1,
        "it must not keep asking a server that said no"
    );
}

#[tokio::test]
async fn room_wait_with_a_since_ts_answers_from_the_drain() {
    // A caller that says how far it has read is not made to wait for old news.
    let session = waiting_session().await;
    let missed = json!({
        "type": "m.room.message",
        "event_id": "$missed",
        "sender": HUMAN,
        "origin_server_ts": 1_700_000_100_000_u64,
        "room_id": ROOM_ID,
        "content": {"msgtype": "m.text", "body": "you missed this"},
    });
    session
        .home
        .with(|state| state.sync_queue.push_back(vec![missed]));
    let mut params = wait_params(60.0);
    params.since_ts = Some(1_700_000_000.0);
    let heard = payload(&call(session.tools.room_wait(Parameters(params)).await));
    assert_eq!(bodies(&heard), vec!["you missed this"]);
    assert_eq!(
        session.home.with(|state| state.syncs.clone()),
        vec![0],
        "it answered from the drain without polling at all"
    );
}

// -- the config a session runs on ---------------------------------------------

#[tokio::test]
async fn a_homeserver_that_is_not_there_answers_instead_of_hanging() {
    // Nothing retries here, so a host that does not resolve is a readable tool
    // error rather than a call that never returns. `.invalid` never resolves,
    // on a network or off one.
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = Arc::new(config(
        &dir,
        "https://matrix.invalid",
        PostAs::Notice,
        PolicyConfig::default(),
    ));
    let api = CommandClient::new(&cfg).expect("a plain HTTP client");
    let rooms = RoomClient::new(cfg, api, FakeClock::new().as_clock()).expect("it builds");
    let tools = AgentRoomServer::new(Arc::new(rooms));
    let started = std::time::Instant::now();
    let refused = call(tools.room_read(Parameters(read_params(ROOM_ID))).await);
    let elapsed = started.elapsed();
    assert_eq!(refused.is_error, Some(true));
    let text = error_text(&refused);
    assert!(text.contains("matrix.invalid"), "{text}");
    assert!(
        elapsed.as_secs() < 20,
        "the tool took {elapsed:?} to say the host is unreachable"
    );
}

#[tokio::test]
async fn a_world_readable_token_stops_the_server_before_it_serves() {
    // The token IS the account. A session that would hand it to anyone who can
    // read the file does not get to start.
    assert!(
        std::env::var(agent_room::config::ALLOW_LOOSE_PERMS_ENV).is_err(),
        "the suite must not run with the loose-permission override set"
    );
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(
        &dir,
        "https://matrix.invalid",
        PostAs::Notice,
        PolicyConfig::default(),
    );
    let token = cfg.access_token_file.clone().expect("a token file");
    std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o644))
        .expect("chmod");
    let exc = agent_room::mcp_server::serve(cfg)
        .await
        .expect_err("a 0644 token must stop the server");
    assert!(format!("{exc}").contains("chmod 600"), "{exc}");
}

#[tokio::test]
async fn the_tool_results_are_json_the_client_can_read() {
    // Every tool answers with structured JSON, not prose about the room.
    let session = Session::new().await;
    session.home.with(|state| state.add("hello", HUMAN));
    let result = call(
        session
            .tools
            .room_read(Parameters(read_params(ROOM_ID)))
            .await,
    );
    assert!(result.structured_content.is_some());
    let blocks = serde_json::to_value(&result.content).expect("content is serialisable");
    for block in blocks.as_array().expect("a list of blocks") {
        let text = block["text"].as_str().expect("a text block");
        serde_json::from_str::<Value>(text).expect("every content block is JSON");
    }
}

// -- room_impulse: a reason to speak later, not a message now ------------------

#[tokio::test]
async fn room_impulse_writes_a_file_and_posts_nothing() {
    // A live session noticing something is not a live session announcing it.
    let session = Session::new().await;
    let result = payload(&call(session.tools.room_impulse(Parameters(
        ImpulseParams {
            room_id: ROOM_ID.to_owned(),
            text: "merged PR #5 in agent-room".to_owned(),
            kind: "git".to_owned(),
        },
    ))));
    assert!(
        session.home.with(|state| state.sent.is_empty()),
        "an impulse must not reach the room"
    );
    assert_eq!(result["room_id"], json!(ROOM_ID));
    assert_eq!(result["kind"], json!("git"));
    let state_dir = session.dir.path().join("state");
    let impulses = read_impulses(&impulse_dir(&state_dir, ROOM_ID), 3600.0);
    assert_eq!(impulses.len(), 1);
    assert_eq!(impulses[0].summary, "merged PR #5 in agent-room");
    assert_eq!(impulses[0].kind, "git");
    assert_eq!(
        result["path"].as_str().expect("a path"),
        impulses[0].path.display().to_string()
    );
    assert_eq!(
        result["expires_in_s"].as_f64(),
        Some(PolicyConfig::default().impulse_ttl_s)
    );
}

#[tokio::test]
async fn an_impulse_carries_the_lifetime_the_config_gave_it() {
    // The lifetime is written INTO the file, because the connector that will
    // read it is a different process with its own clock: an impulse that is
    // still worth saying is one that has not run out yet. A build that used its
    // own number here would keep a five-minute thought alive for six hours.
    let policy = PolicyConfig {
        impulse_ttl_s: 90.0,
        ..PolicyConfig::default()
    };
    let session = Session::with(PostAs::Notice, policy, None).await;
    let result = payload(&call(session.tools.room_impulse(Parameters(
        ImpulseParams {
            room_id: ROOM_ID.to_owned(),
            text: "the render finished".to_owned(),
            kind: "render".to_owned(),
        },
    ))));
    assert_eq!(result["expires_in_s"].as_f64(), Some(90.0));

    let state_dir = session.dir.path().join("state");
    let impulses = read_impulses(&impulse_dir(&state_dir, ROOM_ID), 3600.0);
    assert_eq!(impulses.len(), 1);
    let written = &impulses[0];
    assert!(
        (written.ttl_s - 90.0).abs() < f64::EPSILON,
        "the file carries a {} s lifetime, not the configured 90",
        written.ttl_s
    );
    assert!(
        !written.expired(written.ts + 89.0),
        "it expired inside its own lifetime"
    );
    assert!(
        written.expired(written.ts + 91.0),
        "91 s into a 90 s lifetime it is still worth saying: the shipped six \
         hours is what is being used"
    );
}

#[tokio::test]
async fn room_impulse_refuses_an_empty_line() {
    let session = Session::new().await;
    let refused = call(session.tools.room_impulse(Parameters(ImpulseParams {
        room_id: ROOM_ID.to_owned(),
        text: "   ".to_owned(),
        kind: "note".to_owned(),
    })));
    assert_eq!(refused.is_error, Some(true));
    assert!(!impulse_dir(&session.dir.path().join("state"), ROOM_ID).exists());
}
