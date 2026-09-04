//! `agent-room doctor`: every row, PASS and FAIL.
//!
//! The Matrix half runs against the fake homeserver on loopback; the brain half
//! runs against a REAL OpenAI-style `/models` server and a REAL fake `claude`
//! executable, because those two checks are entirely about what a process on
//! this machine answers, and faking them would test nothing.
//!
//! What matters in every case is the pair (which row, what a person is told to
//! do about it) plus the exit code, since that is all a friend and a script
//! ever see.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_room::config::{
    BrainConfig, BrainKind, ClaudeCodeBrainConfig, Config, McpConfig, OpenAiCompatBrainConfig,
    PolicyConfig, PostAs, TlsConfig, default_history_limit, default_transcript_archives,
    default_transcript_keep,
};
use agent_room::cs_api::CommandClient;
use agent_room::doctor::{Check, Doctor, Status, exit_code, format_report};

use super::fake_homeserver::FakeHomeserver;

const ME: &str = "@bot-a:example.com";
const ROOM_ID: &str = "!room:example.com";
const CONFIG_PATH: &str = "/nowhere/config.yaml";

fn config(dir: &Path, homeserver: &str) -> Config {
    let token = dir.join("access");
    if !token.exists() {
        agent_room::config::write_secret_file(&token, "syt_fake").expect("the token");
    }
    Config {
        homeserver: homeserver.to_owned(),
        user_id: ME.to_owned(),
        access_token_file: Some(token),
        password: None,
        rooms: vec![ROOM_ID.to_owned()],
        persona_file: None,
        state_dir: dir.join("state"),
        brain: Some(BrainConfig {
            kind: BrainKind::Echo,
            openai_compat: None,
            claude_code: None,
            echo: agent_room::config::EchoBrainConfig::default(),
        }),
        policy: PolicyConfig::default(),
        mcp: McpConfig {
            post_as: PostAs::Notice,
        },
        tls: TlsConfig::default(),
        history_limit: default_history_limit(),
        transcript_keep: default_transcript_keep(),
        transcript_archives: default_transcript_archives(),
        allow_wedged_device: false,
    }
}

/// Every check by name, from one run of the real `Doctor`.
async fn rows(cfg: &Config, home: Option<&FakeHomeserver>) -> BTreeMap<String, Check> {
    let api = home.map(|_home| CommandClient::new(cfg).expect("a plain HTTP client"));
    let checks = Doctor::new(cfg, Path::new(CONFIG_PATH), api).run().await;
    let names: std::collections::BTreeSet<&str> =
        checks.iter().map(|check| check.name.as_str()).collect();
    assert_eq!(names.len(), checks.len(), "duplicate rows: {checks:?}");
    checks
        .into_iter()
        .map(|check| (check.name.clone(), check))
        .collect()
}

fn code(rows: &BTreeMap<String, Check>) -> i32 {
    let checks: Vec<Check> = rows.values().cloned().collect();
    exit_code(&checks)
}

fn room_row() -> String {
    format!("room {ROOM_ID}")
}

// -- the happy config --------------------------------------------------------

#[tokio::test]
async fn a_config_that_will_work_passes_every_row() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(
        checks.keys().cloned().collect::<Vec<String>>(),
        vec![
            "brain".to_owned(),
            "device".to_owned(),
            "homeserver".to_owned(),
            room_row(),
            "token".to_owned(),
            "token file".to_owned(),
        ]
    );
    // No store exists before the first `run`, so the device row can only SKIP
    // here - and say why. Every other row passes.
    let device = &checks["device"];
    assert_eq!(device.status, Status::Skip, "{device:?}");
    assert!(device.detail.contains("no store yet"), "{device:?}");
    assert!(
        checks
            .iter()
            .filter(|(name, _)| name.as_str() != "device")
            .all(|(_, check)| check.status == Status::Pass)
    );
    assert_eq!(code(&checks), 0);
}

// -- permissions -------------------------------------------------------------

#[tokio::test]
async fn a_token_file_anybody_can_read_fails_the_permission_row() {
    // Teeth: remove the perms check and doctor passes a 0644 token.
    //
    // The token IS the account. A doctor that reports "all good" on a
    // world-readable one is worse than no doctor, because it is the thing a
    // friend trusts instead of looking.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let token = cfg.access_token_file.clone().expect("a token file");
    std::fs::set_permissions(&token, PermissionsExt::from_mode(0o644)).expect("chmod");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["token file"].status, Status::Fail);
    assert!(checks["token file"].detail.contains("0644"));
    assert!(checks["token file"].fix.contains("chmod 600"));
    assert_eq!(code(&checks), 1);
}

#[tokio::test]
async fn a_token_file_that_is_not_there_names_the_path() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let token = cfg.access_token_file.clone().expect("a token file");
    std::fs::remove_file(&token).expect("unlink");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["token file"].status, Status::Fail);
    assert!(
        checks["token file"]
            .detail
            .contains(&token.display().to_string())
    );
}

#[tokio::test]
async fn a_password_in_the_config_makes_the_config_file_a_secret() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let config_path = dir.path().join("config.yaml");
    std::fs::write(&config_path, "# stand-in\n").expect("write");
    std::fs::set_permissions(&config_path, PermissionsExt::from_mode(0o644)).expect("chmod");
    let mut cfg = config(dir.path(), "https://matrix.example.com");
    cfg.access_token_file = None;
    cfg.password = Some("hunter2".to_owned());

    let checks = Doctor::new(&cfg, &config_path, None).check_permissions();
    assert_eq!(
        checks
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<&str>>(),
        vec!["config file"]
    );
    assert_eq!(checks[0].status, Status::Fail);

    std::fs::set_permissions(&config_path, PermissionsExt::from_mode(0o600)).expect("chmod");
    let checks = Doctor::new(&cfg, &config_path, None).check_permissions();
    assert_eq!(checks[0].status, Status::Pass);
}

#[tokio::test]
async fn the_tls_key_is_held_to_the_same_rule() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let (cert, key) = (dir.path().join("client.crt"), dir.path().join("client.key"));
    std::fs::write(&cert, "PEM").expect("write");
    std::fs::write(&key, "PEM").expect("write");
    std::fs::set_permissions(&key, PermissionsExt::from_mode(0o600)).expect("chmod");
    let mut cfg = config(dir.path(), "https://matrix.example.com");
    cfg.tls = TlsConfig {
        enabled: true,
        client_cert: Some(cert),
        client_key: Some(key.clone()),
        ca_file: None,
        verify: true,
    };
    let named = |checks: Vec<Check>| {
        checks
            .into_iter()
            .map(|check| (check.name, check.status))
            .collect::<BTreeMap<String, Status>>()
    };
    let checks = named(Doctor::new(&cfg, Path::new(CONFIG_PATH), None).check_permissions());
    assert_eq!(checks["tls key"], Status::Pass);

    std::fs::set_permissions(&key, PermissionsExt::from_mode(0o640)).expect("chmod");
    let checks = named(Doctor::new(&cfg, Path::new(CONFIG_PATH), None).check_permissions());
    assert_eq!(checks["tls key"], Status::Fail);
}

// -- the homeserver ----------------------------------------------------------

#[tokio::test]
async fn a_homeserver_that_does_not_answer_skips_what_depends_on_it() {
    let dir = tempfile::tempdir().expect("tmpdir");
    // `.invalid` never resolves, on a network or off one.
    let cfg = config(dir.path(), "https://matrix.invalid");
    let checks = rows(&cfg, None).await;
    assert_eq!(checks["homeserver"].status, Status::Fail);
    assert!(checks["homeserver"].detail.contains("did not answer"));
    assert_eq!(checks["token"].status, Status::Skip);
    assert_eq!(checks[&room_row()].status, Status::Skip);
    assert!(
        checks["token"]
            .detail
            .contains("the homeserver did not answer")
    );
    assert_eq!(code(&checks), 1);
}

#[tokio::test]
async fn something_that_is_not_a_matrix_server_fails_the_homeserver_row() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.versions_status = 404);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["homeserver"].status, Status::Fail);
    assert!(checks["homeserver"].detail.contains("404"));
}

#[tokio::test]
async fn the_homeserver_row_reports_the_spec_version() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert!(checks["homeserver"].detail.contains("v1.13"));
    home.with(|state| {
        assert!(
            state
                .requests
                .iter()
                .any(|seen| seen.path == "/_matrix/client/versions")
        );
    });
}

#[tokio::test]
async fn a_tls_block_that_cannot_be_built_is_a_homeserver_failure() {
    // No client certificate, no connection - and doctor has to say which file.
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut cfg = config(dir.path(), "https://matrix.example.com");
    cfg.tls = TlsConfig {
        enabled: true,
        client_cert: Some(dir.path().join("missing.crt")),
        client_key: Some(dir.path().join("missing.key")),
        ca_file: None,
        verify: true,
    };
    // No injected client here: the real one has to be the thing that fails.
    let checks = rows(&cfg, None).await;
    assert_eq!(checks["homeserver"].status, Status::Fail);
    assert!(checks["homeserver"].detail.contains("missing.crt"));
    assert_eq!(checks["token"].status, Status::Skip);
}

// -- the token ---------------------------------------------------------------

#[tokio::test]
async fn a_token_the_homeserver_refuses_fails_and_stops_there() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.whoami_error = Some("Invalid access token".to_owned()));
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["homeserver"].status, Status::Pass);
    assert_eq!(checks["token"].status, Status::Fail);
    assert!(checks["token"].detail.contains("rejected"));
    assert!(
        checks["token"]
            .fix
            .contains("init --password-from-stdin --force")
    );
    assert_eq!(checks[&room_row()].status, Status::Skip);
    assert_eq!(code(&checks), 1);
}

#[tokio::test]
async fn a_token_for_the_wrong_account_is_a_failure_of_its_own() {
    // Two accounts, one config: everything would "work" and nobody would notice.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.whoami_user = "@somebody-else:example.com".to_owned());
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["token"].status, Status::Fail);
    assert!(
        checks["token"]
            .detail
            .contains("@somebody-else:example.com")
    );
    assert!(checks["token"].detail.contains(ME));
}

// -- the rooms ---------------------------------------------------------------

#[tokio::test]
async fn a_room_the_account_is_only_invited_to_passes() {
    // The normal state of a new agent: invited, not yet joined.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| {
        state.invited = vec![ROOM_ID.to_owned()];
        state.invited_from_sync = 1;
    });
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks[&room_row()].status, Status::Pass);
    assert!(checks[&room_row()].detail.contains("invited"));
    home.with(|state| {
        assert_eq!(
            state.syncs.len(),
            1,
            "one sync, and only because something was not joined"
        );
    });
}

#[tokio::test]
async fn an_invitation_the_first_sync_missed_is_still_found() {
    // Synapse caches an initial sync per device for a couple of minutes, so the
    // first answer can predate an invitation sent a minute ago - and a friend
    // runs doctor exactly then, right after being invited. Found live by gate
    // D1 on 2026-09-02.
    //
    // Teeth: cut SYNC_ATTEMPTS to 1 and this fails.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| {
        state.invited = vec![ROOM_ID.to_owned()];
        state.invited_from_sync = 2;
    });
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks[&room_row()].status, Status::Pass);
    home.with(|state| {
        assert_eq!(
            state.syncs.len(),
            2,
            "one sync is the stale one; the second carries the invitation"
        );
    });
}

#[tokio::test]
async fn a_room_nobody_invited_the_account_to_says_who_to_ask() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks[&room_row()].status, Status::Fail);
    assert!(checks[&room_row()].fix.contains(ME));
}

#[tokio::test]
async fn a_joined_room_costs_no_sync() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    rows(&cfg, Some(&home)).await;
    home.with(|state| assert!(state.syncs.is_empty()));
}

#[tokio::test]
async fn a_homeserver_that_refuses_to_list_the_rooms_says_so_once() {
    // A rate-limited account, most likely. One row, not one per room.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms_error = Some("Too Many Requests".to_owned()));
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["rooms"].status, Status::Fail);
    assert!(checks["rooms"].detail.contains("Too Many Requests"));
}

#[tokio::test]
async fn a_homeserver_that_stops_answering_mid_run_is_a_row_not_a_traceback() {
    // A diagnostic tool that ends in a stack trace has diagnosed nothing.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.rooms_go_away = true);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), &home.base_url);
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["connection"].status, Status::Fail);
    assert!(checks["connection"].detail.contains("stopped answering"));
    assert_eq!(checks[&room_row()].status, Status::Skip);
    assert_eq!(
        checks["brain"].status,
        Status::Pass,
        "the local half is still worth checking"
    );
}

#[tokio::test]
async fn an_alias_is_resolved_before_it_is_judged() {
    let alias = "#the-room:example.com";
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| {
        state.joined_rooms = vec![ROOM_ID.to_owned()];
        state.aliases.insert(alias.to_owned(), ROOM_ID.to_owned());
    });
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut cfg = config(dir.path(), &home.base_url);
    cfg.rooms = vec![alias.to_owned()];
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks[&format!("room {alias}")].status, Status::Pass);

    home.with(|state| state.aliases.clear());
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks[&format!("room {alias}")].status, Status::Fail);
    assert!(
        checks[&format!("room {alias}")]
            .detail
            .contains("does not resolve")
    );
}

// -- the brain ---------------------------------------------------------------

/// A real OpenAI-compatible `/models`, on a real socket.
struct ModelsEndpoint {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ModelsEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ModelsEndpoint {
    async fn start() -> Self {
        Self::start_with_key(None).await
    }

    /// Like `start`, but 401s any request that does not carry
    /// `Authorization: Bearer <required_key>` - so a test can prove the
    /// doctor's brain check actually sends the configured key.
    async fn start_with_key(required_key: Option<&str>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("bound").port();
        let required_key = required_key.map(str::to_owned);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _peer)) = listener.accept().await else {
                    return;
                };
                let required_key = required_key.clone();
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 2048];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let authorized = required_key.as_deref().is_none_or(|key| {
                        request.lines().any(|line| {
                            line.eq_ignore_ascii_case(&format!("authorization: bearer {key}"))
                        })
                    });
                    let response = if authorized {
                        let body = r#"{"object":"list","data":[{"id":"qwen3","object":"model"}]}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        let body = r#"{"error":"missing or wrong bearer token"}"#;
                        format!(
                            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    let _written = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            task,
        }
    }
}

fn openai_config(dir: &Path, homeserver: &str, base_url: &str, model: &str) -> Config {
    let mut cfg = config(dir, homeserver);
    cfg.brain = Some(BrainConfig {
        kind: BrainKind::OpenaiCompat,
        openai_compat: Some(OpenAiCompatBrainConfig::shipped(base_url, model)),
        claude_code: None,
        echo: agent_room::config::EchoBrainConfig::default(),
    });
    cfg
}

#[tokio::test]
async fn a_reachable_endpoint_that_serves_the_model_passes() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let models = ModelsEndpoint::start().await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = openai_config(dir.path(), &home.base_url, &models.base_url, "qwen3");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Pass);
    assert!(checks["brain"].detail.contains("qwen3"));
}

#[tokio::test]
async fn an_endpoint_that_does_not_serve_the_model_fails_and_lists_what_it_has() {
    // The connector would get an HTTP error on every single message.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let models = ModelsEndpoint::start().await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = openai_config(dir.path(), &home.base_url, &models.base_url, "llama9");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Fail);
    assert!(checks["brain"].detail.contains("qwen3"));
}

#[tokio::test]
async fn a_brain_that_is_not_running_fails_with_its_url() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = openai_config(dir.path(), &home.base_url, "http://127.0.0.1:1/v1", "qwen3");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Fail);
    assert!(
        checks["brain"]
            .detail
            .contains("http://127.0.0.1:1/v1/models")
    );
}

#[tokio::test]
async fn a_key_protected_endpoint_passes_when_the_key_is_configured() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let models = ModelsEndpoint::start_with_key(Some("s3cr3t")).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut cfg = openai_config(dir.path(), &home.base_url, &models.base_url, "qwen3");
    cfg.brain
        .as_mut()
        .and_then(|brain| brain.openai_compat.as_mut())
        .expect("openai_compat section")
        .api_key = "s3cr3t".to_owned();
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Pass);
}

#[tokio::test]
async fn a_key_protected_endpoint_fails_without_the_key() {
    // Regression test: the doctor's brain check used to build its GET
    // /models request with no Authorization header at all, so it always
    // failed 401 against an endpoint that requires one - even with the
    // right key configured.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let models = ModelsEndpoint::start_with_key(Some("s3cr3t")).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = openai_config(dir.path(), &home.base_url, &models.base_url, "qwen3");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Fail);
    assert!(checks["brain"].detail.contains("401"));
}

fn write_fake_claude(dir: &Path, version: &str, code: i32) -> PathBuf {
    let path = dir.join("claude");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"{version}\"\nexit {code}\n"),
    )
    .expect("write");
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o755)).expect("chmod");
    path
}

fn claude_config(dir: &Path, homeserver: &str, claude_bin: &str) -> Config {
    let mut cfg = config(dir, homeserver);
    let mut claude = ClaudeCodeBrainConfig::shipped("sonnet", dir.join("state"));
    claude_bin.clone_into(&mut claude.claude_bin);
    cfg.brain = Some(BrainConfig {
        kind: BrainKind::ClaudeCode,
        openai_compat: None,
        claude_code: Some(claude),
        echo: agent_room::config::EchoBrainConfig::default(),
    });
    cfg
}

#[tokio::test]
async fn the_claude_cli_is_checked_by_running_it() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let claude = write_fake_claude(dir.path(), "9.9.9 (Claude Code)", 0);
    let cfg = claude_config(dir.path(), &home.base_url, &claude.display().to_string());
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(
        checks["brain"].status,
        Status::Pass,
        "{:?}",
        checks["brain"]
    );
    assert!(checks["brain"].detail.contains("9.9.9"));
}

#[tokio::test]
async fn a_claude_that_is_not_installed_fails() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = claude_config(dir.path(), &home.base_url, "claude-that-is-not-installed");
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Fail);
    assert!(checks["brain"].detail.contains("not on PATH"));
}

#[tokio::test]
async fn a_claude_that_exits_non_zero_fails() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let claude = write_fake_claude(dir.path(), "not logged in", 1);
    let cfg = claude_config(dir.path(), &home.base_url, &claude.display().to_string());
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Fail);
    assert!(checks["brain"].detail.contains("exited 1"));
}

#[tokio::test]
async fn a_live_session_config_has_no_brain_to_check() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.joined_rooms = vec![ROOM_ID.to_owned()]);
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut cfg = config(dir.path(), &home.base_url);
    cfg.brain = None;
    let checks = rows(&cfg, Some(&home)).await;
    assert_eq!(checks["brain"].status, Status::Skip);
    assert!(checks["brain"].detail.contains("agent-room mcp"));
    assert_eq!(code(&checks), 0, "a skipped row is not a failure");
}

// -- what a person sees ------------------------------------------------------

#[test]
fn the_report_puts_the_fix_under_the_row_that_needs_it() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = config(dir.path(), "https://matrix.example.com");
    let checks = vec![
        Check::pass("token file", "/x/token is 0600"),
        Check::fail("homeserver", "nothing there", "check the URL"),
        Check {
            fix: "unused".to_owned(),
            ..Check::skip("token", "not checked: the homeserver did not answer")
        },
    ];
    let report = format_report(&cfg, Path::new(CONFIG_PATH), &checks);
    assert!(report.contains(CONFIG_PATH));
    assert!(report.contains(&format!("account {ME} at https://matrix.example.com")));
    assert!(report.contains("fix: check the URL"));
    assert!(!report.contains("fix: unused"), "only a FAIL gets a fix");
    assert!(report.contains("1 passed, 1 failed, 1 skipped"));
    // The live gate parses this table by splitting on the status word.
    assert!(report.contains("FAIL  homeserver"));
}

#[tokio::test]
async fn the_command_prints_the_table_and_exits_one_on_a_failure() {
    // The shipped path: `agent-room doctor --config ...` on a broken config.
    use agent_room::cli::{Cli, run};
    use clap::Parser;

    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let token = dir.path().join("token");
    agent_room::config::write_secret_file(&token, "syt_x").expect("the token");
    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "homeserver: {}\nuser_id: \"{ME}\"\naccess_token_file: {}\nrooms:\n  - \"{ROOM_ID}\"\n\
             state_dir: {}\nbrain:\n  kind: echo\n",
            home.base_url,
            token.display(),
            dir.path().join("state").display()
        ),
    )
    .expect("write");
    let cli = Cli::try_parse_from([
        "agent-room",
        "doctor",
        "--config",
        &config_path.display().to_string(),
    ])
    .expect("the flags parse");
    assert_eq!(run(cli).await.expect("doctor returns a code"), 1);
}

#[tokio::test]
async fn a_config_that_does_not_parse_never_reaches_doctor() {
    use agent_room::cli::{Cli, run};
    use clap::Parser;

    let dir = tempfile::tempdir().expect("tmpdir");
    let broken = dir.path().join("broken.yaml");
    std::fs::write(&broken, "homeserver: [").expect("write");
    let cli = Cli::try_parse_from([
        "agent-room",
        "doctor",
        "--config",
        &broken.display().to_string(),
    ])
    .expect("the flags parse");
    assert_eq!(
        run(cli).await.expect("a code"),
        2,
        "a broken config is not a failed check"
    );
}

/// The suite keeps its tokens in temp directories, so nothing here may depend
/// on the loose-permission escape hatch being set.
#[test]
fn the_permission_rule_is_the_one_the_connector_uses() {
    let _unused: Arc<()> = Arc::new(());
    assert!(
        std::env::var(agent_room::config::ALLOW_LOOSE_PERMS_ENV).is_err(),
        "the suite must not run with the loose-permission override set"
    );
}
