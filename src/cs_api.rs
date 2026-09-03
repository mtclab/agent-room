//! The Client-Server API, spoken directly, for the commands a person waits on.
//!
//! `run` drives the matrix-sdk client: it needs a store, a crypto identity and
//! a sync loop that reconnects all night. `mcp`, `init` and `doctor` need none
//! of that and must not pay for it - a `doctor` that builds a sqlite crypto
//! store before it can say "your token is 0644" is the wrong tool - so they
//! talk to the homeserver over plain HTTP through the same [`reqwest::Client`]
//! the mTLS config builds.
//!
//! This was the Python's choice too, for the same reasons it gave: nio's
//! typed helpers put the access token in the QUERY STRING (it ends up in access
//! logs, in exception text and in every proxy in between) and hide the HTTP
//! status behind a typed error - and `/threads` is Matrix v1.4, whose 404 is
//! exactly what has to be recognised in order to fall back. Here the token
//! travels in the `Authorization` header and the status is a number.
//!
//! Nothing retries. Retrying all night is right for a daemon and wrong for a
//! tool call somebody is sitting and waiting on: pointed at a homeserver that
//! is not there, every call here fails at once and says why.

use std::sync::Mutex;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{info, warn};

use crate::config::{Config, read_secret_file, write_secret_file};

pub const CS_V3: &str = "/_matrix/client/v3";
pub const CS_V1: &str = "/_matrix/client/v1";
/// What the room calls this device after a password login.
pub const DEVICE_NAME: &str = "agent-room";

/// A request somebody is waiting on gives up rather than retrying all night.
pub const REQUEST_TIMEOUT_S: u64 = 30;

/// `quote(value, safe="")`: everything but the RFC 3986 unreserved characters.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode one path segment (a room id, an event id, an alias).
#[must_use]
pub fn quote(value: &str) -> String {
    utf8_percent_encode(value, PATH_SEGMENT).to_string()
}

/// Why a Client-Server call did not do what was asked.
///
/// The two halves are kept apart because a person is told different things
/// about them: a homeserver that answered and said no is a credential or a
/// configuration problem, and one that never answered is a network problem.
#[derive(Debug, Error)]
pub enum CsError {
    /// The homeserver answered, and the answer was no.
    #[error("{0}")]
    Refused(String),
    /// The homeserver could not be reached at all. Carries the cause only, so
    /// each caller can word it the way its own output needs.
    #[error("{0}")]
    Unreachable(String),
}

impl CsError {
    fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }
}

pub type Result<T> = std::result::Result<T, CsError>;

/// One homeserver, one access token, no retries.
#[derive(Debug)]
pub struct CommandClient {
    http: reqwest::Client,
    homeserver: String,
    token: Mutex<String>,
}

impl CommandClient {
    /// A client for `cfg`'s homeserver, carrying the mTLS identity when one is
    /// configured.
    ///
    /// # Errors
    /// When the TLS material cannot be loaded - which is the connection that
    /// will not happen, so the caller reports it as such.
    pub fn new(cfg: &Config) -> Result<Self> {
        let http = cfg
            .tls
            .build_client()
            .map_err(|exc| CsError::refused(exc.to_string()))?;
        Ok(Self::with_http(http, &cfg.homeserver))
    }

    #[must_use]
    pub fn with_http(http: reqwest::Client, homeserver: &str) -> Self {
        Self {
            http,
            homeserver: homeserver.trim_end_matches('/').to_owned(),
            token: Mutex::new(String::new()),
        }
    }

    #[must_use]
    pub fn homeserver(&self) -> &str {
        &self.homeserver
    }

    /// The token this client presents. Never logged, only sent.
    /// The token currently carried (empty before `set_token`). Doctor uses it
    /// to open the store for inspection.
    #[must_use]
    pub fn token(&self) -> String {
        self.token
            .lock()
            .map(|token| token.clone())
            .unwrap_or_default()
    }

    pub fn set_token(&self, token: &str) {
        if let Ok(mut held) = self.token.lock() {
            token.clone_into(&mut held);
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.homeserver)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        authenticated: bool,
    ) -> Result<(u16, Value)> {
        let request = if authenticated {
            request.bearer_auth(self.token())
        } else {
            request
        };
        let response = request
            .send()
            .await
            .map_err(|exc| CsError::Unreachable(exc.to_string()))?;
        let status = response.status().as_u16();
        // A body that is not JSON is not a failure of its own: the status is
        // what every caller here branches on, and a homeserver sending junk
        // must not become a panic in a diagnostic tool.
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    /// GET one endpoint. Returns `(status, parsed body)`.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<(u16, Value)> {
        let request = self
            .http
            .get(self.url(path))
            .query(params)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_S));
        self.send(request, true).await
    }

    /// POST a JSON body.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn post(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        let request = self
            .http
            .post(self.url(path))
            .json(body)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_S));
        self.send(request, true).await
    }

    /// PUT a JSON body.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn put(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        let request = self
            .http
            .put(self.url(path))
            .json(body)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_S));
        self.send(request, true).await
    }

    /// The `chunk` array of a paginated endpoint, and the status that came with
    /// it. A status other than 200 is not an error here: `/threads` answering
    /// 404 is a homeserver that predates the endpoint, and the caller falls
    /// back rather than failing.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn chunk(&self, path: &str, params: &[(&str, String)]) -> Result<(u16, Vec<Value>)> {
        let (status, body) = self.get(path, params).await?;
        if status != 200 {
            return Ok((status, Vec::new()));
        }
        let chunk = body
            .get("chunk")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item.is_object())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok((status, chunk))
    }

    // -- the endpoints ---------------------------------------------------

    /// `/_matrix/client/versions`: the one endpoint that needs no account.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn versions(&self) -> Result<(u16, Value)> {
        let request = self
            .http
            .get(self.url("/_matrix/client/versions"))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_S));
        self.send(request, false).await
    }

    /// Put `token` on this client and ask the homeserver who it belongs to.
    ///
    /// Returns the user id the server recognises, or None when it refuses it.
    /// The token itself is never logged: a rejection prints the server's
    /// message only.
    ///
    /// # Errors
    /// When the homeserver cannot be reached - a different thing from a token
    /// it does not like, and the callers say so differently.
    pub async fn try_token(&self, token: &str) -> Result<Option<String>> {
        self.set_token(token);
        let (status, body) = self.get(&format!("{CS_V3}/account/whoami"), &[]).await?;
        if status != 200 {
            self.set_token("");
            warn!("token rejected: {}", error_message(&body));
            return Ok(None);
        }
        Ok(body
            .get("user_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    /// Log in with a password. Returns the access token the homeserver minted.
    ///
    /// No device id is asked for: the homeserver mints one, which is what makes
    /// a lost store cost one new device rather than an account whose encryption
    /// is wedged for ever (see docs/DESIGN.md).
    ///
    /// # Errors
    /// When the homeserver refuses the login or cannot be reached.
    pub async fn login(&self, user_id: &str, password: &str) -> Result<String> {
        let body = json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": user_id},
            "password": password,
            "initial_device_display_name": DEVICE_NAME,
        });
        let (status, answer) = self.post(&format!("{CS_V3}/login"), &body).await?;
        if status != 200 {
            return Err(CsError::refused(format!(
                "login as {user_id} failed: {}",
                error_message(&answer)
            )));
        }
        let token = answer
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| CsError::refused("the homeserver's login answer has no access_token"))?;
        self.set_token(token);
        Ok(token.to_owned())
    }

    /// Set this account's display name. Returns the server's complaint, if any.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn set_display_name(&self, user_id: &str, name: &str) -> Result<Option<String>> {
        let path = format!("{CS_V3}/profile/{}/displayname", quote(user_id));
        let (status, body) = self.put(&path, &json!({ "displayname": name })).await?;
        if status == 200 {
            return Ok(None);
        }
        Ok(Some(error_message(&body)))
    }

    /// Join one room by id or alias.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn join(&self, room: &str) -> Result<()> {
        let path = format!("{CS_V3}/join/{}", quote(room));
        let (status, body) = self.post(&path, &json!({})).await?;
        if status == 200 {
            return Ok(());
        }
        Err(CsError::refused(error_message(&body)))
    }

    /// The rooms this account is joined to.
    ///
    /// # Errors
    /// When the homeserver cannot be reached, or refuses to list them.
    pub async fn joined_rooms(&self) -> Result<Vec<String>> {
        let (status, body) = self.get(&format!("{CS_V3}/joined_rooms"), &[]).await?;
        if status != 200 {
            return Err(CsError::refused(error_message(&body)));
        }
        Ok(body
            .get("joined_rooms")
            .and_then(Value::as_array)
            .map(|rooms| {
                rooms
                    .iter()
                    .filter_map(|room| room.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Everyone joined to a room, mapped to their display name.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn joined_members(&self, room_id: &str) -> Result<Option<Vec<(String, String)>>> {
        let path = format!("{CS_V3}/rooms/{}/joined_members", quote(room_id));
        let (status, body) = self.get(&path, &[]).await?;
        if status != 200 {
            return Ok(None);
        }
        let Some(joined) = body.get("joined").and_then(Value::as_object) else {
            return Ok(None);
        };
        Ok(Some(
            joined
                .iter()
                .map(|(user_id, member)| {
                    let name = member
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    (user_id.clone(), name.to_owned())
                })
                .collect(),
        ))
    }

    /// One room state event's content, or None when there is none.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn room_state_event(&self, room_id: &str, event_type: &str) -> Result<Option<Value>> {
        let path = format!(
            "{CS_V3}/rooms/{}/state/{}",
            quote(room_id),
            quote(event_type)
        );
        let (status, body) = self.get(&path, &[]).await?;
        Ok((status == 200).then_some(body))
    }

    /// Resolve a room alias to a room id.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn resolve_alias(&self, alias: &str) -> Result<std::result::Result<String, String>> {
        let path = format!("{CS_V3}/directory/room/{}", quote(alias));
        let (status, body) = self.get(&path, &[]).await?;
        if status != 200 {
            return Ok(Err(error_message(&body)));
        }
        Ok(body
            .get("room_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| "the homeserver's answer has no room_id".to_owned()))
    }

    /// One event by id, or None when the room does not have it.
    ///
    /// # Errors
    /// When the homeserver cannot be reached.
    pub async fn event(&self, room_id: &str, event_id: &str) -> Result<Option<Value>> {
        let path = format!("{CS_V3}/rooms/{}/event/{}", quote(room_id), quote(event_id));
        let (status, body) = self.get(&path, &[]).await?;
        Ok((status == 200 && body.is_object()).then_some(body))
    }

    /// Send one event. Returns the event id the homeserver gave it.
    ///
    /// # Errors
    /// When the homeserver refuses the event or cannot be reached.
    pub async fn send_event(
        &self,
        room_id: &str,
        event_type: &str,
        content: &Value,
    ) -> Result<String> {
        let txn = uuid::Uuid::new_v4().simple().to_string();
        let path = format!(
            "{CS_V3}/rooms/{}/send/{}/{}",
            quote(room_id),
            quote(event_type),
            quote(&txn)
        );
        let (status, body) = self.put(&path, content).await?;
        if status != 200 {
            return Err(CsError::refused(format!(
                "the homeserver refused the message: {}",
                error_message(&body)
            )));
        }
        body.get("event_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                CsError::refused("the homeserver refused the message: no event_id came back")
            })
    }

    /// One `/sync`. `timeout_ms` 0 drains what is already there.
    ///
    /// # Errors
    /// When the homeserver refuses the sync (a dead token, say) or cannot be
    /// reached. A refusal is never swallowed: it comes back instantly, so
    /// treating it as "nobody spoke" would spin a wait loop against the server.
    pub async fn sync(
        &self,
        since: Option<&str>,
        timeout_ms: u64,
        filter: Option<&Value>,
    ) -> Result<Value> {
        let mut params: Vec<(&str, String)> = vec![("timeout", timeout_ms.to_string())];
        if let Some(since) = since {
            params.push(("since", since.to_owned()));
        }
        if let Some(filter) = filter {
            params.push(("filter", filter.to_string()));
        }
        // The request has to outlive the long poll it asked for, or the client
        // times out the server's own wait.
        let request = self
            .http
            .get(self.url(&format!("{CS_V3}/sync")))
            .query(&params)
            .bearer_auth(self.token())
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_S) + Duration::from_millis(timeout_ms));
        let response = request
            .send()
            .await
            .map_err(|exc| CsError::Unreachable(exc.to_string()))?;
        let status = response.status().as_u16();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        if status != 200 {
            return Err(CsError::refused(format!(
                "the homeserver refused to sync: {}",
                error_message(&body)
            )));
        }
        Ok(body)
    }

    /// The room-visible "I have read this". Best effort, both halves.
    pub async fn mark_read(&self, room_id: &str, event_id: &str, thread_id: &str) {
        let markers = format!("{CS_V3}/rooms/{}/read_markers", quote(room_id));
        if let Err(exc) = self
            .post(
                &markers,
                &json!({"m.fully_read": event_id, "m.read": event_id}),
            )
            .await
        {
            tracing::debug!("{room_id}: read marker for {event_id} failed: {exc}");
        }
        let receipt = format!(
            "{CS_V3}/rooms/{}/receipt/m.read/{}",
            quote(room_id),
            quote(event_id)
        );
        if let Err(exc) = self
            .post(&receipt, &json!({ "thread_id": thread_id }))
            .await
        {
            tracing::debug!("{room_id}: threaded receipt for {event_id} failed: {exc}");
        }
    }
}

/// The homeserver's own words for why it said no.
#[must_use]
pub fn error_message(body: &Value) -> String {
    body.get("error")
        .and_then(Value::as_str)
        .map_or_else(|| "no reason given".to_owned(), ToOwned::to_owned)
}

/// Get a usable access token onto `api`, caching password logins.
///
/// Returns the user id the homeserver confirms, which is the identity every
/// self-echo and mention check is made against - never the one in the config
/// file, which is only what the operator believes.
///
/// # Errors
/// When the configured token is refused, when a password login fails, or when
/// the homeserver cannot be reached.
pub async fn authenticate(api: &CommandClient, cfg: &Config) -> Result<String> {
    if let Some(path) = &cfg.access_token_file {
        let token = read_secret_file(path, "access_token_file")
            .map_err(|exc| CsError::refused(exc.to_string()))?;
        return match api.try_token(&token).await? {
            Some(user_id) => Ok(user_id),
            None => Err(CsError::refused(format!(
                "access token from {} was rejected by {}",
                path.display(),
                api.homeserver()
            ))),
        };
    }

    let cached = cfg.cached_token_path();
    if cached.exists() {
        let token = read_secret_file(&cached, "cached access token")
            .map_err(|exc| CsError::refused(exc.to_string()))?;
        if let Some(user_id) = api.try_token(&token).await? {
            info!(
                "authenticated with the cached token at {}",
                cached.display()
            );
            return Ok(user_id);
        }
        warn!(
            "cached token at {} was rejected; logging in again",
            cached.display()
        );
    }

    let Some(password) = &cfg.password else {
        return Err(CsError::refused(
            "no access_token_file and no password configured",
        ));
    };
    let token = api.login(&cfg.user_id, password).await?;
    write_secret_file(&cached, &token).map_err(|exc| CsError::refused(exc.to_string()))?;
    info!("logged in as {}; token cached", cfg.user_id);
    // Ask who the token actually belongs to rather than trusting the config.
    match api.try_token(&token).await? {
        Some(user_id) => Ok(user_id),
        None => Err(CsError::refused(
            "the homeserver would not confirm the token it just issued",
        )),
    }
}

/// Join every configured room, returning the ones that worked.
///
/// A room that refuses us is logged and skipped rather than fatal: one bad room
/// id in a list of five must not keep the account out of the other four. The
/// caller decides what to do when the list comes back empty.
pub async fn join_rooms(api: &CommandClient, rooms: &[String]) -> Vec<String> {
    let mut joined = Vec::new();
    for room_id in rooms {
        match api.join(room_id).await {
            Ok(()) => {
                info!("joined {room_id}");
                joined.push(room_id.clone());
            }
            Err(exc) => tracing::error!("cannot join {room_id}: {exc}"),
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_id_is_quoted_the_way_the_reference_quotes_it() {
        // `quote(value, safe="")`: the sigil and the colon are escaped, the
        // unreserved characters are not.
        assert_eq!(quote("!abc:example.com"), "%21abc%3Aexample.com");
        assert_eq!(quote("$event-id_x~y"), "%24event-id_x~y");
        assert_eq!(quote("#room:example.com"), "%23room%3Aexample.com");
    }

    #[test]
    fn a_homeserver_url_never_keeps_its_trailing_slash() {
        let api = CommandClient::with_http(reqwest::Client::new(), "https://example.com/");
        assert_eq!(api.homeserver(), "https://example.com");
        assert_eq!(
            api.url("/_matrix/client/versions"),
            "https://example.com/_matrix/client/versions"
        );
    }

    #[test]
    fn the_servers_own_words_are_what_a_refusal_reports() {
        assert_eq!(
            error_message(&json!({"errcode": "M_FORBIDDEN", "error": "you are not invited"})),
            "you are not invited"
        );
        assert_eq!(error_message(&Value::Null), "no reason given");
    }
}
