//! `agent-room doctor`: will this config work, and if not, which part is wrong?
//!
//! Everything a connector needs before it can say a word - a token nobody else
//! can read, a homeserver that answers, a token the homeserver accepts, rooms
//! the account is actually in, a brain that is reachable - fails in its own
//! way, and every one of those failures looks the same from the outside: the
//! agent says nothing. So each is a row here, with a one-line fix.
//!
//! The rows are ordered the way the connector meets them, and a row that cannot
//! be checked because an earlier one failed is skipped rather than guessed at:
//! a token cannot be "wrong" when the homeserver never answered.
//!
//! Exit code: 1 if any row FAILs, 0 otherwise. (A config that will not parse at
//! all never reaches doctor; the CLI reports that and exits 2.)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tracing::debug;

use crate::config::{BrainKind, Config, require_private_mode};
use crate::cs_api::{CommandClient, CsError, authenticate};
use crate::matrix::{self, DeviceCheck, WEDGE_CURE};

/// A brain endpoint that has not answered in this long is not going to save the
/// connector either. Deliberately shorter than the brain's own cold-start
/// allowance: doctor answers a person, it does not boot a model.
pub const BRAIN_TIMEOUT_S: u64 = 15;
/// `claude --version` on a busy laptop; the CLI is a node process with a
/// startup.
pub const CLAUDE_VERSION_TIMEOUT_S: u64 = 30;
/// How many `/sync` calls may go into finding an invitation. See [`invites`]:
/// the first answer can come from Synapse's initial-sync cache and predate the
/// invitation, so one is not enough and three is already generous.
pub const SYNC_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: String,
}

impl Check {
    #[must_use]
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Pass,
            detail: detail.into(),
            fix: String::new(),
        }
    }

    #[must_use]
    pub fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Skip,
            detail: detail.into(),
            fix: String::new(),
        }
    }

    #[must_use]
    pub fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
            fix: fix.into(),
        }
    }
}

/// Runs the checks. Every network call goes through the injected client.
pub struct Doctor<'a> {
    cfg: &'a Config,
    config_path: PathBuf,
    api: Option<CommandClient>,
}

impl<'a> Doctor<'a> {
    #[must_use]
    pub fn new(cfg: &'a Config, config_path: &Path, api: Option<CommandClient>) -> Self {
        Self {
            cfg,
            config_path: config_path.to_path_buf(),
            api,
        }
    }

    // -- permissions -----------------------------------------------------

    /// The secrets on disk: whoever can read them can be this account.
    #[must_use]
    pub fn check_permissions(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        if self.cfg.password.is_some() {
            checks.push(mode_check(
                "config file",
                &self.config_path,
                "the password is in it",
            ));
        }
        if let Some(token) = &self.cfg.access_token_file {
            checks.push(mode_check("token file", token, "the token IS this account"));
        }
        if self.cfg.tls.enabled
            && let Some(key) = &self.cfg.tls.client_key
        {
            checks.push(mode_check(
                "tls key",
                key,
                "the key is what the server trusts",
            ));
        }
        checks
    }

    // -- the homeserver --------------------------------------------------

    /// `/_matrix/client/versions`: the one endpoint that needs no account.
    async fn check_homeserver(&self, api: &CommandClient) -> Check {
        let url = format!("{}/_matrix/client/versions", self.cfg.homeserver);
        let answer = api.versions().await;
        let (status, body) = match answer {
            Ok(answer) => answer,
            Err(exc) => {
                let mut fix = "check the URL and that this machine can reach it".to_owned();
                if self.cfg.tls.enabled {
                    fix.push_str(" with the client certificate in tls:");
                }
                return Check::fail(
                    "homeserver",
                    format!("{} did not answer: {exc}", self.cfg.homeserver),
                    fix,
                );
            }
        };
        if status != 200 {
            return Check::fail(
                "homeserver",
                format!("{url} answered HTTP {status}"),
                "check the homeserver URL - this is not a Matrix client-server API",
            );
        }
        let newest = body
            .get("versions")
            .and_then(Value::as_array)
            .and_then(|versions| versions.last())
            .and_then(Value::as_str)
            .unwrap_or("?");
        Check::pass(
            "homeserver",
            format!("{} answers, spec {newest}", self.cfg.homeserver),
        )
    }

    /// Whoever this credential belongs to had better be who the config says.
    ///
    /// This is the same call the connector makes, so a `password:` config logs
    /// in here exactly as it would at start-up, and a cached token is reused
    /// rather than replaced.
    async fn check_token(&self, api: &CommandClient) -> (Check, Option<String>) {
        match authenticate(api, self.cfg).await {
            Err(CsError::Refused(message)) => (
                Check::fail(
                    "token",
                    message,
                    "ask whoever runs the homeserver for a new password, then \
                     re-run `agent-room init --password-from-stdin --force`",
                ),
                None,
            ),
            Err(CsError::Unreachable(cause)) => (
                Check::fail(
                    "token",
                    format!("the homeserver did not answer: {cause}"),
                    "try again",
                ),
                None,
            ),
            Ok(me) => {
                if me == self.cfg.user_id {
                    (
                        Check::pass("token", format!("accepted, and it is {me}")),
                        Some(me),
                    )
                } else {
                    let fix = format!(
                        "set user_id to {me}, or use the credential for {}",
                        self.cfg.user_id
                    );
                    (
                        Check::fail(
                            "token",
                            format!(
                                "this credential belongs to {me}, but user_id says {}",
                                self.cfg.user_id
                            ),
                            fix,
                        ),
                        Some(me),
                    )
                }
            }
        }
    }

    // -- the device --------------------------------------------------------

    /// Does the local crypto store own the token's device, as far as the
    /// homeserver is concerned? A store that does not is wedged for ever
    /// (see `matrix::DeviceWedged`), and `run` will refuse it with exit 3.
    async fn check_device(&self, api: &CommandClient) -> Check {
        if !self.cfg.store_path().exists() {
            return Check::skip(
                "device",
                "no store yet: the first `run` creates it and publishes this device's keys",
            );
        }
        let (status, whoami) = match api.get("/_matrix/client/v3/account/whoami", &[]).await {
            Ok(answer) => answer,
            Err(exc) => return Check::skip("device", format!("not checked: {exc}")),
        };
        let Some(device_id) = (status == 200)
            .then(|| whoami.get("device_id").and_then(Value::as_str))
            .flatten()
        else {
            return Check::skip("device", "not checked: whoami named no device");
        };
        let http = match self.cfg.tls.build_client() {
            Ok(http) => http,
            Err(exc) => return Check::skip("device", format!("not checked: {exc}")),
        };
        let client = match matrix::build_client(self.cfg, http.clone()).await {
            Ok(client) => client,
            Err(exc) => return Check::skip("device", format!("cannot open the store: {exc}")),
        };
        // Restore WITHOUT the wedge check: that is what this row performs.
        if let Err(exc) =
            matrix::restore_for_inspection(&client, &self.cfg.user_id, device_id, api).await
        {
            return Check::skip("device", format!("cannot read the store: {exc}"));
        }
        match matrix::device_check(&client, &self.cfg.user_id, device_id).await {
            Ok(DeviceCheck::Matches) => Check::pass(
                "device",
                format!("{device_id}: this store holds the identity the homeserver knows"),
            ),
            Ok(DeviceCheck::NoServerKeys) => Check::pass(
                "device",
                format!("{device_id}: no keys published yet; this store will publish them"),
            ),
            Ok(DeviceCheck::Mismatch { .. }) => Check::fail(
                "device",
                format!("{device_id}: the homeserver holds keys this store did not publish"),
                WEDGE_CURE,
            ),
            Err(exc) => Check::skip("device", format!("not checked: {exc}")),
        }
    }

    // -- the rooms -------------------------------------------------------

    async fn check_rooms(&self, api: &CommandClient) -> std::result::Result<Vec<Check>, CsError> {
        let joined: BTreeSet<String> = match api.joined_rooms().await {
            Ok(rooms) => rooms.into_iter().collect(),
            Err(CsError::Refused(message)) => {
                return Ok(vec![Check::fail(
                    "rooms",
                    format!("the homeserver would not list this account's rooms: {message}"),
                    "try again; if it persists, the account may be rate-limited",
                )]);
            }
            Err(exc) => return Err(exc),
        };
        let wanted: BTreeSet<String> = self.cfg.rooms.iter().cloned().collect();
        let mut invited: BTreeSet<String> = BTreeSet::new();
        if !wanted.is_subset(&joined) {
            // Only worth syncing when something is not joined yet: an
            // invitation is the normal state of an agent never started.
            let missing: BTreeSet<String> = wanted.difference(&joined).cloned().collect();
            invited = invites(api, &missing).await?;
        }
        let mut checks = Vec::new();
        for room in &self.cfg.rooms {
            checks.push(self.check_room(api, room, &joined, &invited).await?);
        }
        Ok(checks)
    }

    async fn check_room(
        &self,
        api: &CommandClient,
        room: &str,
        joined: &BTreeSet<String>,
        invited: &BTreeSet<String>,
    ) -> std::result::Result<Check, CsError> {
        let name = format!("room {room}");
        let mut room_id = room.to_owned();
        if room.starts_with('#') {
            match api.resolve_alias(room).await? {
                Ok(resolved) => room_id = resolved,
                Err(message) => {
                    return Ok(Check::fail(
                        name,
                        format!("the alias does not resolve: {message}"),
                        "check the alias, or use the room id (!id:server)",
                    ));
                }
            }
        }
        if joined.contains(&room_id) {
            return Ok(Check::pass(name, "joined"));
        }
        if invited.contains(&room_id) {
            return Ok(Check::pass(
                name,
                "invited; the connector joins it when it starts",
            ));
        }
        Ok(Check::fail(
            name,
            "this account is neither in it nor invited to it",
            format!("ask whoever runs the room to invite {}", self.cfg.user_id),
        ))
    }

    // -- the brain -------------------------------------------------------

    async fn check_brain(&self) -> Check {
        let Some(brain) = &self.cfg.brain else {
            return Check::skip(
                "brain",
                "no brain: this is a live session's config (`agent-room mcp`)",
            );
        };
        match brain.kind {
            BrainKind::OpenaiCompat => match &brain.openai_compat {
                Some(openai) => {
                    check_openai(&openai.base_url, &openai.model, &openai.resolved_api_key()).await
                }
                None => Check::skip("brain", "no openai_compat section to check"),
            },
            BrainKind::ClaudeCode => match &brain.claude_code {
                Some(claude) => check_claude(&claude.claude_bin).await,
                None => Check::skip("brain", "no claude_code section to check"),
            },
            // The config validator guarantees the section for the kind, so what
            // is left is `echo`: no endpoint, no binary, nothing that can be
            // down.
            BrainKind::Echo => {
                Check::pass("brain", "the echo brain is built in (it is for the gates)")
            }
        }
    }

    // -- the run ---------------------------------------------------------

    /// Every check, in the order the connector meets them.
    pub async fn run(mut self) -> Vec<Check> {
        let mut checks = self.check_permissions();
        let api = match self.api.take() {
            Some(api) => api,
            None => match CommandClient::new(self.cfg) {
                Ok(api) => api,
                Err(exc) => {
                    // A TLS block that cannot be built is a homeserver failure:
                    // it is the connection that will not happen.
                    checks.push(Check::fail(
                        "homeserver",
                        exc.to_string(),
                        "fix the tls: block in the config",
                    ));
                    checks.push(Check::skip(
                        "token",
                        "not checked: there is no usable connection",
                    ));
                    checks.extend(self.skip_rooms("there is no usable connection"));
                    checks.push(self.check_brain().await);
                    return checks;
                }
            },
        };
        self.run_with(&api, checks).await
    }

    async fn run_with(&self, api: &CommandClient, mut checks: Vec<Check>) -> Vec<Check> {
        let homeserver = self.check_homeserver(api).await;
        let homeserver_ok = homeserver.status == Status::Pass;
        checks.push(homeserver);
        if !homeserver_ok {
            checks.push(Check::skip(
                "token",
                "not checked: the homeserver did not answer",
            ));
            checks.extend(self.skip_rooms("the homeserver did not answer"));
            checks.push(self.check_brain().await);
            return checks;
        }
        let (token, _me) = self.check_token(api).await;
        let token_ok = token.status == Status::Pass;
        checks.push(token);
        if !token_ok {
            checks.extend(self.skip_rooms("the token was not accepted"));
            checks.push(self.check_brain().await);
            return checks;
        }
        checks.push(self.check_device(api).await);
        match self.check_rooms(api).await {
            Ok(rooms) => checks.extend(rooms),
            // A homeserver that answered and then stopped, mid-run. Every check
            // handles its own failures; this is the net under the rest of them,
            // because a diagnostic tool that ends in a traceback has diagnosed
            // nothing.
            Err(exc) => {
                checks.push(Check::fail(
                    "connection",
                    format!("the homeserver stopped answering: {exc}"),
                    "try again",
                ));
                checks.extend(self.skip_rooms("the homeserver stopped answering"));
            }
        }
        checks.push(self.check_brain().await);
        checks
    }

    fn skip_rooms(&self, why: &str) -> Vec<Check> {
        self.cfg
            .rooms
            .iter()
            .map(|room| Check::skip(format!("room {room}"), format!("not checked: {why}")))
            .collect()
    }
}

/// Which rooms this account has been invited to, past the sync cache.
///
/// More than one sync, for the same reason the connector drains its backlog at
/// start-up: Synapse caches an initial sync per device for a couple of minutes,
/// so the first answer can predate an invitation sent a minute ago. A single
/// sync therefore tells a friend "nobody invited you" exactly when somebody
/// just did - which is the one moment they run this command. The second sync
/// continues from the first one's token and is not served from that cache.
/// Found by gate D1, 2026-09-02.
///
/// # Errors
/// When the homeserver stops answering mid-run.
pub async fn invites(
    api: &CommandClient,
    wanted: &BTreeSet<String>,
) -> std::result::Result<BTreeSet<String>, CsError> {
    let mut invited: BTreeSet<String> = BTreeSet::new();
    let mut since: Option<String> = None;
    for _attempt in 0..SYNC_ATTEMPTS {
        let body = api.sync(since.as_deref(), 0, None).await?;
        since = body
            .get("next_batch")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(rooms) = body
            .get("rooms")
            .and_then(|rooms| rooms.get("invite"))
            .and_then(Value::as_object)
        {
            invited.extend(rooms.keys().cloned());
        }
        if wanted.is_subset(&invited) {
            break;
        }
    }
    Ok(invited)
}

fn mode_check(name: &str, path: &Path, why: &str) -> Check {
    if !path.is_file() {
        return Check::fail(
            name,
            format!("{} does not exist", path.display()),
            format!("create it, or fix the path ({why})"),
        );
    }
    match require_private_mode(path, name) {
        Err(exc) => Check::fail(
            name,
            exc.to_string(),
            format!("chmod 600 {} - {why}", path.display()),
        ),
        Ok(()) => Check::pass(name, format!("{} is 0600", path.display())),
    }
}

/// `GET {base_url}/models` - the cheapest question an endpoint answers.
async fn check_openai(base_url: &str, model: &str, api_key: &str) -> Check {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(BRAIN_TIMEOUT_S))
        .build()
    {
        Ok(client) => client,
        Err(exc) => {
            return Check::fail("brain", format!("cannot build an HTTP client: {exc}"), "");
        }
    };
    let mut request = client.get(&url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(exc) => {
            return Check::fail(
                "brain",
                format!("{url} did not answer: {exc}"),
                "start the model server, or fix brain.openai_compat.base_url",
            );
        }
    };
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    if status != 200 {
        return Check::fail(
            "brain",
            format!("{url} answered HTTP {status}"),
            "check base_url (it usually ends in /v1) and the api_key",
        );
    }
    let served = model_ids(&body);
    if !served.is_empty() && !served.contains(model) {
        let listed: Vec<&str> = served.iter().map(String::as_str).collect();
        return Check::fail(
            "brain",
            format!(
                "{url} does not serve '{model}'; it has {}",
                listed.join(", ")
            ),
            "set brain.openai_compat.model to one of those",
        );
    }
    Check::pass("brain", format!("{url} answers and serves {model}"))
}

async fn check_claude(claude_bin: &str) -> Check {
    let Some(binary) = which(claude_bin) else {
        return Check::fail(
            "brain",
            format!("{claude_bin} is not on PATH"),
            "install Claude Code, or set brain.claude_code.claude_bin to its full path",
        );
    };
    let spawned = tokio::process::Command::new(&binary)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output =
        match tokio::time::timeout(Duration::from_secs(CLAUDE_VERSION_TIMEOUT_S), spawned).await {
            Err(_elapsed) => {
                return Check::fail(
                    "brain",
                    format!(
                        "{} --version failed: it did not answer in {CLAUDE_VERSION_TIMEOUT_S} s",
                        binary.display()
                    ),
                    "check the Claude Code CLI",
                );
            }
            Ok(Err(exc)) => {
                return Check::fail(
                    "brain",
                    format!("{} --version failed: {exc}", binary.display()),
                    "check the Claude Code CLI",
                );
            }
            Ok(Ok(output)) => output,
        };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let lines: Vec<&str> = text.trim().lines().collect();
    if !output.status.success() {
        let joined: String = lines.join(" ").chars().take(120).collect();
        return Check::fail(
            "brain",
            format!(
                "{} --version exited {}: {joined}",
                binary.display(),
                output.status.code().unwrap_or(-1)
            ),
            "check the Claude Code CLI is installed and logged in",
        );
    }
    Check::pass(
        "brain",
        format!(
            "{} is {}",
            binary.display(),
            lines.first().copied().unwrap_or("there")
        ),
    )
}

/// `shutil.which`: the binary as named, or the first hit on `PATH`.
#[must_use]
pub fn which(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Model ids from an OpenAI-style `/models` body, or nothing we can trust.
fn model_ids(body: &Value) -> BTreeSet<String> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The PASS/FAIL table, with the fix under every row that needs one.
#[must_use]
pub fn format_report(cfg: &Config, config_path: &Path, checks: &[Check]) -> String {
    let width = checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec![
        format!("agent-room doctor: {}", config_path.display()),
        format!("account {} at {}", cfg.user_id, cfg.homeserver),
        String::new(),
    ];
    for check in checks {
        lines.push(format!(
            "{:4}  {:width$}  {}",
            check.status.label(),
            check.name,
            check.detail
        ));
        if !check.fix.is_empty() && check.status == Status::Fail {
            lines.push(format!("      {:width$}  fix: {}", "", check.fix));
        }
    }
    let count = |status: Status| checks.iter().filter(|c| c.status == status).count();
    lines.push(String::new());
    lines.push(format!(
        "{} passed, {} failed, {} skipped",
        count(Status::Pass),
        count(Status::Fail),
        count(Status::Skip)
    ));
    lines.join("\n")
}

#[must_use]
pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|check| check.status == Status::Fail))
}

/// What the CLI calls: check, print the table, return the exit code.
pub async fn run(cfg: &Config, config_path: &Path) -> i32 {
    let checks = Doctor::new(cfg, config_path, None).run().await;
    debug!("doctor ran {} checks", checks.len());
    println!("{}", format_report(cfg, config_path, &checks));
    exit_code(&checks)
}
