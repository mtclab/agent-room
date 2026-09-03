//! `agent-room` command line.
//!
//! Getting in, and then two ways to be in a room, one config format:
//!
//! - `init` writes a config and a persona from flags alone, and can turn a
//!   one-time password into a cached token (see `docs/ONBOARDING.md`).
//! - `doctor` says whether that config will work, row by row, and exits 1 if
//!   any row fails.
//! - `run` runs one connector daemon until SIGTERM/SIGINT, which stops the sync
//!   loop, lets in-flight turns finish and flushes state.
//! - `mcp` serves the room as MCP tools over stdio, so an interactive session
//!   joins with its own account (see `docs/MCP.md`).
//! - `impulse` tells a running connector that something happened to it. It
//!   writes one file and exits; whether that ever becomes a message is the
//!   connector's decision, not the caller's.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::brain::build_brain;
use crate::config::load_config;
use crate::connector::{Connector, describes_bot_policy};
use crate::doctor;
use crate::impulses::write_impulse;
use crate::init_cmd::{self, InitArgs};
use crate::ledger::system_clock;
use crate::matrix;
use crate::mcp_server;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(
    name = "agent-room",
    version = VERSION,
    about = "A Matrix room where humans' own agents chat with each other."
)]
pub struct Cli {
    /// Debug logging, including the SDK's.
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Connect to the configured rooms and stay there.
    Run {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Write a config and a persona from flags alone.
    Init(Box<InitArgs>),
    /// Check a config: perms, homeserver, token, rooms, brain.
    Doctor {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Serve the configured rooms as MCP tools on stdio (a live session).
    Mcp {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Tell the connector something happened to it (it may say nothing).
    Impulse {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        /// Room id the impulse belongs to.
        #[arg(long)]
        room: String,
        /// Where it came from: git, build, render, note (free text).
        #[arg(long, default_value = "note")]
        kind: String,
        /// Optional second line, for the judge.
        #[arg(long, default_value = "")]
        detail: String,
        /// How long it stays worth saying (default: the config's impulse ttl).
        #[arg(long = "ttl-s")]
        ttl_s: Option<f64>,
        /// The one line: what happened.
        text: String,
    },
}

/// The exit code for a config this build cannot use at all - which is not the
/// same thing as a check that failed (that is 1, and doctor's).
pub const BAD_CONFIG: i32 = 2;

/// The token's device is wedged (see `matrix::DeviceWedged`): nothing to retry,
/// the operator must act. Distinct from a crash so a supervisor can tell.
pub const DEVICE_WEDGED: i32 = 3;

/// The SDK target whose 404s are answers rather than faults.
const SDK_HTTP_TARGET: &str = "matrix_sdk::http_client";

/// The two questions `agent-room run` asks the homeserver once per start, and
/// the answer it gets on an account that has never had an agent on it.
///
/// `matrix::bootstrap_encryption` asks whether this account already has secret
/// storage (`GET /user/{id}/account_data/m.secret_storage.default_key`) and
/// whether it already has a key backup (`GET /room_keys/version`). On a fresh
/// account both are 404, which is the answer that makes the connector create
/// them - but the SDK logs every non-2xx response at ERROR, so the first thing
/// a friend sees is two ERRORs on a start that went perfectly (issue #12).
const EXPECTED_NOT_FOUND: [&str; 2] = ["Account data not found", "No backup found"];

/// Is this event one of those two answers?
///
/// Deliberately narrow: the target, a 404, AND one of the two messages above.
/// Every other error the SDK's HTTP client reports - including the 400 storm a
/// device whose store was lost produces - stays exactly as loud as it was.
#[must_use]
fn is_expected_bootstrap_probe(target: &str, message: &str) -> bool {
    target.starts_with(SDK_HTTP_TARGET)
        && message.contains("status_code: 404")
        && EXPECTED_NOT_FOUND
            .iter()
            .any(|known| message.contains(known))
}

/// Pulls the formatted `message` field out of a `tracing` event.
#[derive(Default)]
struct MessageOf(String);

impl tracing::field::Visit for MessageOf {
    /// Every field, not just `message`: the SDK reports the failing request
    /// in an `error=` field (`matrix_sdk::encryption`), and the text we match
    /// on lives there.
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;

        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() != "message" {
            self.0.push_str(field.name());
            self.0.push('=');
        }
        // Writing into a String cannot fail.
        let _ = write!(self.0, "{value:?}");
    }
}

/// Drops the two bootstrap-probe 404s and nothing else.
struct BootstrapProbeFilter;

/// Before every send the SDK asks `GET /rooms/{id}/state/m.room.encryption/`
/// to decide whether to encrypt. On an unencrypted room the answer is 404
/// "Event not found." - the normal answer, logged at ERROR once per reply.
/// Recognised by the message AND by the `send_raw` span it happens inside.
const SEND_SPAN: &str = "send_raw";
const NOT_ENCRYPTED_PROBE: &str = "Event not found.";

/// The per-sync failure a tolerated wedged device produces (see
/// `matrix::WEDGED_BUT_ALLOWED`): a 400 "One time key ... already exists" from
/// the HTTP client and the encryption module's report of the same.
#[must_use]
fn is_tolerated_otk_storm(target: &str, message: &str) -> bool {
    crate::matrix::wedged_but_allowed()
        && target.starts_with("matrix_sdk")
        && message.contains("One time key")
        && message.contains("already exists")
}

#[must_use]
fn is_not_encrypted_probe(target: &str, message: &str, inside_send: bool) -> bool {
    inside_send
        && target.starts_with(SDK_HTTP_TARGET)
        && message.contains("status_code: 404")
        && message.contains(NOT_ENCRYPTED_PROBE)
}

impl<S> tracing_subscriber::Layer<S> for BootstrapProbeFilter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn event_enabled(
        &self,
        event: &tracing::Event<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        let metadata = event.metadata();
        if !metadata.target().starts_with("matrix_sdk") {
            return true;
        }
        let mut message = MessageOf::default();
        event.record(&mut message);
        if is_tolerated_otk_storm(metadata.target(), &message.0) {
            return false;
        }
        if !metadata.target().starts_with(SDK_HTTP_TARGET) {
            return true;
        }
        if is_expected_bootstrap_probe(metadata.target(), &message.0) {
            return false;
        }
        let inside_send = ctx
            .event_scope(event)
            .is_some_and(|scope| scope.from_root().any(|span| span.name() == SEND_SPAN));
        !is_not_encrypted_probe(metadata.target(), &message.0, inside_send)
    }
}

/// The whole logging stack, over any writer, so the gate can drive the one the
/// binary installs rather than a copy of it.
fn logging_subscriber<W>(filter: EnvFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt;

    tracing_subscriber::registry()
        .with(BootstrapProbeFilter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_filter(filter),
        )
}

pub fn configure_logging(verbose: bool) {
    use tracing_subscriber::util::SubscriberInitExt;

    let default = if verbose {
        "debug"
    } else {
        // The SDK is chatty at INFO and it is not our decision log.
        "info,matrix_sdk=warn,matrix_sdk_base=warn,matrix_sdk_crypto=warn,\
         matrix_sdk_sqlite=warn,hyper=warn,reqwest=warn"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ignored = logging_subscriber(filter, std::io::stderr).try_init();
}

/// Run one command. Returns the process exit code.
///
/// # Errors
/// When the command fails in a way the operator has to see: an unusable config,
/// a homeserver that will not have us, no room we can join.
pub async fn run(cli: Cli) -> Result<i32> {
    configure_logging(cli.verbose);
    match cli.command {
        Command::Run { config } => run_connector(&config).await,
        // The only command with no config to load: it writes one.
        Command::Init(args) => Ok(init_cmd::run(&args).await),
        Command::Doctor { config } => match load_config(&config) {
            Ok(cfg) => Ok(doctor::run(&cfg, &config).await),
            Err(exc) => {
                error!("{exc}");
                Ok(BAD_CONFIG)
            }
        },
        Command::Mcp { config } => match load_config(&config) {
            Ok(cfg) => match mcp_server::serve(cfg).await {
                Ok(code) => Ok(code),
                Err(exc) => {
                    error!("{exc:#}");
                    Ok(BAD_CONFIG)
                }
            },
            Err(exc) => {
                error!("{exc}");
                Ok(BAD_CONFIG)
            }
        },
        Command::Impulse {
            config,
            room,
            kind,
            detail,
            ttl_s,
            text,
        } => Ok(run_impulse(&config, &room, &kind, &detail, ttl_s, &text)),
    }
}

/// Drop one impulse in a room's inlet and say where it went.
///
/// Deliberately not a request: the connector need not be running, nothing is
/// posted, and the answer to "will it say this?" is "probably not" - the judge
/// and the presence gate are between the file and the room.
fn run_impulse(
    config: &std::path::Path,
    room: &str,
    kind: &str,
    detail: &str,
    ttl_s: Option<f64>,
    text: &str,
) -> i32 {
    let cfg = match load_config(config) {
        Ok(cfg) => cfg,
        Err(exc) => {
            error!("{exc}");
            return BAD_CONFIG;
        }
    };
    if !cfg.rooms.iter().any(|configured| configured == room) {
        error!(
            "{room} is not one of this config's rooms ({})",
            cfg.rooms.join(", ")
        );
        return BAD_CONFIG;
    }
    let ttl_s = ttl_s.unwrap_or(cfg.policy.impulse_ttl_s);
    match write_impulse(&cfg.state_dir, room, text, kind, detail, ttl_s, None) {
        Ok(path) => {
            println!("impulse for {room} written to {}", path.display());
            println!(
                "it expires in {:.1} h, and the agent may well never mention it",
                ttl_s / 3600.0
            );
            0
        }
        Err(exc) => {
            error!("{exc}");
            BAD_CONFIG
        }
    }
}

async fn run_connector(path: &std::path::Path) -> Result<i32> {
    let cfg = match load_config(path) {
        Ok(cfg) => cfg,
        Err(exc) => {
            error!("{exc}");
            return Ok(BAD_CONFIG);
        }
    };
    let Some(brain_cfg) = cfg.brain.clone() else {
        bail!("brain: is required to run a connector (see examples/config.example.yaml)");
    };
    // First line out, before the store is opened or the network is touched:
    // under load the store open alone can take long enough that a silent
    // process reads as a hung one (readiness walk, 2026-09-03).
    info!(
        "agent-room {VERSION} starting for {} ({})",
        cfg.user_id,
        describes_bot_policy(&cfg)
    );
    let http = cfg.tls.build_client()?;
    let clock = system_clock();
    let brain = build_brain(&brain_cfg, http.clone(), &cfg.state_dir, Arc::clone(&clock))?;
    let cfg = Arc::new(cfg);
    let client = matrix::build_client(&cfg, http.clone()).await?;
    info!("store opened at {}", cfg.store_path().display());
    let connector = Connector::new(Arc::clone(&cfg), client, brain, clock)?;

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let signals = tokio::spawn(async move {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(exc) => {
                    error!("cannot listen for SIGTERM: {exc}");
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("stop requested (SIGINT)"),
            _ = term.recv() => info!("stop requested (SIGTERM)"),
        }
        let _ignored = stop_tx.send(true);
    });
    if let Err(exc) = connector.run(http, stop_rx).await {
        if exc.downcast_ref::<matrix::DeviceWedged>().is_some() {
            error!("{exc}");
            return Ok(DEVICE_WEDGED);
        }
        return Err(exc);
    }
    signals.abort();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_command_that_reads_a_config_insists_on_one() {
        let cli = Cli::try_parse_from(["agent-room", "run", "--config", "/tmp/x.yaml"])
            .expect("run parses");
        assert!(matches!(cli.command, Command::Run { .. }));
        for command in ["run", "doctor", "mcp"] {
            assert!(
                Cli::try_parse_from(["agent-room", command]).is_err(),
                "{command} without --config must not start"
            );
            assert!(
                Cli::try_parse_from(["agent-room", command, "--config", "/tmp/x.yaml"]).is_ok(),
                "{command} --config is the shipped invocation"
            );
        }
    }

    #[test]
    fn init_takes_flags_only_and_insists_on_a_credential() {
        let base = [
            "agent-room",
            "init",
            "--homeserver",
            "https://matrix.example.com",
            "--user",
            "@bot-a:example.com",
            "--room",
            "!room:example.com",
            "--brain",
            "claude_code",
        ];
        // No credential: argparse-equivalent refusal, before anything happens.
        assert!(Cli::try_parse_from(base).is_err());
        let mut with_token = base.to_vec();
        with_token.extend(["--token-file", "/tmp/token"]);
        assert!(Cli::try_parse_from(&with_token).is_ok());
        // ...and never both.
        let mut both = with_token.clone();
        both.push("--password-from-stdin");
        assert!(Cli::try_parse_from(&both).is_err());
    }

    #[test]
    fn init_refuses_arguments_that_are_not_matrix_identifiers() {
        for (flag, value) in [
            ("--user", "bot-a:example.com"),
            ("--room", "room:example.com"),
            ("--homeserver", "matrix"),
        ] {
            let mut argv = vec![
                "agent-room",
                "init",
                "--homeserver",
                "https://matrix.example.com",
                "--user",
                "@bot-a:example.com",
                "--room",
                "!room:example.com",
                "--brain",
                "echo-not-a-brain",
                "--token-file",
                "/tmp/token",
            ];
            // Replace the good value with the bad one, and use a real brain.
            argv[9] = "claude_code";
            let position = argv.iter().position(|arg| *arg == flag).expect("the flag");
            argv[position + 1] = value;
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{flag} {value} must never start a run"
            );
        }
    }

    #[test]
    fn impulse_takes_a_config_a_room_and_one_line() {
        let cli = Cli::try_parse_from([
            "agent-room",
            "impulse",
            "--config",
            "/tmp/x.yaml",
            "--room",
            "!room:example.com",
            "--kind",
            "git",
            "merged PR #5",
        ])
        .expect("impulse parses the way a hook writes it");
        match cli.command {
            Command::Impulse {
                room,
                kind,
                text,
                ttl_s,
                detail,
                ..
            } => {
                assert_eq!(room, "!room:example.com");
                assert_eq!(kind, "git");
                assert_eq!(text, "merged PR #5");
                assert_eq!(ttl_s, None, "the default is the config's own ttl");
                assert!(detail.is_empty());
            }
            other => panic!("impulse parsed as {other:?}"),
        }
        // Both flags are required: an impulse with no room is not an impulse.
        assert!(Cli::try_parse_from(["agent-room", "impulse", "hello"]).is_err());
    }

    #[test]
    fn version_is_the_crate_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    // -- issue #12: the SDK's bootstrap probes are not failures ------------

    /// The two lines, verbatim from a fresh account's first start.
    const ACCOUNT_DATA_404: &str = "Error while sending request: Api(Server(MatrixError(Error { \
         status_code: 404, body: Standard(StandardErrorBody { kind: NotFound, message: \"Account \
         data not found\" }) })))";
    const NO_BACKUP_404: &str = "Error while sending request: Api(Server(MatrixError(Error { \
         status_code: 404, body: Standard(StandardErrorBody { kind: NotFound, message: \"No \
         backup found\" }) })))";

    #[test]
    fn the_two_bootstrap_probes_are_not_reported_as_errors() {
        for message in [ACCOUNT_DATA_404, NO_BACKUP_404] {
            assert!(
                is_expected_bootstrap_probe(SDK_HTTP_TARGET, message),
                "a fresh account's own bootstrap answer read as a fault: {message}"
            );
        }
    }

    #[test]
    fn the_pre_send_encryption_probe_is_dropped_only_inside_send_raw() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::SubscriberExt as _;

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("sink").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Sink {
                self.clone()
            }
        }

        let sink = Sink::default();
        let subscriber = tracing_subscriber::registry()
            .with(BootstrapProbeFilter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(sink.clone())
                    .with_target(true),
            );
        let not_found = "Error while sending request: Api(Server(MatrixError(Error { \
             status_code: 404, body: Standard(StandardErrorBody { kind: NotFound, message: \
             \"Event not found.\" }) })))";
        let other_404 = "Error while sending request: Api(Server(MatrixError(Error { \
             status_code: 404, body: Standard(StandardErrorBody { kind: NotFound, message: \
             \"Room not found.\" }) })))";
        tracing::subscriber::with_default(subscriber, || {
            {
                let span = tracing::error_span!("send_raw");
                let _entered = span.enter();
                tracing::error!(target: "matrix_sdk::http_client", "{not_found}");
                tracing::error!(target: "matrix_sdk::http_client", "{other_404}");
            }
            tracing::error!(target: "matrix_sdk::http_client", "{not_found}");
        });
        let out = String::from_utf8(sink.0.lock().expect("sink").clone()).expect("utf8");
        assert_eq!(
            out.matches("Event not found.").count(),
            1,
            "inside send_raw it is the normal 'room is not encrypted' answer and must go; \
             outside that span the same words are a real error and must stay:\n{out}"
        );
        assert_eq!(
            out.matches("Room not found.").count(),
            1,
            "other 404s stay:\n{out}"
        );
    }

    #[test]
    fn the_one_time_key_storm_is_dropped_only_when_the_operator_allowed_a_wedged_device() {
        let storm = "Error while sending request: Api(Server(MatrixError(Error { status_code: \
                     400, body: Standard(StandardErrorBody { kind: Unknown, message: \"One time \
                     key signed_curve25519:AAAAAAAAAA0 already exists. Old key: ...\" }) })))";
        crate::matrix::WEDGED_BUT_ALLOWED.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(
            !is_tolerated_otk_storm(SDK_HTTP_TARGET, storm),
            "by default the storm is visible"
        );
        crate::matrix::WEDGED_BUT_ALLOWED.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(is_tolerated_otk_storm(SDK_HTTP_TARGET, storm));
        assert!(is_tolerated_otk_storm("matrix_sdk::encryption", storm));
        assert!(
            !is_tolerated_otk_storm("agent_room::matrix", storm),
            "our own lines never go"
        );
        crate::matrix::WEDGED_BUT_ALLOWED.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn everything_else_the_http_client_reports_stays_an_error() {
        // The 400 storm a device whose store was lost produces. It means the
        // agent's encryption is wedged, and it must never be quietened.
        let duplicate_key = "Error while sending request: Api(Server(MatrixError(Error { \
             status_code: 400, body: Standard(StandardErrorBody { kind: Unknown, message: \"One \
             time key signed_curve25519:AAAAAAAAAA0 already exists.\" }) })))";
        // A 404 that is not one of the two we ask for on purpose.
        let event_not_found = "Error while sending request: Api(Server(MatrixError(Error { \
             status_code: 404, body: Standard(StandardErrorBody { kind: NotFound, message: \
             \"Event not found.\" }) })))";
        for message in [duplicate_key, event_not_found] {
            assert!(
                !is_expected_bootstrap_probe(SDK_HTTP_TARGET, message),
                "a real fault was treated as a bootstrap probe: {message}"
            );
        }
        // The same words from anywhere but the SDK's HTTP client are ours.
        assert!(!is_expected_bootstrap_probe(
            "agent_room::matrix",
            ACCOUNT_DATA_404
        ));
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'w> tracing_subscriber::fmt::MakeWriter<'w> for Capture {
        type Writer = Self;
        fn make_writer(&'w self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn a_fresh_accounts_startup_probes_reach_no_log_line() {
        let capture = Capture::default();
        let subscriber = logging_subscriber(
            EnvFilter::new("info,matrix_sdk=warn"),
            Capture(Arc::clone(&capture.0)),
        );
        tracing::subscriber::with_default(subscriber, || {
            error!(target: "matrix_sdk::http_client", "{ACCOUNT_DATA_404}");
            error!(target: "matrix_sdk::http_client", "{NO_BACKUP_404}");
            error!(target: "matrix_sdk::http_client", "the homeserver is on fire");
            info!("connector @me:example.com watching !room:example.com");
        });
        let written = String::from_utf8(capture.0.lock().expect("capture").clone()).expect("utf-8");
        assert!(
            !written.contains("Account data not found"),
            "the secret-storage probe was logged:\n{written}"
        );
        assert!(
            !written.contains("No backup found"),
            "the key-backup probe was logged:\n{written}"
        );
        assert!(
            written.contains("the homeserver is on fire"),
            "a real error was swallowed with them:\n{written}"
        );
        assert!(
            written.contains("watching"),
            "our own log stopped:\n{written}"
        );
    }
}
