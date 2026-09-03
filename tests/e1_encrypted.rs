//! E1: the agent holds up its end of an ENCRYPTED room.
//!
//! The Python live harness cannot drive this one. Its `human` client is
//! matrix-nio without a crypto store, so it can neither encrypt a mention nor
//! decrypt an answer; a gate written there could only assert that *something*
//! arrived, which proves nothing about encryption. So E1 is driven from Rust,
//! with matrix-sdk playing the human account, and the assertion is the real one:
//! the reply arrives as `m.room.encrypted`, this client decrypts it, and the
//! plaintext body contains `echo: `.
//!
//! Everything else is the shipped path: a real private room with
//! `m.room.encryption` on it, a real `agent-room run` subprocess with the echo
//! brain, and a real mention.
//!
//! Run with: `AGENT_ROOM_LIVE=1 cargo test --test e1_encrypted -- --nocapture`
//!
//! **The store is deliberately persistent** (`target/e1/`). An access token
//! binds a client to the DEVICE that token belongs to, and the homeserver keeps
//! that device's published one-time keys. Throw the store away and the device's
//! keys on the server are ones nobody can prove they own any more: the next
//! upload is refused (`One time key ... already exists`) and nobody can start
//! an olm session with it. Keeping the store is the same thing a real
//! deployment does with its `state_dir`.
//!
//! That is also why the two journeys here take a LOCK instead of a store each.
//! One token is one device, one device is one store, and two of these tests
//! running at once (cargo's default) opened the same sqlite twice: the second
//! failed with `database is locked` on 2026-09-03, the first time both were run
//! after R3. Giving each journey its own store would be worse, not better - two
//! stores for one device is exactly the wedge the paragraph above describes.

#![forbid(unsafe_code)]
#![allow(clippy::items_after_statements)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::api::client::room::create_room::v3::{
    Request as CreateRoomRequest, RoomPreset,
};
use matrix_sdk::ruma::events::InitialStateEvent;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UserId};
use matrix_sdk::{Client, RoomMemberships};
use serde_json::{Value, json};

/// Plays the human, with a real crypto store of its own.
/// The account that plays the human, from the live env.
fn reader() -> String {
    live_var("AGENT_ROOM_LIVE_HUMAN")
}
/// The agent under test. Deliberately NOT one of the accounts the G1-G4
/// journeys use: those get a throwaway state directory per test, which is
/// exactly the thing that wedges a device's one-time keys (see the module doc).
/// The account whose connector this gate runs, from the live env. Its device is
/// wedged like the others' (fresh store per run), which E1 tolerates by keeping
/// its store between the two journeys of one run.
fn e1_bot() -> String {
    live_var("AGENT_ROOM_LIVE_E1_BOT")
}

fn live() -> bool {
    std::env::var("AGENT_ROOM_LIVE").as_deref() == Ok("1")
}

/// `~/.config/agent-room/live.env` (or `AGENT_ROOM_LIVE_ENV`), parsed once.
///
/// The same file the pytest harness reads, for the same reason: which
/// homeserver the live gates run against, and which accounts they borrow, are
/// deployment details that live OUTSIDE the repository. It is READ rather than
/// exported - `set_var` is unsafe in this edition, and nothing in this repo is.
static DOTENV: std::sync::LazyLock<BTreeMap<String, String>> = std::sync::LazyLock::new(|| {
    let path = std::env::var("AGENT_ROOM_LIVE_ENV")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".config/agent-room/live.env"))
        })
        .unwrap_or_default();
    let Ok(text) = fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| {
            (
                name.trim().to_owned(),
                value.trim().trim_matches(['"', '\'']).to_owned(),
            )
        })
        .collect()
});

/// A live setting, or a panic naming the file it belongs in.
///
/// The environment wins over the file, so one value can be overridden for a
/// single run.
fn live_var(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| DOTENV.get(name).cloned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            panic!("{name} is not set - see tests/live/live.env.example (goes in ~/.config/agent-room/live.env)")
        })
}

fn homeserver() -> String {
    live_var("AGENT_ROOM_LIVE_HOMESERVER")
}

fn server_name() -> String {
    live_var("AGENT_ROOM_LIVE_SERVER_NAME")
}

fn tokens() -> BTreeMap<String, String> {
    let path = PathBuf::from(live_var("AGENT_ROOM_TOKENS_FILE"));
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|exc| panic!("cannot read {}: {exc}", path.display()));
    serde_json::from_str(&raw).expect("the tokens file is a JSON object of name -> token")
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn binary() -> PathBuf {
    std::env::var("AGENT_ROOM_BIN")
        .map_or_else(|_| repo().join("target/release/agent-room"), PathBuf::from)
}

/// A directory that SURVIVES between runs: see the module doc.
fn persistent(name: &str) -> PathBuf {
    let path = repo().join("target/e1").join(name);
    fs::create_dir_all(&path).expect("target/ is writable");
    path
}

async fn client_for(name: &str, token: &str, store: &Path) -> Client {
    let client = Client::builder()
        .homeserver_url(homeserver())
        .sqlite_store(store, None)
        .build()
        .await
        .expect("the homeserver answers");
    let http = reqwest::Client::new();
    let (user_id, device_id) = agent_room::matrix::whoami(&http, &homeserver(), token)
        .await
        .expect("whoami reaches the homeserver")
        .unwrap_or_else(|| panic!("the token for {name} was refused"));
    let session = matrix_sdk::authentication::matrix::MatrixSession {
        meta: matrix_sdk::SessionMeta {
            user_id: UserId::parse(&user_id).expect("a user id"),
            device_id: device_id.expect("the homeserver names the token's device"),
        },
        tokens: matrix_sdk::SessionTokens {
            access_token: token.to_owned(),
            refresh_token: None,
        },
    };
    client
        .matrix_auth()
        .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
        .await
        .expect("the session restores");
    client
}

/// The connector under test, as a real process.
struct Bot {
    child: Child,
    log: PathBuf,
}

impl Bot {
    fn start(config: &Path, log: PathBuf) -> Self {
        let handle = fs::File::create(&log).expect("the log is writable");
        let child = Command::new(binary())
            .arg("run")
            .arg("--config")
            .arg(config)
            .stdout(Stdio::from(handle.try_clone().expect("dup")))
            .stderr(Stdio::from(handle))
            .spawn()
            .unwrap_or_else(|exc| panic!("cannot start {}: {exc}", binary().display()));
        Self { child, log }
    }

    fn text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if self.text().contains("watching") {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("the connector exited early ({status}):\n{}", self.text());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!("the connector never became ready:\n{}", self.text());
    }

    fn stop(&mut self) {
        let _ignored = self.child.kill();
        let _ignored = self.child.wait();
    }
}

fn write_config(home: &Path, token: &str, room_id: &RoomId, state_dir: &Path) -> PathBuf {
    fs::create_dir_all(home).expect("writable");
    let token_path = home.join("access");
    fs::write(&token_path, token).expect("writable");
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).expect("chmod");
    let config = json!({
        "homeserver": homeserver(),
        "user_id": format!("@{}:{}", e1_bot(), server_name()),
        "access_token_file": token_path.to_string_lossy(),
        "rooms": [room_id.as_str()],
        "state_dir": state_dir.to_string_lossy(),
        "brain": { "kind": "echo" },
        "policy": {
            "bot_user_ids": [format!("@{}:{}", e1_bot(), server_name())],
            "bot_localpart_patterns": [],
        },
    });
    // The config format is YAML, and JSON is YAML: no serialiser needed for a
    // file this small, and what the connector parses is exactly what is here.
    let path = home.join("config.yaml");
    fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialises"),
    )
    .expect("writable");
    path
}

/// A fresh private room, encrypted or not.
async fn fresh_room(reader: &Client, bot_user: &str, encrypted: bool) -> matrix_sdk::Room {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs();
    let mut request = CreateRoomRequest::new();
    request.name = Some(format!("agent-room E1 {stamp}"));
    request.topic = Some("agent-room E1: created and forgotten by the test suite".to_owned());
    request.preset = Some(RoomPreset::PrivateChat);
    request.invite = vec![UserId::parse(bot_user).expect("a user id")];
    request.is_direct = false;
    if encrypted {
        request.initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];
    }
    let room = reader
        .create_room(request)
        .await
        .expect("the room is created");
    assert_eq!(
        room.latest_encryption_state()
            .await
            .expect("the encryption state is known")
            .is_encrypted(),
        encrypted,
        "the room the gate created is not the room the gate asked for"
    );
    room
}

/// One run of the journey: post a mention, come back with what was said and
/// whether the wire carried it encrypted.
/// One journey at a time: see the module doc. Both tests share the accounts,
/// so they share the stores, and sqlite is not shared.
static ONE_AT_A_TIME: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn one_journey(encrypted: bool) -> Reply {
    let _serialised = ONE_AT_A_TIME.lock().await;
    let tokens = tokens();
    let bot_user = format!("@{}:{}", e1_bot(), server_name());
    let reader = client_for(
        &reader(),
        tokens.get(&reader()).expect("a token for the reader"),
        &persistent("reader.store"),
    )
    .await;
    // One sync before anything else, so the client knows its own device and has
    // uploaded its keys before it is asked to encrypt anything.
    reader
        .sync_once(SyncSettings::new().timeout(Duration::from_secs(0)))
        .await
        .expect("the first sync answers");

    let room = fresh_room(&reader, &bot_user, encrypted).await;
    let room_id: OwnedRoomId = room.room_id().to_owned();
    let home = repo().join("target/e1/bot");
    let config = write_config(
        &home,
        tokens.get(&e1_bot()).expect("a token for the bot"),
        &room_id,
        &persistent("bot-state"),
    );
    let mut bot = Bot::start(&config, home.join("connector.log"));

    let outcome = async {
        bot.wait_ready();
        wait_for_join(&reader, &room, &bot_user).await;
        let sent = room
            .send_raw(
                "m.room.message",
                json!({
                    "msgtype": "m.text",
                    "body": format!("{bot_user} hello from a room"),
                    "m.mentions": { "user_ids": [bot_user] },
                }),
            )
            .await
            .expect("the mention is sent");
        assert_eq!(
            sent.encryption_info.is_some(),
            encrypted,
            "the gate's own message did not go out the way the room says it should"
        );
        wait_for_reply(&reader, &room_id, &bot_user, 60)
            .await
            .unwrap_or_else(|| panic!("no reply from {bot_user}:\n{}", bot.text()))
    }
    .await;

    bot.stop();
    let _ignored = room.leave().await;
    let _ignored = room.forget().await;
    outcome
}

/// Wait until the agent is actually IN the room: an encrypted message is only
/// ever encrypted for the devices that were there when it was sent.
async fn wait_for_join(reader: &Client, room: &matrix_sdk::Room, bot_user: &str) {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        reader
            .sync_once(SyncSettings::new().timeout(Duration::from_secs(2)))
            .await
            .expect("the sync answers");
        let members = room
            .members(RoomMemberships::JOIN)
            .await
            .expect("the member list answers");
        if members
            .iter()
            .any(|member| member.user_id().as_str() == bot_user)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{bot_user} never joined the room"
        );
    }
}

/// One decrypted message from `sender`, or None.
struct Reply {
    body: String,
    was_encrypted: bool,
}

async fn wait_for_reply(
    client: &Client,
    room_id: &RoomId,
    sender: &str,
    seconds: u64,
) -> Option<Reply> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let response = client
            .sync_once(SyncSettings::new().timeout(Duration::from_secs(5)))
            .await
            .expect("the sync answers");
        let Some(update) = response.rooms.joined.get(room_id) else {
            continue;
        };
        for event in &update.timeline.events {
            let was_encrypted = event.encryption_info().is_some();
            let Ok(source) = serde_json::from_str::<Value>(event.raw().json().get()) else {
                continue;
            };
            if source.get("sender").and_then(Value::as_str) != Some(sender) {
                continue;
            }
            if source.get("type").and_then(Value::as_str) != Some("m.room.message") {
                continue;
            }
            let body = source
                .pointer("/content/body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            return Some(Reply {
                body,
                was_encrypted,
            });
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn e1_the_agent_answers_in_an_encrypted_room_and_the_room_can_read_it() {
    if !live() {
        eprintln!("E1 skipped: set AGENT_ROOM_LIVE=1 to run it against the real homeserver");
        return;
    }
    let reply = one_journey(true).await;
    assert!(
        reply.was_encrypted,
        "the reply arrived unencrypted in an encrypted room: {}",
        reply.body
    );
    assert!(
        reply.body.contains("echo: "),
        "the decrypted reply does not read like an answer: {:?}",
        reply.body
    );
    println!("E1: decrypted reply {:?}", reply.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn e1_has_teeth_because_the_same_journey_in_a_plain_room_is_not_encrypted() {
    // The negative control for the gate above. `was_encrypted` is read off the
    // SDK's decryption info, and an assertion that is true whatever happens is
    // not an assertion: the identical journey in a room with no
    // `m.room.encryption` must come back with it FALSE, or E1 proves nothing
    // about encryption at all.
    if !live() {
        eprintln!("skipped: set AGENT_ROOM_LIVE=1 to run it against the real homeserver");
        return;
    }
    let reply = one_journey(false).await;
    assert!(
        !reply.was_encrypted,
        "a plain room reported an encrypted reply, so E1's assertion means nothing"
    );
    assert!(reply.body.contains("echo: "), "{:?}", reply.body);
}
