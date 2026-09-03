//! A homeserver small enough to read, on a real socket.
//!
//! The reference's unit tests fake nio's client object. Here the fake is an
//! HTTP server on `127.0.0.1:0`, because what is under test in `cs_api` IS the
//! HTTP: which endpoint, which query string, and - the one that matters - that
//! the access token travels in the `Authorization` header and never in the URL.
//! A trait-object fake would prove none of those.
//!
//! It answers the endpoints `mcp`, `init` and `doctor` actually call, and every
//! knob a test needs (a `/threads` that 404s, a `/sync` that refuses, a
//! homeserver that stops answering mid-run) is a field on [`State`].

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A clock the tests move by hand.
#[derive(Clone)]
pub struct FakeClock(Arc<Mutex<f64>>);

impl FakeClock {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(1_700_000_000.0)))
    }

    #[must_use]
    pub fn now(&self) -> f64 {
        *self.0.lock().expect("the clock is never poisoned")
    }

    pub fn advance(&self, seconds: f64) {
        *self.0.lock().expect("the clock is never poisoned") += seconds;
    }

    #[must_use]
    pub fn as_clock(&self) -> agent_room::ledger::Clock {
        let inner = Arc::clone(&self.0);
        Arc::new(move || *inner.lock().expect("the clock is never poisoned"))
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

/// One request the fake was asked for.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub params: BTreeMap<String, String>,
    pub body: Value,
}

/// Everything one fake homeserver knows and can be told to do.
pub struct State {
    pub me: String,
    pub room_id: String,
    /// The room, oldest first, exactly as the homeserver would serialise it.
    pub events: Vec<Value>,
    /// `(room, event type, content)` for everything that was sent.
    pub sent: Vec<(String, String, Value)>,
    pub requests: Vec<Seen>,
    /// The `timeout` of every `/sync` that was asked for.
    pub syncs: Vec<u64>,
    /// What each successive `/sync` returns in the timeline.
    pub sync_queue: VecDeque<Vec<Value>>,
    pub receipts: Vec<String>,
    pub threads_status: u16,
    pub thread_bundles: Vec<Value>,
    pub members: BTreeMap<String, String>,
    pub name: Option<String>,
    pub receipts_fail: bool,
    /// A `/sync` the homeserver refuses - a dead token, say.
    pub sync_refuses: bool,
    /// Advance this clock by the `/sync` timeout, so a wait's own arithmetic is
    /// what the test measures rather than wall time.
    pub clock: Option<FakeClock>,
    pub versions: Vec<String>,
    pub versions_status: u16,
    /// The account the token belongs to, and whether it is accepted at all.
    pub whoami_user: String,
    pub whoami_error: Option<String>,
    pub joined_rooms: Vec<String>,
    pub joined_rooms_error: Option<String>,
    /// Which `/sync` attempt (1-based) first carries the invitation. 0 = never.
    pub invited_from_sync: usize,
    pub invited: Vec<String>,
    pub aliases: BTreeMap<String, String>,
    /// Stop answering `/joined_rooms` altogether, mid-run.
    pub rooms_go_away: bool,
    pub login_token: String,
    pub login_error: Option<String>,
    pub passwords: Vec<String>,
    pub display_names: Vec<String>,
    pub display_name_error: Option<String>,
    counter: u64,
}

impl State {
    fn new(room_id: &str, me: &str) -> Self {
        Self {
            me: me.to_owned(),
            room_id: room_id.to_owned(),
            events: Vec::new(),
            sent: Vec::new(),
            requests: Vec::new(),
            syncs: Vec::new(),
            sync_queue: VecDeque::new(),
            receipts: Vec::new(),
            threads_status: 200,
            thread_bundles: Vec::new(),
            members: BTreeMap::new(),
            name: Some("the room".to_owned()),
            receipts_fail: false,
            sync_refuses: false,
            clock: None,
            versions: vec!["v1.1".to_owned(), "v1.13".to_owned()],
            versions_status: 200,
            whoami_user: me.to_owned(),
            whoami_error: None,
            joined_rooms: Vec::new(),
            joined_rooms_error: None,
            invited_from_sync: 0,
            invited: Vec::new(),
            aliases: BTreeMap::new(),
            rooms_go_away: false,
            login_token: "syt_from_login".to_owned(),
            login_error: None,
            passwords: Vec::new(),
            display_names: Vec::new(),
            display_name_error: None,
            counter: 0,
        }
    }

    /// Add one event to the room. Returns it as the homeserver would send it.
    pub fn add(&mut self, body: &str, sender: &str) -> Value {
        self.add_full(body, sender, "m.text", None, "m.room.message")
    }

    pub fn add_notice(&mut self, body: &str, sender: &str) -> Value {
        self.add_full(body, sender, "m.notice", None, "m.room.message")
    }

    pub fn add_threaded(&mut self, body: &str, sender: &str, msgtype: &str, root: &str) -> Value {
        self.add_full(body, sender, msgtype, Some(root), "m.room.message")
    }

    pub fn add_state(&mut self, event_type: &str) -> Value {
        self.add_full(
            "joined",
            "@somebody:example.com",
            "m.text",
            None,
            event_type,
        )
    }

    pub fn add_full(
        &mut self,
        body: &str,
        sender: &str,
        msgtype: &str,
        thread_root: Option<&str>,
        event_type: &str,
    ) -> Value {
        self.counter += 1;
        let mut content = json!({"msgtype": msgtype, "body": body});
        if let Some(root) = thread_root {
            content["m.relates_to"] = json!({
                "rel_type": "m.thread",
                "event_id": root,
                "is_falling_back": true,
                "m.in_reply_to": {"event_id": root},
            });
        }
        let event = json!({
            "type": event_type,
            "event_id": format!("$e{}", self.counter),
            "sender": sender,
            "origin_server_ts": 1_700_000_000_000_u64 + self.counter * 1000,
            "room_id": self.room_id,
            "content": content,
        });
        self.events.push(event.clone());
        event
    }

    /// Every `/messages` page that was asked for, in order.
    #[must_use]
    pub fn message_pages(&self) -> Vec<BTreeMap<String, String>> {
        self.requests
            .iter()
            .filter(|seen| seen.path.ends_with("/messages"))
            .map(|seen| seen.params.clone())
            .collect()
    }

    #[must_use]
    pub fn sent_bodies(&self) -> Vec<String> {
        self.sent
            .iter()
            .map(|(_room, _type, content)| {
                content
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }
}

/// A running fake homeserver. Dropping it stops the listener.
pub struct FakeHomeserver {
    pub base_url: String,
    pub state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeHomeserver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeHomeserver {
    /// Start one on a free port and answer until dropped.
    pub async fn start(room_id: &str, me: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        let state = Arc::new(Mutex::new(State::new(room_id, me)));
        let served = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _peer)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&served);
                tokio::spawn(async move { serve_connection(socket, state).await });
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            state,
            task,
        }
    }

    /// Do something to the fake's state, under its lock.
    pub fn with<T>(&self, edit: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.state.lock().expect("the fake is never poisoned");
        edit(&mut state)
    }
}

async fn serve_connection(mut socket: TcpStream, state: Arc<Mutex<State>>) {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let Some((head_len, body_len)) = read_head(&mut socket, &mut buffer).await else {
            return;
        };
        while buffer.len() < head_len + body_len {
            let mut chunk = [0_u8; 4096];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let head = String::from_utf8_lossy(&buffer[..head_len]).into_owned();
        let body = String::from_utf8_lossy(&buffer[head_len..head_len + body_len]).into_owned();
        buffer.drain(..head_len + body_len);

        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_owned();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let target = parts.next().unwrap_or_default().to_owned();
        let authorization = head
            .lines()
            .find_map(|line| line.strip_prefix("authorization: ").map(ToOwned::to_owned))
            .or_else(|| {
                head.lines()
                    .find_map(|line| line.strip_prefix("Authorization: ").map(ToOwned::to_owned))
            });

        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let path = decode(path);
        let params = parse_query(query);
        let sent: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

        let answer = {
            let mut room = state.lock().expect("the fake is never poisoned");
            room.requests.push(Seen {
                method: method.clone(),
                path: path.clone(),
                params: params.clone(),
                body: sent.clone(),
            });
            route(
                &mut room,
                &method,
                &path,
                &params,
                &sent,
                authorization.as_deref(),
            )
        };
        let Some((status, body)) = answer else {
            // The homeserver stopped answering: drop the connection with no
            // response at all, which is what the client has to survive.
            return;
        };
        let rendered = body.to_string();
        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            reason(status),
            rendered.len()
        );
        if socket.write_all(response.as_bytes()).await.is_err()
            || socket.write_all(rendered.as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Read one request head, returning `(head length, content length)`.
async fn read_head(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<(usize, usize)> {
    loop {
        if let Some(index) = find(buffer, b"\r\n\r\n") {
            let head_len = index + 4;
            let head = String::from_utf8_lossy(&buffer[..head_len]).to_lowercase();
            let body_len = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            return Some((head_len, body_len));
        }
        let mut chunk = [0_u8; 4096];
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

/// One request, answered. `None` means "say nothing at all".
#[allow(clippy::too_many_lines)]
fn route(
    state: &mut State,
    method: &str,
    path: &str,
    params: &BTreeMap<String, String>,
    body: &Value,
    authorization: Option<&str>,
) -> Option<(u16, Value)> {
    if path == "/_matrix/client/versions" {
        return Some((state.versions_status, json!({"versions": state.versions})));
    }
    if path == "/_matrix/client/v3/login" {
        let password = body
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        state.passwords.push(password);
        if let Some(message) = state.login_error.clone() {
            return Some((403, json!({"errcode": "M_FORBIDDEN", "error": message})));
        }
        return Some((
            200,
            json!({
                "access_token": state.login_token,
                "user_id": state.me,
                "device_id": "DEV",
            }),
        ));
    }

    // Everything below needs the token, and it must be in the header.
    assert!(
        authorization.is_some_and(|value| value.starts_with("Bearer ")),
        "the token must travel in the Authorization header, never in the URL: {method} {path}"
    );

    if path == "/_matrix/client/v3/account/whoami" {
        if let Some(message) = state.whoami_error.clone() {
            return Some((401, json!({"errcode": "M_UNKNOWN_TOKEN", "error": message})));
        }
        return Some((
            200,
            json!({"user_id": state.whoami_user, "device_id": "DEV"}),
        ));
    }
    if path == "/_matrix/client/v3/keys/query" {
        // A fake account has published no device keys: a store may publish.
        return Some((200, json!({"device_keys": {}, "failures": {}})));
    }
    if path == "/_matrix/client/v3/joined_rooms" {
        if state.rooms_go_away {
            return None;
        }
        if let Some(message) = state.joined_rooms_error.clone() {
            return Some((
                429,
                json!({"errcode": "M_LIMIT_EXCEEDED", "error": message}),
            ));
        }
        return Some((200, json!({"joined_rooms": state.joined_rooms})));
    }
    if path == "/_matrix/client/v3/sync" {
        return Some(sync(state, params));
    }
    if let Some(alias) = path.strip_prefix("/_matrix/client/v3/directory/room/") {
        return Some(match state.aliases.get(alias) {
            Some(room_id) => (200, json!({ "room_id": room_id })),
            None => (
                404,
                json!({"errcode": "M_NOT_FOUND", "error": "Room alias not found"}),
            ),
        });
    }
    if let Some(rest) = path.strip_prefix("/_matrix/client/v3/profile/")
        && rest.ends_with("/displayname")
    {
        {
            let name = body
                .get("displayname")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(message) = state.display_name_error.clone() {
                return Some((403, json!({"errcode": "M_FORBIDDEN", "error": message})));
            }
            state.display_names.push(name);
            return Some((200, json!({})));
        }
    }
    if path.starts_with("/_matrix/client/v3/join/") {
        return Some((200, json!({ "room_id": state.room_id })));
    }
    if path.ends_with("/joined_members") {
        let joined: serde_json::Map<String, Value> = state
            .members
            .iter()
            .map(|(user_id, name)| (user_id.clone(), json!({ "display_name": name })))
            .collect();
        return Some((200, json!({ "joined": joined })));
    }
    if path.ends_with("/state/m.room.name") {
        return Some(match &state.name {
            Some(name) => (200, json!({ "name": name })),
            None => (404, json!({"errcode": "M_NOT_FOUND"})),
        });
    }
    if path.ends_with("/read_markers") {
        if state.receipts_fail {
            return Some((500, json!({"errcode": "M_UNKNOWN"})));
        }
        if let Some(event_id) = body.get("m.read").and_then(Value::as_str) {
            state.receipts.push(event_id.to_owned());
        }
        return Some((200, json!({})));
    }
    if path.contains("/receipt/m.read/") {
        return Some(if state.receipts_fail {
            (500, json!({"errcode": "M_UNKNOWN"}))
        } else {
            (200, json!({}))
        });
    }
    if path.ends_with("/messages") {
        return Some(messages(state, params));
    }
    if path.contains("/relations/") {
        let root = path.split('/').nth(7).unwrap_or_default().to_owned();
        let related: Vec<Value> = state
            .events
            .iter()
            .filter(|event| thread_root_of(event).as_deref() == Some(root.as_str()))
            .rev()
            .cloned()
            .collect();
        return Some((200, json!({ "chunk": related })));
    }
    if path.ends_with("/threads") {
        if state.threads_status != 200 {
            return Some((
                state.threads_status,
                json!({"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"}),
            ));
        }
        return Some((200, json!({ "chunk": state.thread_bundles })));
    }
    if let Some(index) = path.find("/event/") {
        let wanted = &path[index + "/event/".len()..];
        return Some(
            match state
                .events
                .iter()
                .find(|event| event["event_id"] == json!(wanted))
            {
                Some(event) => (200, event.clone()),
                None => (404, json!({"errcode": "M_NOT_FOUND"})),
            },
        );
    }
    if let Some(index) = path.find("/send/") {
        let rest = &path[index + "/send/".len()..];
        let event_type = rest.split('/').next().unwrap_or_default().to_owned();
        state.counter += 1;
        let event_id = format!("$sent{}", state.counter);
        state
            .sent
            .push((state.room_id.clone(), event_type.clone(), body.clone()));
        if event_type == "m.room.message" {
            let sender = state.me.clone();
            state.counter += 1;
            let event = json!({
                "type": "m.room.message",
                "event_id": event_id,
                "sender": sender,
                "origin_server_ts": 1_700_000_000_000_u64 + state.counter * 1000,
                "room_id": state.room_id,
                "content": body.clone(),
            });
            state.events.push(event);
        }
        return Some((200, json!({ "event_id": event_id })));
    }
    panic!("the fake homeserver was asked for {method} {path}");
}

/// `/messages`, newest first, with `from` as an index into that ordering.
fn messages(state: &State, params: &BTreeMap<String, String>) -> (u16, Value) {
    let limit: usize = params
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let start: usize = params
        .get("from")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let newest_first: Vec<Value> = state.events.iter().rev().cloned().collect();
    let page: Vec<Value> = newest_first
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect();
    let mut body = json!({ "chunk": page });
    if start + limit < newest_first.len() {
        body["end"] = json!((start + limit).to_string());
    }
    (200, body)
}

fn sync(state: &mut State, params: &BTreeMap<String, String>) -> (u16, Value) {
    let timeout: u64 = params
        .get("timeout")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    state.syncs.push(timeout);
    if state.sync_refuses {
        return (
            401,
            json!({"errcode": "M_UNKNOWN_TOKEN", "error": "token expired"}),
        );
    }
    if let Some(clock) = &state.clock {
        #[allow(clippy::cast_precision_loss)]
        clock.advance(timeout as f64 / 1000.0);
    }
    let batch = state.sync_queue.pop_front().unwrap_or_default();
    let attempt = state.syncs.len();
    let mut invite = serde_json::Map::new();
    if state.invited_from_sync > 0 && attempt >= state.invited_from_sync {
        for room_id in &state.invited {
            invite.insert(room_id.clone(), json!({}));
        }
    }
    (
        200,
        json!({
            "next_batch": format!("s{attempt}"),
            "rooms": {
                "join": {state.room_id.clone(): {"timeline": {"events": batch}}},
                "invite": invite,
            },
        }),
    )
}

fn thread_root_of(event: &Value) -> Option<String> {
    let relation = event.get("content")?.get("m.relates_to")?;
    if relation.get("rel_type")? != &json!("m.thread") {
        return None;
    }
    relation.get("event_id")?.as_str().map(ToOwned::to_owned)
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((decode(key), decode(value)))
        })
        .collect()
}

/// Percent-decoding, plus `+` for a space the way a query string means it.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
