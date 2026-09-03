//! `agent-room init`, driven through the CLI a friend actually types.
//!
//! Every test goes through `Cli::try_parse_from(["agent-room", "init", ...])`
//! and then `cli::run`, because the flags are half the product here: a slice of
//! `init_cmd` called directly would not notice a flag clap never accepted.
//! Nothing is written outside the test's own temp directory, and the two tests
//! that need a homeserver get the fake one on loopback.

use std::path::{Path, PathBuf};

use agent_room::cli::{Cli, Command, run};
use agent_room::config::{PolicyConfig, TlsConfig, load_config};
use agent_room::init_cmd::{
    InitArgs, PERSONA_TEMPLATE, PasswordSource, count_blanks, persona_body, render_persona,
    run_with,
};
use clap::Parser;

use super::fake_homeserver::FakeHomeserver;

const ME: &str = "@bot-a:example.com";
const ROOM_ID: &str = "!room:example.com";
const PASSWORD: &str = "correct-horse-battery-staple";

/// The flags every test starts from: a token file, out and state in tmp.
///
/// Never the shipped defaults for `--out` / `--state-dir`: those are where the
/// person running the suite keeps their own agent.
struct Flags {
    homeserver: String,
    user: String,
    rooms: Vec<String>,
    brain: String,
    openai_base_url: Option<String>,
    openai_model: Option<String>,
    out: PathBuf,
    state_dir: PathBuf,
    token_file: Option<PathBuf>,
    password_from_stdin: bool,
    extra: Vec<String>,
}

impl Flags {
    fn new(dir: &Path) -> Self {
        let token = dir.join("token");
        if !token.exists() {
            agent_room::config::write_secret_file(&token, "syt_existing").expect("the token");
        }
        Self {
            homeserver: "https://matrix.example.com".to_owned(),
            user: ME.to_owned(),
            rooms: vec![ROOM_ID.to_owned()],
            brain: "openai_compat".to_owned(),
            openai_base_url: Some("http://localhost:11434/v1".to_owned()),
            openai_model: Some("qwen3".to_owned()),
            out: dir.join("config"),
            state_dir: dir.join("state"),
            token_file: Some(token),
            password_from_stdin: false,
            extra: Vec::new(),
        }
    }

    fn claude(mut self) -> Self {
        "claude_code".clone_into(&mut self.brain);
        self.openai_base_url = None;
        self.openai_model = None;
        self
    }

    fn with(mut self, flag: &str, value: &str) -> Self {
        self.extra.push(flag.to_owned());
        if !value.is_empty() {
            self.extra.push(value.to_owned());
        }
        self
    }

    fn argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = vec!["agent-room".to_owned(), "init".to_owned()];
        argv.extend(["--homeserver".to_owned(), self.homeserver.clone()]);
        argv.extend(["--user".to_owned(), self.user.clone()]);
        for room in &self.rooms {
            argv.extend(["--room".to_owned(), room.clone()]);
        }
        argv.extend(["--brain".to_owned(), self.brain.clone()]);
        if let Some(base_url) = &self.openai_base_url {
            argv.extend(["--openai-base-url".to_owned(), base_url.clone()]);
        }
        if let Some(model) = &self.openai_model {
            argv.extend(["--openai-model".to_owned(), model.clone()]);
        }
        argv.extend(["--out".to_owned(), self.out.display().to_string()]);
        argv.extend([
            "--state-dir".to_owned(),
            self.state_dir.display().to_string(),
        ]);
        if let Some(token) = &self.token_file {
            argv.extend(["--token-file".to_owned(), token.display().to_string()]);
        }
        if self.password_from_stdin {
            argv.push("--password-from-stdin".to_owned());
        }
        argv.extend(self.extra.clone());
        argv
    }
}

/// `agent-room init ...`, the way the shipped binary runs it. Returns the code.
async fn init(flags: &Flags) -> i32 {
    let cli = match Cli::try_parse_from(flags.argv()) {
        Ok(cli) => cli,
        // clap refuses a flag argparse would have refused too: exit 2, and
        // nothing has happened yet.
        Err(_exc) => return 2,
    };
    run(cli)
        .await
        .expect("init never returns an error, only a code")
}

fn written(dir: &Path) -> (PathBuf, PathBuf) {
    (
        dir.join("config").join("config.yaml"),
        dir.join("config").join("persona.md"),
    )
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("it exists")
        .permissions()
        .mode()
        & 0o777
}

// -- what it writes ----------------------------------------------------------

#[tokio::test]
async fn init_writes_a_config_and_a_persona_that_load() {
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    let (config_path, persona_path) = written(dir.path());
    assert_eq!(mode(&config_path), 0o600, "the config points at the token");
    assert_eq!(mode(&persona_path), 0o600);

    let cfg = load_config(&config_path).expect("the written config loads");
    assert_eq!(cfg.user_id, ME);
    assert_eq!(cfg.rooms, vec![ROOM_ID.to_owned()]);
    assert_eq!(cfg.persona_file, Some(persona_path));
    assert_eq!(cfg.state_dir, dir.path().join("state"));
    let brain = cfg.brain.expect("it configures a brain");
    assert_eq!(brain.kind, agent_room::config::BrainKind::OpenaiCompat);
    assert_eq!(
        brain.openai_compat.expect("the section").model,
        "qwen3".to_owned()
    );
}

#[tokio::test]
async fn the_state_directory_is_made_private_up_front() {
    // It ends up holding the room's transcript, so 0700 is not optional - and a
    // directory created on the way past by `create_dir_all` gets the umask.
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    assert_eq!(mode(&dir.path().join("state")), 0o700);
}

#[tokio::test]
async fn the_written_policy_is_the_shipped_default() {
    // A friend's first config must BE the default policy, not a copy of it.
    // The block is dumped from `PolicyConfig::default()` for exactly this
    // reason: a hand-written copy is a second place for the defaults to live,
    // and it goes stale the first time one of them changes.
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    let cfg = load_config(&written(dir.path()).0).expect("it loads");
    assert_eq!(cfg.policy, PolicyConfig::default());
    assert_eq!(cfg.policy.budgets.per_hour_max, 30);
    assert_eq!(
        cfg.policy.heartbeat_minutes, 0,
        "unprompted speech is opt-in"
    );
    assert_eq!(cfg.tls, TlsConfig::default());
    assert!(cfg.tls.verify, "a written tls block never turns verify off");
}

#[tokio::test]
async fn the_persona_carries_the_name_and_says_what_is_left_to_fill_in() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path()).with("--display-name", "Riku");
    flags.homeserver = home.base_url.clone();
    assert_eq!(init(&flags).await, 0);
    let persona = std::fs::read_to_string(written(dir.path()).1).expect("the persona");
    assert!(persona.starts_with("I am Riku,"));
    assert!(
        !persona.contains("agent-room init"),
        "the instructions to the human must not reach the model"
    );
    assert!(count_blanks(&persona) > 0);
}

#[tokio::test]
async fn without_a_display_name_the_persona_is_named_after_the_account() {
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    let persona = std::fs::read_to_string(written(dir.path()).1).expect("the persona");
    assert!(persona.starts_with("I am bot-a,"));
}

#[tokio::test]
async fn the_claude_brain_stands_in_the_state_dir_unless_told_otherwise() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let flags = Flags::new(dir.path())
        .claude()
        .with("--claude-model", "haiku");
    assert_eq!(init(&flags).await, 0);
    let cfg = load_config(&written(dir.path()).0).expect("it loads");
    let claude = cfg
        .brain
        .expect("a brain")
        .claude_code
        .expect("the claude section");
    assert_eq!(claude.model, "haiku");
    assert_eq!(claude.cwd, Some(dir.path().join("state")));
    assert_eq!(
        claude.allowed_tools,
        vec!["Read", "Grep", "Glob", "WebSearch"]
    );
}

#[tokio::test]
async fn a_claude_cwd_is_honoured() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let work = dir.path().join("work");
    let flags = Flags::new(dir.path())
        .claude()
        .with("--claude-cwd", &work.display().to_string());
    assert_eq!(init(&flags).await, 0);
    let cfg = load_config(&written(dir.path()).0).expect("it loads");
    assert_eq!(
        cfg.brain.expect("a brain").claude_code.expect("it").cwd,
        Some(work)
    );
}

#[tokio::test]
async fn several_rooms_all_reach_the_config() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path());
    flags.rooms.push("!second:example.com".to_owned());
    assert_eq!(init(&flags).await, 0);
    let cfg = load_config(&written(dir.path()).0).expect("it loads");
    assert_eq!(
        cfg.rooms,
        vec![ROOM_ID.to_owned(), "!second:example.com".to_owned()]
    );
}

#[tokio::test]
async fn the_tls_block_is_written_from_the_certificate_flags() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let (cert, key, ca) = (
        dir.path().join("c.crt"),
        dir.path().join("c.key"),
        dir.path().join("ca.pem"),
    );
    for path in [&cert, &key, &ca] {
        std::fs::write(path, "PEM").expect("write");
    }
    let flags = Flags::new(dir.path())
        .with("--tls-cert", &cert.display().to_string())
        .with("--tls-key", &key.display().to_string())
        .with("--tls-ca", &ca.display().to_string());
    assert_eq!(init(&flags).await, 0);
    let cfg = load_config(&written(dir.path()).0).expect("it loads");
    assert!(cfg.tls.enabled);
    assert_eq!(
        (cfg.tls.client_cert, cfg.tls.client_key, cfg.tls.ca_file),
        (Some(cert), Some(key), Some(ca))
    );
}

// -- what it refuses ---------------------------------------------------------

#[tokio::test]
async fn init_refuses_to_overwrite_what_is_already_there() {
    // The guard that stands between `init` and somebody's working agent.
    //
    // Teeth: drop the check in `prepare` and this test fails, because the
    // second run silently replaces a config that may be the only copy of a
    // hand-tuned persona.
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    let (config_path, persona_path) = written(dir.path());
    std::fs::write(
        &persona_path,
        "I am a persona somebody spent an evening on.\n",
    )
    .expect("write");
    let before = std::fs::read_to_string(&config_path).expect("read");

    assert_eq!(init(&Flags::new(dir.path())).await, 2);
    assert!(
        std::fs::read_to_string(&persona_path)
            .expect("read")
            .starts_with("I am a persona")
    );
    assert_eq!(std::fs::read_to_string(&config_path).expect("read"), before);
}

#[tokio::test]
async fn force_overwrites_both_files() {
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    std::fs::write(written(dir.path()).1, "old persona\n").expect("write");
    let flags = Flags::new(dir.path()).with("--force", "");
    assert_eq!(init(&flags).await, 0);
    assert!(
        std::fs::read_to_string(written(dir.path()).1)
            .expect("read")
            .starts_with("I am bot-a,")
    );
}

#[tokio::test]
async fn an_openai_brain_without_an_endpoint_is_refused() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path());
    flags.openai_base_url = None;
    flags.openai_model = None;
    assert_eq!(init(&flags).await, 2);
    assert!(
        !dir.path().join("config").exists(),
        "nothing is written when the flags do not add up"
    );
}

#[tokio::test]
async fn half_a_client_certificate_is_refused() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let flags = Flags::new(dir.path()).with("--tls-cert", "/nowhere/c.crt");
    assert_eq!(init(&flags).await, 2);
}

#[tokio::test]
async fn a_missing_token_file_is_refused() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path());
    flags.token_file = Some(dir.path().join("nope"));
    assert_eq!(init(&flags).await, 2);
}

#[tokio::test]
async fn a_group_readable_token_file_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tmpdir");
    let loose = dir.path().join("loose");
    std::fs::write(&loose, "syt_x").expect("write");
    std::fs::set_permissions(&loose, PermissionsExt::from_mode(0o644)).expect("chmod");
    let mut flags = Flags::new(dir.path());
    flags.token_file = Some(loose);
    assert_eq!(init(&flags).await, 2);
}

// -- the login path ----------------------------------------------------------

#[tokio::test]
async fn a_credential_belonging_to_somebody_else_is_refused() {
    // A typo in `--user` otherwise writes a config that connects as somebody
    // else, and everything would "work" while the room saw the wrong account.
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.whoami_user = "@someone-else:example.com".to_owned());
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path()).with("--display-name", "Riku");
    flags.homeserver = home.base_url.clone();
    assert_eq!(init(&flags).await, 2);
    assert!(!dir.path().join("config").exists());
}

#[tokio::test]
async fn a_display_name_with_a_token_file_still_reaches_the_homeserver() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path()).with("--display-name", "Riku");
    flags.homeserver = home.base_url.clone();
    assert_eq!(init(&flags).await, 0);
    home.with(|state| {
        assert_eq!(state.display_names, vec!["Riku".to_owned()]);
        assert!(
            state.passwords.is_empty(),
            "a token file is not a reason to log in again"
        );
    });
}

#[tokio::test]
async fn a_display_name_the_server_refuses_is_reported_and_the_files_are_still_written() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    home.with(|state| state.display_name_error = Some("not allowed here".to_owned()));
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path()).with("--display-name", "Riku");
    flags.homeserver = home.base_url.clone();
    assert_eq!(
        init(&flags).await,
        2,
        "init says something it was asked to do did not happen"
    );
    assert!(
        written(dir.path()).0.exists(),
        "the credential is already good; the config belongs with it"
    );
}

/// The password path cannot read the process's own stdin in a suite that runs
/// its cases side by side, so the password is handed to `run_with` instead. The
/// flags still go through clap, and the promise under test - the password
/// reaches the homeserver ONCE and lands in no file - is what is asserted.
fn init_args(flags: &Flags) -> InitArgs {
    let cli = Cli::try_parse_from(flags.argv()).expect("the flags parse");
    match cli.command {
        Command::Init(args) => *args,
        other => panic!("init parsed as {other:?}"),
    }
}

#[tokio::test]
async fn the_password_logs_in_once_and_is_never_written_anywhere() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path());
    flags.homeserver = home.base_url.clone();
    flags.token_file = None;
    flags.password_from_stdin = true;
    let code = run_with(
        &init_args(&flags),
        &PasswordSource::Given(PASSWORD.to_owned()),
    )
    .await;
    assert_eq!(code, 0);

    home.with(|state| {
        assert_eq!(
            state.passwords,
            vec![PASSWORD.to_owned()],
            "the password was not used to log in"
        );
    });
    let cfg = load_config(&written(dir.path()).0).expect("the written config loads");
    assert!(cfg.password.is_none(), "no password may reach the config");
    let token_path = cfg.access_token_file.expect("a cached token");
    assert_eq!(
        std::fs::read_to_string(&token_path).expect("read"),
        "syt_from_login"
    );
    assert_eq!(mode(&token_path), 0o600);

    for path in walk(dir.path()) {
        let body = std::fs::read(&path).expect("read");
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains(PASSWORD),
            "the password was written to {}",
            path.display()
        );
    }
}

#[tokio::test]
async fn an_empty_password_is_refused() {
    let home = FakeHomeserver::start(ROOM_ID, ME).await;
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut flags = Flags::new(dir.path());
    flags.homeserver = home.base_url.clone();
    flags.token_file = None;
    flags.password_from_stdin = true;
    let code = run_with(&init_args(&flags), &PasswordSource::Given(String::new())).await;
    assert_eq!(code, 2);
    assert!(!dir.path().join("config").exists());
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

// -- the template ------------------------------------------------------------

#[test]
fn the_persona_template_travels_inside_the_binary() {
    // `init` must work from a copied binary, where there is no `examples/`.
    let body = persona_body(PERSONA_TEMPLATE).expect("the shipped template has a separator");
    assert!(body.starts_with("I am <name>,"));
    assert!(
        render_persona("Riku", PERSONA_TEMPLATE)
            .expect("it renders")
            .starts_with("I am Riku,")
    );
}

#[test]
fn a_template_without_a_separator_is_a_loud_failure() {
    let exc = persona_body("no separator here\n").expect_err("it must refuse");
    assert!(format!("{exc}").contains("separator"), "{exc}");
}

#[tokio::test]
async fn the_config_is_yaml_a_person_can_edit() {
    // No tags, no anchors: a friend opens this file and changes a number.
    let dir = tempfile::tempdir().expect("tmpdir");
    assert_eq!(init(&Flags::new(dir.path())).await, 0);
    let text = std::fs::read_to_string(written(dir.path()).0).expect("read");
    assert!(text.starts_with("# agent-room connector config"));
    assert!(!text.contains("!!"), "no YAML tags: {text}");
    assert!(
        !text.contains(" &") && !text.contains(" *"),
        "no anchors: {text}"
    );
    // A pair written as anything but a two-item sequence would come back as a
    // parse error rather than a range.
    assert!(text.contains("  backoff_s:\n  - 5.0\n  - 40.0\n"), "{text}");
    let cfg = load_config(&written(dir.path()).0).expect("it loads");
    assert!((cfg.policy.backoff_s.0 - 5.0).abs() < f64::EPSILON);
    assert!((cfg.policy.backoff_s.1 - 40.0).abs() < f64::EPSILON);
}

/// Room version 12 homeservers mint room ids with no `:server` part. `init`
/// must take them: the first real room this project ever ran in was one.
#[test]
fn init_accepts_a_room_id_without_a_server_part() {
    for room in [
        "!Q3kd0oA9tYr4f7c2b8VnL1xwE5mZpHsGjR6uTqK9yWc",
        "!room:example.com",
        "#alias:example.com",
    ] {
        let parsed = Cli::try_parse_from([
            "agent-room",
            "init",
            "--homeserver",
            "https://matrix.example.com",
            "--user",
            "@a:example.com",
            "--room",
            room,
            "--brain",
            "openai_compat",
            "--openai-base-url",
            "http://localhost:11434/v1",
            "--openai-model",
            "m",
            "--token-file",
            "/dev/null",
        ]);
        assert!(parsed.is_ok(), "{room} was refused: {:?}", parsed.err());
    }
    for bad in ["#alias", "room:example.com", "!", "!has space:example.com"] {
        let parsed = Cli::try_parse_from([
            "agent-room",
            "init",
            "--homeserver",
            "https://matrix.example.com",
            "--user",
            "@a:example.com",
            "--room",
            bad,
            "--brain",
            "openai_compat",
            "--openai-base-url",
            "http://localhost:11434/v1",
            "--openai-model",
            "m",
            "--token-file",
            "/dev/null",
        ]);
        assert!(parsed.is_err(), "{bad} was accepted");
    }
}
