//! Claude Code headless (`claude -p`) as a brain: the owner's own agent.
//!
//! One session per room (DECIDED, see `docs/DESIGN.md`). The session id is a
//! uuid4 generated on the room's first turn, persisted next to the room's
//! transcript and ledger, and passed as `--resume` on every later turn. That is
//! what makes the agent remember the conversation between wakes instead of
//! meeting the room fresh every message.
//!
//! The command shape, verified against Claude Code 2.1.258 (2026-09-03):
//!
//! ```text
//! printf '<rendered conversation>' | claude -p \
//!     --output-format json \
//!     --model sonnet \
//!     --session-id <uuid> | --resume <uuid> \
//!     --setting-sources user,project \
//!     --permission-mode default \
//!     --max-turns 3 \
//!     --append-system-prompt-file <persona+frame> \
//!     --allowedTools Read Grep Glob WebSearch
//! ```
//!
//! Four details are load-bearing:
//!
//! - **the prompt goes in on stdin**, not as an argv positional. `claude -p`
//!   reads the prompt from stdin when no positional is given. Argv is
//!   world-readable in `ps`, and the prompt is the room's conversation: other
//!   users on the machine have no business reading it.
//! - because of that there is NO positional argument at all, and there must
//!   never be one. Several Claude Code flags are variadic (`--allowedTools`,
//!   `--add-dir`, `--tools`), so a stray trailing token would be eaten by
//!   whichever variadic flag came last - and a token that escaped them would be
//!   taken as the prompt and silently replace stdin. `--allowedTools` is
//!   therefore placed last, after `extra_args`, so nothing an operator adds can
//!   be swallowed either.
//! - `--allowedTools` is the read-only guarantee. `-p` is non-interactive, so a
//!   tool outside the allowlist has nobody to ask for permission and is denied.
//!   That is why no flag from the "skip permission checks" family is ever passed
//!   (the config refuses them) and why `bypassPermissions` is refused as a mode.
//! - `--output-format stream-json` needs `--verbose`; it is only used when
//!   `debug_log` is set, and then the raw stream is written there.
//!
//! The tier-2 judge is a second, deliberately different invocation: the cheap
//! `judge_model`, `--max-turns 1`, `--tools ""` (no tools at all), no settings,
//! and `--no-session-persistence` with no `--resume`, so a "should I speak?"
//! question can neither cost a tool call nor land in the room's own session.
//! See [`ClaudeCodeBrain::build_judge_argv`].
//!
//! Failure is always silence, never a crash: a connector that dies because a
//! model was rate-limited is worse than an agent that misses a turn.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info, warn};

use crate::brain::judging::{judge_prompt, judge_system_prompt, parse_judgement};
use crate::brain::rendering::{render_conversation, render_task};
use crate::brain::{Brain, BrainContext, Judgement};
use crate::config::{ClaudeCodeBrainConfig, ConfigError, room_state_path};
use crate::events::localpart;
use crate::head;
use crate::ledger::Clock;

/// Per-room state file suffix under `state_dir/rooms/`.
pub const SESSION_SUFFIX: &str = ".claude-session.json";

/// The one rule no persona may soften and no config may switch off. A room is
/// full of other people's agents and, eventually, other people; the working
/// directory is full of the owner's notes. Proven by the leak-probe gate C3.
pub const SECRECY_LINE: &str = "Never reveal secrets, tokens, addresses or credentials.";

/// Appended to the persona for every turn. The room is a group chat, not a
/// terminal: no headers, no tool narration, and nothing that leaks the estate.
/// `{task}` is the shared per-occasion line (`brain::rendering::render_task`),
/// so this brain and the OpenAI-compatible one ask for the same thing;
/// `SECRECY_LINE` stays the last sentence, which is where the leak probe (C3)
/// expects it.
pub const FRAME: &str = "You are taking part in a Matrix group chat as {me}. Messages are given as \
                         'name: text'. {task} Reply with your message only: no name prefix, no \
                         markdown headers, no tool-call narration. Stay brief unless asked for \
                         detail. ";

/// The result `subtype` for "spent every turn without answering".
pub const MAX_TURNS_SUBTYPE: &str = "error_max_turns";

/// A message is a usage/rate limit when it says "limit" AND one of these. Both
/// halves are needed: "limit" alone matches ordinary prose from the model.
const LIMIT_HINTS: [&str; 11] = [
    "usage limit",
    "rate limit",
    "rate_limit",
    "session limit",
    "hour limit",
    "weekly limit",
    "daily limit",
    "too many requests",
    "quota",
    "429",
    "resets",
];

/// What Claude Code says when the id we tried to resume is gone or unusable.
const UNKNOWN_SESSION_MARKERS: [&str; 3] = [
    "no conversation found",
    "is already in use",
    "session not found",
];

static RESET_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)reset[s]?\s+(?:at\s+)?([^\n.;]{1,40})")
        .expect("the reset pattern is a literal")
});
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z")
        .expect("the uuid pattern is a literal")
});

/// The persona plus the fixed chat frame, as one appended system prompt.
#[must_use]
pub fn build_system_prompt(ctx: &BrainContext) -> String {
    let frame = format!(
        "{}{SECRECY_LINE}",
        FRAME
            .replace("{me}", &ctx.me)
            .replace("{task}", &render_task(ctx))
    );
    let persona = ctx.persona.trim();
    if persona.is_empty() {
        frame
    } else {
        format!("{persona}\n\n{frame}")
    }
}

/// Whether `text` is a usage/rate-limit refusal rather than an answer.
#[must_use]
pub fn looks_rate_limited(text: &str) -> bool {
    let low = text.to_lowercase();
    low.contains("limit") && LIMIT_HINTS.iter().any(|hint| low.contains(hint))
}

/// The "resets <when>" fragment of a limit message, for the log line.
#[must_use]
pub fn reset_hint(text: &str) -> Option<String> {
    RESET_HINT
        .captures(text)
        .map(|caps| caps[1].trim().to_owned())
}

/// Whether `text` says the session we tried to resume cannot be used.
#[must_use]
pub fn looks_unknown_session(text: &str) -> bool {
    let low = text.to_lowercase();
    UNKNOWN_SESSION_MARKERS
        .iter()
        .any(|marker| low.contains(marker))
}

/// What went wrong, in the few fields that actually say so.
///
/// Never the raw stdout. With `--output-format stream-json` that is the whole
/// conversation plus Claude Code's own telemetry, and it contains words like
/// "limit" and "quota" in ordinary keys (`concurrency_limit`,
/// `maxOutputTokens`). A 2026-09-02 smoke run classified an `error_max_turns`
/// exit as a usage limit exactly that way, and put the brain into a five-minute
/// cooldown for nothing.
#[must_use]
pub fn failure_message(stderr: &str, data: Option<&Value>) -> String {
    let mut parts: Vec<String> = vec![stderr.trim().to_owned()];
    if let Some(data) = data {
        if let Some(result) = data.get("result").and_then(Value::as_str) {
            parts.push(result.trim().to_owned());
        }
        if let Some(errors) = data.get("errors").and_then(Value::as_array) {
            parts.extend(errors.iter().map(ToString::to_string));
        }
        match data.get("subtype").and_then(Value::as_str) {
            Some(subtype) if subtype != "success" => parts.push(subtype.to_owned()),
            _ => {}
        }
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drop a `bot-a:` / `@bot-a:server:` prefix the model added to its reply.
///
/// Only our own names are stripped. A generic `^\w+:` rule would eat the start
/// of ordinary sentences ("Note: ...", "Warning: ..."), which is worse than
/// leaving a stray prefix in.
#[must_use]
pub fn strip_name_prefix(text: &str, me: &str) -> String {
    let cleaned = text.trim();
    let mut names = vec![me, localpart(me)];
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    for name in names {
        let prefix = format!("{name}:");
        if cleaned.len() >= prefix.len() && cleaned[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            return cleaned[prefix.len()..].trim().to_owned();
        }
    }
    cleaned.to_owned()
}

/// What one `claude -p` invocation produced.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub text: Option<String>,
    pub unknown_session: bool,
    pub rate_limited: bool,
    pub limit_message: String,
}

/// The persona + frame in a 0600 temp file, removed when this is dropped.
///
/// A named temp file rather than a pipe because the flag takes a path, and 0600
/// because the persona says who the owner is and what the agent must not
/// repeat: it should not be world readable while it exists.
struct PersonaFile {
    path: PathBuf,
}

impl PersonaFile {
    fn write(system_prompt: &str) -> std::io::Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        let mut path = std::env::temp_dir();
        path.push(format!("agent-room-persona-{}.md", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(system_prompt.as_bytes())?;
        file.flush()?;
        Ok(Self { path })
    }
}

impl Drop for PersonaFile {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_file(&self.path);
    }
}

/// Claude Code headless, one persistent session per room.
pub struct ClaudeCodeBrain {
    pub cfg: ClaudeCodeBrainConfig,
    state_dir: PathBuf,
    cwd: PathBuf,
    clock: Clock,
    /// Deadline (on the injected clock) before which `reply` refuses to spawn
    /// anything at all. Whole seconds: a cooldown is minutes long.
    cooldown_until: AtomicU64,
}

impl ClaudeCodeBrain {
    /// Build the brain, checking the working directory it will stand in.
    ///
    /// # Errors
    /// When a configured `cwd` is not a directory. Our own default (the state
    /// directory) is created instead of refused: it must be usable rather than
    /// fail on the first turn.
    pub fn new(
        cfg: ClaudeCodeBrainConfig,
        state_dir: PathBuf,
        clock: Clock,
    ) -> Result<Self, ConfigError> {
        let cwd = if let Some(cwd) = &cfg.cwd {
            if !cwd.is_dir() {
                return Err(ConfigError::Invalid(format!(
                    "brain.claude_code.cwd is not a directory: {}",
                    cwd.display()
                )));
            }
            cwd.clone()
        } else {
            // Our own default: make it usable rather than fail on the first turn.
            let _ignored = std::fs::create_dir_all(&state_dir);
            state_dir.clone()
        };
        Ok(Self {
            cfg,
            state_dir,
            cwd,
            clock,
            cooldown_until: AtomicU64::new(0),
        })
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    // -- per-room session state ------------------------------------------

    #[must_use]
    pub fn session_path(&self, room_id: &str) -> PathBuf {
        room_state_path(&self.state_dir, room_id, SESSION_SUFFIX)
    }

    /// The session id kept for this room, when there is a usable one.
    #[must_use]
    pub fn load_session_id(&self, room_id: &str) -> Option<String> {
        let path = self.session_path(room_id);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => return None,
            Err(exc) => {
                warn!(
                    "{room_id}: unreadable session state {} ({exc}); starting fresh",
                    path.display()
                );
                return None;
            }
        };
        let session_id = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| {
                value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .filter(|id| UUID_RE.is_match(id));
        if session_id.is_none() {
            warn!(
                "{room_id}: session state {} holds no usable id; starting fresh",
                path.display()
            );
        }
        session_id
    }

    /// Remember the session this room is talking in. Written atomically.
    pub fn store_session_id(&self, room_id: &str, session_id: &str) {
        let path = self.session_path(room_id);
        if let Some(parent) = path.parent() {
            let _ignored = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let payload = serde_json::json!({ "session_id": session_id }).to_string();
        if let Err(exc) = std::fs::write(&tmp, payload).and_then(|()| std::fs::rename(&tmp, &path))
        {
            warn!(
                "{room_id}: cannot write the session file {}: {exc}",
                path.display()
            );
        }
    }

    // -- the command -----------------------------------------------------

    /// The full `claude` argv for one turn. The prompt is NOT in it.
    ///
    /// The conversation goes in on stdin, so argv carries no positional at all
    /// and `ps` shows nobody the room. `--allowedTools` (variadic) still comes
    /// last, after `extra_args`, so nothing an operator adds can be swallowed.
    #[must_use]
    pub fn build_argv(&self, prompt_file: &Path, session_id: &str, resume: bool) -> Vec<String> {
        let mut argv: Vec<String> = vec!["-p".to_owned()];
        if self.cfg.debug_log.is_some() {
            argv.extend(["--output-format", "stream-json", "--verbose"].map(ToOwned::to_owned));
        } else {
            argv.extend(["--output-format", "json"].map(ToOwned::to_owned));
        }
        argv.push("--model".to_owned());
        argv.push(self.cfg.model.clone());
        argv.push(if resume { "--resume" } else { "--session-id" }.to_owned());
        argv.push(session_id.to_owned());
        if !self.cfg.setting_sources.is_empty() {
            argv.push("--setting-sources".to_owned());
            argv.push(self.cfg.setting_sources.clone());
        }
        argv.push("--permission-mode".to_owned());
        argv.push(self.cfg.permission_mode.clone());
        argv.push("--max-turns".to_owned());
        argv.push(self.cfg.max_turns.to_string());
        argv.push("--append-system-prompt-file".to_owned());
        argv.push(prompt_file.display().to_string());
        argv.extend(self.cfg.extra_args.iter().cloned());
        if !self.cfg.allowed_tools.is_empty() {
            argv.push("--allowedTools".to_owned());
            argv.extend(self.cfg.allowed_tools.iter().cloned());
        }
        argv
    }

    /// The argv for one "should I speak?" call.
    ///
    /// Four deliberate differences from a turn, each of them a cost or a safety
    /// property:
    ///
    /// - `judge_model` (haiku by default): a yes/no question does not need the
    ///   model that writes the answers.
    /// - `--tools ""` and `--max-turns 1`: the judge cannot read, search or do
    ///   anything at all. It looks at the room and answers.
    /// - `--setting-sources ""`: it judges as the persona, not as the owner's
    ///   whole setup. Measured against the real CLI on 2026-09-02, this halves
    ///   the call: 6.5k vs 12.2k input tokens, $0.014 vs $0.025 on haiku.
    /// - `--no-session-persistence` and no `--resume`/`--session-id`: a
    ///   throwaway session that never reaches disk, so the room's own session
    ///   cannot be polluted by questions the room never sees.
    ///
    /// `extra_args` is deliberately not passed on: those tune the agent that
    /// talks (`--add-dir` and friends), not the doorman.
    #[must_use]
    pub fn build_judge_argv(&self, prompt_file: &Path) -> Vec<String> {
        let model = if self.cfg.judge_model.is_empty() {
            &self.cfg.model
        } else {
            &self.cfg.judge_model
        };
        vec![
            "-p".to_owned(),
            "--output-format".to_owned(),
            "json".to_owned(),
            "--model".to_owned(),
            model.clone(),
            "--max-turns".to_owned(),
            "1".to_owned(),
            "--no-session-persistence".to_owned(),
            "--setting-sources".to_owned(),
            String::new(),
            "--permission-mode".to_owned(),
            self.cfg.permission_mode.clone(),
            "--append-system-prompt-file".to_owned(),
            prompt_file.display().to_string(),
            "--tools".to_owned(),
            String::new(),
        ]
    }

    // -- one turn --------------------------------------------------------

    /// Seconds left of a usage-limit cooldown, during which nothing spawns.
    #[must_use]
    pub fn cooldown_remaining(&self) -> f64 {
        let until = self.cooldown_until.load(Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)]
        let until = until as f64;
        (until - self.now()).max(0.0)
    }

    fn start_cooldown(&self, room_id: &str, outcome: &Outcome) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let until = (self.now() + self.cfg.rate_limit_backoff_s).max(0.0) as u64;
        self.cooldown_until.store(until, Ordering::Relaxed);
        let hint = reset_hint(&outcome.limit_message)
            .map(|hint| format!(" (resets {hint})"))
            .unwrap_or_default();
        warn!(
            "{room_id}: claude hit a usage limit{hint}; quiet for {:.0} s. {}",
            self.cfg.rate_limit_backoff_s,
            head(&outcome.limit_message, 300)
        );
    }

    /// Spawn one `claude -p`, with the persona in a temp file.
    async fn run(
        &self,
        room_id: &str,
        prompt: &str,
        system_prompt: &str,
        session_id: &str,
        resume: bool,
    ) -> Outcome {
        let Ok(persona) = PersonaFile::write(system_prompt) else {
            error!("{room_id}: cannot write the persona file; staying quiet");
            return Outcome::default();
        };
        let argv = self.build_argv(&persona.path, session_id, resume);
        self.spawn(
            room_id,
            &argv,
            prompt,
            Some(session_id),
            self.cfg.timeout_s,
            "turn",
        )
        .await
    }

    /// One `claude` process, prompt on stdin.
    ///
    /// `session_id` is None for a throwaway run (the judge): nothing about it is
    /// ever written to the room's session file.
    async fn spawn(
        &self,
        room_id: &str,
        argv: &[String],
        prompt: &str,
        session_id: Option<&str>,
        timeout_s: f64,
        label: &str,
    ) -> Outcome {
        let started = self.now();
        self.debug(&format!(
            "--- {room_id} {label} session={} argv={}",
            session_id.unwrap_or("none"),
            serde_json::json!(argv)
        ));
        let mut command = tokio::process::Command::new(&self.cfg.claude_bin);
        command
            .args(argv)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut process = match command.spawn() {
            Ok(process) => process,
            Err(exc) => {
                error!("{room_id}: cannot run {}: {exc}", self.cfg.claude_bin);
                return Outcome::default();
            }
        };
        if let Some(mut stdin) = process.stdin.take() {
            let _ignored = stdin.write_all(prompt.as_bytes()).await;
            let _ignored = stdin.shutdown().await;
        }
        let timeout = Duration::from_secs_f64(timeout_s.max(0.0));
        let output = match tokio::time::timeout(timeout, process.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(exc)) => {
                error!("{room_id}: claude ({label}) could not be run: {exc}");
                return Outcome::default();
            }
            Err(_elapsed) => {
                // `kill_on_drop` reaps the process when the handle is dropped.
                error!(
                    "{room_id}: claude ({label}) did not finish within {timeout_s:.0} s; killed it"
                );
                // The session directory exists now, so remember the id rather
                // than orphaning it and starting a new session on every timeout.
                if let Some(session_id) = session_id {
                    self.store_session_id(room_id, session_id);
                }
                return Outcome::default();
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        self.debug(&stdout);
        if !stderr.trim().is_empty() {
            self.debug(&format!("--- stderr\n{stderr}"));
        }
        let elapsed = self.now() - started;
        let data = parse_result(&stdout);

        if !output.status.success() {
            let message = failure_message(&stderr, data.as_ref());
            if looks_unknown_session(&message) {
                return Outcome {
                    unknown_session: true,
                    ..Outcome::default()
                };
            }
            if looks_rate_limited(&message) {
                return Outcome {
                    rate_limited: true,
                    limit_message: message,
                    ..Outcome::default()
                };
            }
            error!(
                "{room_id}: claude exited {} after {elapsed:.1} s: {}",
                output.status.code().unwrap_or(-1),
                if message.is_empty() {
                    "no output".to_owned()
                } else {
                    head(&message, 500)
                }
            );
            return Outcome::default();
        }

        let Some(data) = data else {
            error!(
                "{room_id}: claude produced no result object: {}",
                head(stdout.trim(), 500)
            );
            if let Some(session_id) = session_id {
                self.store_session_id(room_id, session_id);
            }
            return Outcome::default();
        };
        if let Some(session_id) = session_id {
            self.store_session_id(room_id, &effective_session(room_id, &data, session_id));
        }
        Self::interpret(room_id, &data, elapsed, label)
    }

    // -- the result object -----------------------------------------------

    fn interpret(room_id: &str, data: &Value, elapsed: f64, label: &str) -> Outcome {
        let text = data
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if data.get("is_error").and_then(Value::as_bool) == Some(true) {
            let message = failure_message("", Some(data));
            if looks_rate_limited(&message) {
                return Outcome {
                    rate_limited: true,
                    limit_message: message,
                    ..Outcome::default()
                };
            }
            if data.get("subtype").and_then(Value::as_str) == Some(MAX_TURNS_SUBTYPE) {
                // Not a fault: the agent spent its tool budget (auto-memory
                // reads cost a turn on a fresh session) and never answered.
                warn!(
                    "{room_id}: claude ran out of turns before answering; raise \
                     brain.claude_code.max_turns. {}",
                    head(&message, 300)
                );
                return Outcome::default();
            }
            error!(
                "{room_id}: claude {label} reported an error: {}",
                if message.is_empty() {
                    "no detail".to_owned()
                } else {
                    head(&message, 500)
                }
            );
            return Outcome::default();
        }

        // Read out of the object BEFORE the macro: `tracing`'s expansion brings
        // its own `Value` trait into scope, so `Value::as_f64` there is a trait
        // path and does not compile.
        let turns = data
            .get("num_turns")
            .map_or_else(|| "?".to_owned(), ToString::to_string);
        let cost = data
            .get("total_cost_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let session = data
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_owned();
        info!(
            "{room_id}: claude {label} answered in {elapsed:.1} s              ({turns} turns, ${cost:.4}, session {session})"
        );
        if let Some(denials) = data
            .get("permission_denials")
            .and_then(Value::as_array)
            .filter(|denials| !denials.is_empty())
        {
            // Not an error: this is the read-only allowlist doing its job.
            let count = denials.len();
            let listed = serde_json::json!(denials).to_string();
            info!("{room_id}: claude was denied {count} tool call(s): {listed}");
        }
        if text.is_empty() {
            warn!("{room_id}: claude returned an empty result; staying quiet");
            return Outcome::default();
        }
        Outcome {
            text: Some(text),
            ..Outcome::default()
        }
    }

    fn debug(&self, text: &str) {
        let Some(path) = &self.cfg.debug_log else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ignored = std::fs::create_dir_all(parent);
        }
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| {
                use std::io::Write as _;
                if text.ends_with('\n') {
                    file.write_all(text.as_bytes())
                } else {
                    file.write_all(text.as_bytes())?;
                    file.write_all(b"\n")
                }
            });
        if let Err(exc) = written {
            // A debug aid must never break a turn.
            debug!("cannot write claude debug log {}: {exc}", path.display());
        }
    }
}

/// The session the run actually continued in.
///
/// Normally the one we asked for. It differs if someone puts `--fork-session`
/// in `extra_args`, and then the fork is the live session: following it is what
/// keeps the room's memory intact.
fn effective_session(room_id: &str, data: &Value, asked_for: &str) -> String {
    let reported = data.get("session_id").and_then(Value::as_str);
    match reported {
        Some(reported) if UUID_RE.is_match(reported) && reported != asked_for => {
            warn!(
                "{room_id}: claude continued in session {reported}, not the {asked_for} we asked \
                 for; following it"
            );
            reported.to_owned()
        }
        _ => asked_for.to_owned(),
    }
}

/// The `type: result` object, from either output format.
#[must_use]
pub fn parse_result(stdout: &str) -> Option<Value> {
    let mut found: Option<Value> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(candidate) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_result = candidate.is_object()
            && match candidate.get("type") {
                None | Some(Value::Null) => true,
                Some(kind) => kind.as_str() == Some("result"),
            };
        if is_result {
            found = Some(candidate);
        }
    }
    if found.is_some() {
        return found;
    }
    // `--output-format json` may pretty-print across several lines.
    serde_json::from_str::<Value>(stdout)
        .ok()
        .filter(Value::is_object)
}

#[async_trait]
impl Brain for ClaudeCodeBrain {
    async fn reply(&self, ctx: &BrainContext) -> Option<String> {
        let remaining = self.cooldown_remaining();
        if remaining > 0.0 {
            warn!(
                "{}: claude is rate-limited for another {remaining:.0} s; staying quiet without \
                 spawning",
                ctx.room_id
            );
            return None;
        }

        let prompt = render_conversation(ctx, None);
        let system_prompt = build_system_prompt(ctx);
        let stored = self.load_session_id(&ctx.room_id);
        let session_id = stored
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mut outcome = self
            .run(
                &ctx.room_id,
                &prompt,
                &system_prompt,
                &session_id,
                stored.is_some(),
            )
            .await;
        if outcome.unknown_session && stored.is_some() {
            let fresh = uuid::Uuid::new_v4().to_string();
            warn!(
                "{}: claude could not resume session {}; starting a fresh session {fresh}",
                ctx.room_id,
                stored.unwrap_or_default()
            );
            outcome = self
                .run(&ctx.room_id, &prompt, &system_prompt, &fresh, false)
                .await;
        }

        if outcome.rate_limited {
            self.start_cooldown(&ctx.room_id, &outcome);
            return None;
        }
        let text = strip_name_prefix(outcome.text.as_deref()?, &ctx.me);
        if text.is_empty() {
            warn!(
                "{}: claude replied with nothing but its own name prefix",
                ctx.room_id
            );
            return None;
        }
        Some(text)
    }

    /// The cheap "should I speak?" call. A failed judge is always a no.
    async fn judge(&self, ctx: &BrainContext) -> Judgement {
        let remaining = self.cooldown_remaining();
        if remaining > 0.0 {
            return Judgement::no(format!(
                "claude is rate-limited for another {remaining:.0} s"
            ));
        }
        let Ok(persona) = PersonaFile::write(&judge_system_prompt(ctx)) else {
            return Judgement::no("the judge's persona file could not be written");
        };
        let argv = self.build_judge_argv(&persona.path);
        let outcome = self
            .spawn(
                &ctx.room_id,
                &argv,
                &judge_prompt(ctx),
                None,
                self.cfg.judge_timeout_s,
                "judge",
            )
            .await;
        drop(persona);
        if outcome.rate_limited {
            self.start_cooldown(&ctx.room_id, &outcome);
            return Judgement::no("claude hit a usage limit while judging");
        }
        let judgement = parse_judgement(outcome.text.as_deref(), ctx.speak_threshold);
        info!(
            "{}: judge ({}) says {}: {}",
            ctx.room_id,
            if self.cfg.judge_model.is_empty() {
                &self.cfg.model
            } else {
                &self.cfg.judge_model
            },
            judgement.says(ctx.speak_threshold),
            judgement.why
        );
        judgement
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use crate::brain::Occasion;
    use crate::events::RoomEvent;

    const ME: &str = "@bot-a:example.com";
    const HUMAN: &str = "@human:example.com";
    const ROOM_ID: &str = "!room:example.com";

    /// Stand-in for `claude`: records how it was called, prints what it is told
    /// to. The same script the Python's tests used, so both suites held
    /// the CLI contract to the same shape.
    const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time

here = pathlib.Path(__file__).resolve().parent
control = json.loads((here / "control.json").read_text(encoding="utf-8"))
args = sys.argv[1:]

record = {"argv": args, "cwd": os.getcwd(), "stdin": sys.stdin.read()}
if "--append-system-prompt-file" in args:
    prompt_file = args[args.index("--append-system-prompt-file") + 1]
    record["system_prompt"] = pathlib.Path(prompt_file).read_text(encoding="utf-8")
with (here / "calls.jsonl").open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(record) + "\n")

if control.get("sleep"):
    time.sleep(control["sleep"])
if control.get("fail_on_resume") and "--resume" in args:
    sys.stderr.write(control["fail_on_resume"])
    sys.exit(1)
if control.get("exit_code"):
    sys.stderr.write(control.get("stderr", ""))
    sys.exit(control["exit_code"])

session_id = ""
for flag in ("--session-id", "--resume"):
    if flag in args:
        session_id = args[args.index(flag) + 1]
if control.get("stdout_raw") is not None:
    sys.stdout.write(control["stdout_raw"])
    sys.stderr.write(control.get("stderr", ""))
    sys.exit(control.get("raw_exit_code", 0))
print(json.dumps({
    "type": "result",
    "subtype": "success",
    "is_error": control.get("is_error", False),
    "result": control.get("result", "Hello from the fake."),
    "session_id": control.get("session_id", session_id),
    "num_turns": 2,
    "total_cost_usd": 0.0123,
    "usage": {"input_tokens": 11, "output_tokens": 7},
    "permission_denials": control.get("permission_denials", []),
}))
"#;

    /// Claude Code emits this on EVERY stream-json run, even when nothing is
    /// limited. Feeding the raw stream to the limit classifier turned every
    /// failed run into a five-minute cooldown (found by the 2026-09-02 smoke
    /// against the real CLI).
    const RATE_LIMIT_EVENT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1788375000,"rateLimitType":"five_hour","overageStatus":"rejected"}}"#;
    const MAX_TURNS_RESULT: &str = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"errors":["Reached maximum number of turns (2)"],"num_turns":3,"total_cost_usd":0.031,"permission_denials":[]}"#;

    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<f64>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(1_000.0)))
        }
        fn advance(&self, seconds: f64) {
            *self.0.lock().expect("the clock is not poisoned") += seconds;
        }
        fn as_clock(&self) -> Clock {
            let inner = Arc::clone(&self.0);
            Arc::new(move || *inner.lock().expect("the clock is not poisoned"))
        }
    }

    /// The fake executable plus the control file that steers it.
    struct FakeClaude {
        home: PathBuf,
    }

    impl FakeClaude {
        fn new(home: &Path) -> Self {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(home).expect("the fake's directory");
            let path = home.join("claude");
            std::fs::write(&path, FAKE_CLAUDE).expect("the fake claude");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("the fake is executable");
            let fake = Self {
                home: home.to_path_buf(),
            };
            fake.behave(&serde_json::json!({}));
            fake
        }

        fn path(&self) -> PathBuf {
            self.home.join("claude")
        }

        /// Replace what the next invocations do.
        fn behave(&self, control: &Value) {
            std::fs::write(self.home.join("control.json"), control.to_string())
                .expect("the control file");
        }

        fn calls(&self) -> Vec<Value> {
            let path = self.home.join("calls.jsonl");
            let Ok(text) = std::fs::read_to_string(path) else {
                return Vec::new();
            };
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("a recorded call"))
                .collect()
        }

        /// The argv of one recorded invocation.
        ///
        /// The bounds check is not decoration. When the fake never gets as far
        /// as writing its line - the brain killed it on `timeout_s`, or the
        /// machine was too loaded to spawn another process at all - the raw
        /// index panics with "index out of bounds: the len is 0", and a person
        /// reading that has no idea the spawn was the problem rather than the
        /// argv. This says which.
        fn argv(&self, index: usize) -> Vec<String> {
            let calls = self.calls();
            let call = calls.get(index).unwrap_or_else(|| {
                panic!(
                    "the fake claude recorded {} call(s), so there is no argv {index}: it never \
                     got as far as writing one. The brain either killed it on timeout_s or could \
                     not spawn it - the connector's own log line for this turn says which.",
                    calls.len()
                )
            });
            call["argv"]
                .as_array()
                .expect("argv is a list")
                .iter()
                .map(|token| token.as_str().unwrap_or_default().to_owned())
                .collect()
        }
    }

    fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        let index = argv.iter().position(|token| token == flag)?;
        argv.get(index + 1).map(String::as_str)
    }

    /// Everything a variadic flag consumes: up to the next `--option`.
    fn variadic_values<'a>(argv: &'a [String], flag: &str) -> Vec<&'a str> {
        let Some(index) = argv.iter().position(|token| token == flag) else {
            return Vec::new();
        };
        argv[index + 1..]
            .iter()
            .take_while(|token| !token.starts_with("--"))
            .map(String::as_str)
            .collect()
    }

    fn config(fake: &FakeClaude, work: &Path) -> ClaudeCodeBrainConfig {
        serde_json::from_value(serde_json::json!({
            "claude_bin": fake.path().display().to_string(),
            "model": "haiku",
            "cwd": work.display().to_string(),
            "setting_sources": "user,project",
            "allowed_tools": ["Read", "Grep"],
            "max_turns": 3,
            // Generous on purpose. The fake `claude` is a Python script, and
            // starting an interpreter on a loaded machine can take a
            // surprising while; at 30 s the brain occasionally killed it before
            // it had recorded anything, and a contract test then failed for a
            // reason that had nothing to do with the contract. The tests that
            // are ABOUT the timeout set their own (`cfg.timeout_s = 1.0`).
            "timeout_s": 300,
        }))
        .expect("a valid claude config")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        fake: FakeClaude,
        state_dir: PathBuf,
        work: PathBuf,
        clock: FakeClock,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let fake = FakeClaude::new(&dir.path().join("fake"));
            let work = dir.path().join("work");
            std::fs::create_dir_all(&work).expect("the work dir");
            Self {
                state_dir: dir.path().join("state"),
                work,
                fake,
                clock: FakeClock::new(),
                _dir: dir,
            }
        }

        fn brain(&self) -> ClaudeCodeBrain {
            self.brain_with(|_cfg| {})
        }

        fn brain_with(&self, tune: impl FnOnce(&mut ClaudeCodeBrainConfig)) -> ClaudeCodeBrain {
            let mut cfg = config(&self.fake, &self.work);
            tune(&mut cfg);
            ClaudeCodeBrain::new(cfg, self.state_dir.clone(), self.clock.as_clock())
                .expect("the brain builds")
        }
    }

    fn event(event_id: &str, body: &str) -> RoomEvent {
        RoomEvent {
            event_id: event_id.to_owned(),
            room_id: ROOM_ID.to_owned(),
            sender: HUMAN.to_owned(),
            sender_display: Some("Alex".to_owned()),
            body: body.to_owned(),
            formatted_body: None,
            msgtype: "m.text".to_owned(),
            ts: 1.0,
            thread_root: None,
            reply_to: None,
            reply_is_fallback: false,
            mentions: BTreeSet::new(),
            is_bot: false,
        }
    }

    fn context(body: &str) -> BrainContext {
        context_in(ROOM_ID, body, "You are Alex's agent.")
    }

    fn context_in(room_id: &str, body: &str, persona: &str) -> BrainContext {
        let earlier = event("$earlier", "earlier line");
        let trigger = event("$trigger", body);
        BrainContext {
            persona: persona.to_owned(),
            me: ME.to_owned(),
            room_id: room_id.to_owned(),
            trigger: trigger.clone(),
            history: vec![earlier, trigger],
            thread: Vec::new(),
            occasion: Occasion::Reply,
            note: String::new(),
            want_urgency: false,
            speak_threshold: 5,
            participants: 0,
        }
    }

    // -- one session per room --------------------------------------------

    #[tokio::test]
    async fn the_first_turn_opens_a_session_and_the_next_one_resumes_it() {
        // One session per room is the whole point: without the resume the agent
        // meets the room fresh on every message.
        let fixture = Fixture::new();
        let brain = fixture.brain();
        assert_eq!(
            brain.reply(&context("how are you?")).await.as_deref(),
            Some("Hello from the fake.")
        );

        let first = fixture.fake.argv(0);
        let opened = flag_value(&first, "--session-id").expect("the first turn opens a session");
        assert!(!first.iter().any(|token| token == "--resume"));
        let state: Value = serde_json::from_str(
            &std::fs::read_to_string(brain.session_path(ROOM_ID)).expect("the session file"),
        )
        .expect("valid json");
        assert_eq!(state, serde_json::json!({ "session_id": opened }));

        assert!(brain.reply(&context("and now?")).await.is_some());
        let second = fixture.fake.argv(1);
        assert_eq!(flag_value(&second, "--resume"), Some(opened));
        assert!(!second.iter().any(|token| token == "--session-id"));
    }

    #[tokio::test]
    async fn each_room_gets_its_own_session() {
        let fixture = Fixture::new();
        let brain = fixture.brain();
        let other = "!other:example.com";
        brain.reply(&context("hi")).await;
        brain
            .reply(&context_in(other, "hi", "You are Alex's agent."))
            .await;
        let first = fixture.fake.argv(0);
        let second = fixture.fake.argv(1);
        let first = flag_value(&first, "--session-id").expect("a session");
        let second = flag_value(&second, "--session-id").expect("a session");
        assert_ne!(first, second);
        assert_ne!(brain.session_path(ROOM_ID), brain.session_path(other));
    }

    // -- the command shape -----------------------------------------------

    #[tokio::test]
    async fn the_allowlist_reaches_the_cli_exactly_as_configured() {
        // `--allowedTools` IS the read-only guarantee: `-p` cannot ask a human,
        // so a tool outside this list is denied. Anything added silently would
        // widen it.
        let fixture = Fixture::new();
        let brain = fixture.brain_with(|cfg| {
            cfg.allowed_tools = ["Read", "Glob", "WebSearch"]
                .iter()
                .map(|t| (*t).to_owned())
                .collect();
        });
        brain.reply(&context("hi")).await;

        let argv = fixture.fake.argv(0);
        assert_eq!(
            variadic_values(&argv, "--allowedTools"),
            ["Read", "Glob", "WebSearch"]
        );
        assert!(
            !argv
                .iter()
                .any(|token| token.to_lowercase().contains("dangerous")),
            "{argv:?}"
        );
        assert_eq!(flag_value(&argv, "--permission-mode"), Some("default"));
    }

    #[tokio::test]
    async fn model_settings_turns_and_cwd_are_honoured() {
        let fixture = Fixture::new();
        let brain = fixture.brain_with(|cfg| {
            cfg.model = "sonnet".to_owned();
            cfg.setting_sources = "user".to_owned();
        });
        brain.reply(&context("hi")).await;

        let argv = fixture.fake.argv(0);
        assert_eq!(flag_value(&argv, "--model"), Some("sonnet"));
        assert_eq!(flag_value(&argv, "--setting-sources"), Some("user"));
        assert_eq!(flag_value(&argv, "--max-turns"), Some("3"));
        assert_eq!(flag_value(&argv, "--output-format"), Some("json"));
        let cwd = fixture.fake.calls()[0]["cwd"].as_str().unwrap().to_owned();
        assert_eq!(
            std::fs::canonicalize(cwd).unwrap(),
            std::fs::canonicalize(&fixture.work).unwrap()
        );
    }

    #[tokio::test]
    async fn the_prompt_goes_in_on_stdin_and_never_into_argv() {
        // The room's conversation must not be visible in `ps`, and there must be
        // no positional argument at all: `--allowedTools`, `--add-dir` and
        // `--tools` are variadic, and a trailing token is either swallowed by
        // one of them or taken as the prompt in place of stdin.
        let fixture = Fixture::new();
        let brain = fixture.brain();
        brain.reply(&context("are you there?")).await;

        let call = fixture.fake.calls()[0].clone();
        let stdin = call["stdin"].as_str().expect("stdin was recorded");
        assert_eq!(
            stdin.lines().collect::<Vec<_>>(),
            ["Alex: earlier line", "Alex: are you there?"]
        );
        let argv = fixture.fake.argv(0);
        assert_eq!(argv[0], "-p");
        assert!(
            !argv.iter().any(|token| token.contains("are you there?")),
            "{argv:?}"
        );
        assert!(argv[1].starts_with("--"), "{argv:?}");
    }

    #[tokio::test]
    async fn the_persona_and_the_frame_reach_the_system_prompt_file() {
        let fixture = Fixture::new();
        let brain = fixture.brain();
        brain
            .reply(&context_in(ROOM_ID, "hi", "You are Alex's agent, bot-a."))
            .await;

        let call = fixture.fake.calls()[0].clone();
        let system_prompt = call["system_prompt"].as_str().expect("a system prompt");
        assert!(system_prompt.contains("You are Alex's agent, bot-a."));
        assert!(system_prompt.contains(&format!("Matrix group chat as {ME}")));
        assert!(system_prompt.contains("no name prefix"));
        assert!(system_prompt.ends_with(SECRECY_LINE));
        assert!(flag_value(&fixture.fake.argv(0), "--append-system-prompt-file").is_some());
    }

    #[tokio::test]
    async fn the_persona_file_is_removed_after_the_turn() {
        let fixture = Fixture::new();
        let brain = fixture.brain();
        brain.reply(&context("hi")).await;
        let argv = fixture.fake.argv(0);
        let path = flag_value(&argv, "--append-system-prompt-file").expect("a prompt file");
        assert!(
            !Path::new(path).exists(),
            "the persona temp file outlived the turn"
        );
    }

    #[tokio::test]
    async fn extra_args_are_appended_before_the_variadic_allowlist() {
        let fixture = Fixture::new();
        let brain = fixture.brain_with(|cfg| {
            cfg.extra_args = vec!["--add-dir".to_owned(), "/tmp".to_owned()];
        });
        brain.reply(&context("hi")).await;
        let argv = fixture.fake.argv(0);
        let add_dir = argv
            .iter()
            .position(|t| t == "--add-dir")
            .expect("--add-dir");
        let allowed = argv
            .iter()
            .position(|t| t == "--allowedTools")
            .expect("--allowedTools");
        assert!(add_dir < allowed);
        assert_eq!(variadic_values(&argv, "--allowedTools"), ["Read", "Grep"]);
    }

    // -- what makes it stay quiet ----------------------------------------

    #[tokio::test]
    async fn an_error_result_means_no_reply() {
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"is_error": true, "result": "something went wrong"}));
        assert!(fixture.brain().reply(&context("hi")).await.is_none());
    }

    #[tokio::test]
    async fn an_empty_result_means_no_reply() {
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"result": "   \n  "}));
        assert!(fixture.brain().reply(&context("hi")).await.is_none());
    }

    #[tokio::test]
    async fn a_crash_means_no_reply_not_a_crashed_connector() {
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"exit_code": 2, "stderr": "Error: something unexpected"}));
        assert!(fixture.brain().reply(&context("hi")).await.is_none());
    }

    #[tokio::test]
    async fn a_missing_executable_means_no_reply() {
        let fixture = Fixture::new();
        let brain = fixture.brain_with(|cfg| cfg.claude_bin = "/nonexistent/claude".to_owned());
        assert!(brain.reply(&context("hi")).await.is_none());
    }

    #[tokio::test]
    async fn a_hung_claude_is_killed_and_answers_nothing() {
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({"sleep": 30}));
        let brain = fixture.brain_with(|cfg| cfg.timeout_s = 1.0);
        assert!(brain.reply(&context("hi")).await.is_none());
    }

    #[tokio::test]
    async fn a_leading_name_prefix_is_stripped() {
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"result": "bot-a: doing fine, thanks"}));
        assert_eq!(
            fixture.brain().reply(&context("hi")).await.as_deref(),
            Some("doing fine, thanks")
        );
    }

    #[tokio::test]
    async fn a_reply_that_is_only_a_name_prefix_is_no_reply() {
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"result": "bot-a:"}));
        assert!(fixture.brain().reply(&context("hi")).await.is_none());
    }

    // -- rate limits ------------------------------------------------------

    #[tokio::test]
    async fn a_usage_limit_stops_the_brain_spawning_claude_until_the_backoff_passes() {
        // A limited account must not be hammered once per message. The proof is
        // that the fake's record file does not grow during the cooldown.
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({
            "exit_code": 1,
            "stderr": "Claude AI usage limit reached: session limit, resets 4pm",
        }));
        let brain = fixture.brain_with(|cfg| cfg.rate_limit_backoff_s = 300.0);

        assert!(brain.reply(&context("hi")).await.is_none());
        assert_eq!(fixture.fake.calls().len(), 1);

        fixture
            .fake
            .behave(&serde_json::json!({"result": "I am back"}));
        fixture.clock.advance(120.0);
        assert!(brain.reply(&context("hi")).await.is_none());
        assert_eq!(
            fixture.fake.calls().len(),
            1,
            "claude was spawned during the rate-limit cooldown"
        );

        fixture.clock.advance(200.0);
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("I am back")
        );
        assert_eq!(fixture.fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn the_cooldown_after_a_usage_limit_is_as_long_as_the_config_says() {
        // The gate above proves a limit stops the spawning; this one proves the
        // length of the silence is the operator's number and not one of ours. A
        // shorter one is somebody who would rather be hammered than quiet.
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({
            "exit_code": 1,
            "stderr": "Claude AI usage limit reached: session limit, resets 4pm",
        }));
        let brain = fixture.brain_with(|cfg| cfg.rate_limit_backoff_s = 60.0);
        assert!(brain.reply(&context("hi")).await.is_none());
        assert_eq!(fixture.fake.calls().len(), 1);

        fixture
            .fake
            .behave(&serde_json::json!({"result": "I am back"}));
        fixture.clock.advance(30.0);
        assert!(brain.reply(&context("hi")).await.is_none());
        assert_eq!(
            fixture.fake.calls().len(),
            1,
            "claude was spawned 30 s into a 60 s cooldown"
        );

        fixture.clock.advance(40.0);
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("I am back"),
            "70 s in, a 60 s cooldown is over"
        );
        assert_eq!(fixture.fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn a_limit_reported_in_the_result_object_also_triggers_the_cooldown() {
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({
            "is_error": true,
            "result": "5-hour limit reached, resets at 18:00",
        }));
        let brain = fixture.brain_with(|cfg| cfg.rate_limit_backoff_s = 300.0);
        assert!(brain.reply(&context("hi")).await.is_none());
        fixture.fake.behave(&serde_json::json!({"result": "back"}));
        assert!(brain.reply(&context("hi")).await.is_none());
        assert_eq!(fixture.fake.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_stream_dump_never_reaches_the_rate_limit_classifier() {
        // The stream carries `rate_limit_event` and `resetsAt` on every run.
        // Reading limits out of it silences the agent for five minutes over
        // nothing.
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({
            "stdout_raw": format!("{RATE_LIMIT_EVENT}\n{MAX_TURNS_RESULT}\n"),
            "raw_exit_code": 1,
        }));
        let brain = fixture.brain_with(|cfg| cfg.rate_limit_backoff_s = 300.0);
        assert!(brain.reply(&context("hi")).await.is_none());

        fixture
            .fake
            .behave(&serde_json::json!({"result": "still here"}));
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("still here"),
            "the brain went into a cooldown it never earned"
        );
        assert_eq!(fixture.fake.calls().len(), 2);
    }

    #[test]
    fn the_failure_message_is_built_from_the_fields_that_explain_the_failure() {
        let data: Value = serde_json::from_str(MAX_TURNS_RESULT).expect("valid json");
        let message = failure_message("", Some(&data));
        assert!(message.contains("Reached maximum number of turns (2)"));
        assert!(message.contains("error_max_turns"));
        assert!(!message.contains("rate_limit"));
        assert!(!looks_rate_limited(&message));
        // The raw line on its own does look limited - which is exactly why it is
        // never the thing we classify.
        assert!(looks_rate_limited(RATE_LIMIT_EVENT));
    }

    #[tokio::test]
    async fn running_out_of_turns_is_silence_not_a_cooldown() {
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({
            "stdout_raw": format!("{MAX_TURNS_RESULT}\n"),
            "raw_exit_code": 0,
        }));
        let brain = fixture.brain();
        assert!(brain.reply(&context("hi")).await.is_none());
        fixture
            .fake
            .behave(&serde_json::json!({"result": "answered this time"}));
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("answered this time")
        );
    }

    #[test]
    fn limit_detection_needs_more_than_the_word_limit() {
        assert!(looks_rate_limited(
            "Claude AI usage limit reached|1756800000"
        ));
        assert!(looks_rate_limited("session limit reached, resets 4pm"));
        assert!(looks_rate_limited("HTTP 429: rate limit exceeded"));
        assert!(!looks_rate_limited(
            "There is no limit to how much I enjoy this room."
        ));
        assert!(!looks_rate_limited("Error: something unexpected"));
    }

    // -- a session that cannot be resumed --------------------------------

    #[tokio::test]
    async fn an_unresumable_session_is_replaced_and_the_turn_still_happens() {
        // Claude Code's session store is not ours to guarantee. When the id we
        // kept is gone, the agent loses the room's memory - it must not lose the
        // turn too.
        let fixture = Fixture::new();
        let brain = fixture.brain();
        let stale = "11111111-2222-3333-4444-555555555555";
        brain.store_session_id(ROOM_ID, stale);

        // Exactly what the real CLI does: refuse the resume, accept a new one.
        fixture.fake.behave(&serde_json::json!({
            "fail_on_resume": format!("No conversation found with session ID: {stale}"),
            "result": "fresh start",
        }));
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("fresh start")
        );

        assert_eq!(flag_value(&fixture.fake.argv(0), "--resume"), Some(stale));
        let second = fixture.fake.argv(1);
        let fresh = flag_value(&second, "--session-id").expect("a fresh session");
        assert_ne!(fresh, stale);
        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(brain.session_path(ROOM_ID)).expect("the session file"),
        )
        .expect("valid json");
        assert_eq!(written, serde_json::json!({ "session_id": fresh }));
    }

    #[tokio::test]
    async fn a_corrupt_session_file_is_treated_as_no_session() {
        let fixture = Fixture::new();
        let brain = fixture.brain();
        let path = brain.session_path(ROOM_ID);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the rooms dir");
        std::fs::write(&path, "{not json").expect("a corrupt file");

        assert!(brain.reply(&context("hi")).await.is_some());
        assert!(flag_value(&fixture.fake.argv(0), "--session-id").is_some());
    }

    #[test]
    fn unknown_session_detection_matches_what_claude_says() {
        assert!(looks_unknown_session(
            "No conversation found with session ID: abc"
        ));
        assert!(looks_unknown_session(
            "Error: Session ID abc is already in use."
        ));
        assert!(!looks_unknown_session("Error: something unexpected"));
    }

    // -- debug stream ------------------------------------------------------

    #[tokio::test]
    async fn debug_mode_streams_json_and_records_it() {
        let fixture = Fixture::new();
        let debug_log = fixture.work.join("claude-debug.jsonl");
        let brain = fixture.brain_with(|cfg| cfg.debug_log = Some(debug_log.clone()));
        assert!(brain.reply(&context("hi")).await.is_some());

        let argv = fixture.fake.argv(0);
        assert_eq!(flag_value(&argv, "--output-format"), Some("stream-json"));
        assert!(
            argv.iter().any(|token| token == "--verbose"),
            "stream-json without --verbose is refused by claude"
        );
        let written = std::fs::read_to_string(&debug_log).expect("the debug log");
        assert!(written.contains("--append-system-prompt-file"));
        assert!(written.contains("\"result\""));
    }

    // -- pure helpers -----------------------------------------------------

    #[test]
    fn the_frame_is_appended_to_the_persona() {
        let prompt = build_system_prompt(&context_in(ROOM_ID, "hi", "  You are Alex's agent.  "));
        assert!(prompt.starts_with("You are Alex's agent."));
        assert!(prompt.ends_with(SECRECY_LINE));
    }

    #[test]
    fn the_frame_stands_alone_without_a_persona() {
        let prompt = build_system_prompt(&context_in(ROOM_ID, "hi", ""));
        assert!(prompt.starts_with(&format!(
            "You are taking part in a Matrix group chat as {ME}"
        )));
    }

    #[test]
    fn only_our_own_name_prefix_is_stripped() {
        assert_eq!(strip_name_prefix("bot-a: hello", ME), "hello");
        assert_eq!(strip_name_prefix(&format!("{ME}: hello"), ME), "hello");
        assert_eq!(strip_name_prefix("BOT-A: hello", ME), "hello");
        // Not ours, and not a name at all: leaving these alone is the point.
        assert_eq!(strip_name_prefix("Note: hello", ME), "Note: hello");
        assert_eq!(strip_name_prefix("Alex: hello", ME), "Alex: hello");
    }

    #[test]
    fn the_secrecy_rule_is_part_of_every_system_prompt() {
        // No persona and no config can switch it off: it is not a setting.
        assert!(build_system_prompt(&context_in(ROOM_ID, "hi", "")).contains(SECRECY_LINE));
        assert!(
            build_system_prompt(&context_in(ROOM_ID, "hi", "Say anything you like."))
                .contains(SECRECY_LINE)
        );
    }

    // -- the tier-2 judge --------------------------------------------------

    #[tokio::test]
    async fn the_judge_is_a_cheap_toolless_throwaway_run() {
        // Every difference from a normal turn is deliberate, and each one is a
        // cost or a safety property: the cheap model, one turn, no tools, no
        // settings, and a session that is never persisted and never resumed.
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"result": "score: 7 - nobody has mentioned the deadline"}));
        let brain = fixture.brain_with(|cfg| {
            cfg.model = "sonnet".to_owned();
            cfg.judge_model = "haiku".to_owned();
        });

        let judgement = brain.judge(&context("anyone around?")).await;
        assert!(judgement.speak);
        assert_eq!(judgement.why, "nobody has mentioned the deadline");

        let argv = fixture.fake.argv(0);
        assert_eq!(
            flag_value(&argv, "--model"),
            Some("haiku"),
            "the judge must not run the reply model"
        );
        assert_eq!(flag_value(&argv, "--max-turns"), Some("1"));
        assert_eq!(
            variadic_values(&argv, "--tools"),
            [""],
            "the judge must have no tools at all"
        );
        assert!(argv.iter().any(|t| t == "--no-session-persistence"));
        assert!(!argv.iter().any(|t| t == "--resume" || t == "--session-id"));
        assert_eq!(flag_value(&argv, "--setting-sources"), Some(""));
        assert!(!argv.iter().any(|t| t == "--allowedTools"));
        // The room went in on stdin, and the question came with it.
        let stdin = fixture.fake.calls()[0]["stdin"]
            .as_str()
            .expect("stdin")
            .to_owned();
        assert!(stdin.contains("how much would you add by speaking"));
        assert!(stdin.contains("`score: N`"));
        assert!(stdin.contains("anyone around?"));
    }

    #[tokio::test]
    async fn the_judge_runs_the_model_it_was_told_to_and_the_reply_model_when_it_was_not() {
        // The judge model is the cost control: it is asked on every unaddressed
        // line, and it is the reason a room full of chatter is cheap. A build
        // that ran the reply model here would be the same bill on every line.
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"result": "no: nothing to add"}));
        let brain = fixture.brain_with(|cfg| {
            cfg.model = "sonnet".to_owned();
            cfg.judge_model = "opus".to_owned();
        });
        brain.judge(&context("anyone around?")).await;
        assert_eq!(flag_value(&fixture.fake.argv(0), "--model"), Some("opus"));

        // And empty means "the same model as the reply", which is the only way
        // to run one model for both.
        let plain = fixture.brain_with(|cfg| {
            cfg.model = "sonnet".to_owned();
            cfg.judge_model = String::new();
        });
        plain.judge(&context("anyone around?")).await;
        assert_eq!(flag_value(&fixture.fake.argv(1), "--model"), Some("sonnet"));
    }

    #[tokio::test]
    async fn the_judge_gives_up_on_its_own_timeout_and_not_the_replys() {
        // A verdict that arrives late is worthless: the judge has a timeout of
        // its own, well under the reply's, because the room is waiting to hear
        // whether anybody is going to say anything at all. The fixture's
        // `timeout_s` is 300 s and the fake sleeps for 30, so a judge that used
        // the reply's timeout would sit here for half a minute.
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({"sleep": 30}));
        let brain = fixture.brain_with(|cfg| cfg.judge_timeout_s = 1.0);
        let started = std::time::Instant::now();
        let judgement = brain.judge(&context("anyone around?")).await;
        let waited = started.elapsed();
        assert!(!judgement.speak);
        assert!(
            judgement.why.contains("answered nothing"),
            "{}",
            judgement.why
        );
        assert!(
            waited < Duration::from_secs(15),
            "the judge waited {waited:?}: that is the reply's timeout, not its own"
        );
    }

    #[tokio::test]
    async fn the_permission_mode_the_operator_set_is_the_one_claude_runs_under() {
        // The config refuses `bypassPermissions`; every other mode is the
        // operator's to choose, and a build that passed its own would make the
        // choice decorative - in both directions, since the judge runs under it
        // too.
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"result": "no: nothing to add"}));
        let brain = fixture.brain_with(|cfg| cfg.permission_mode = "plan".to_owned());
        brain.reply(&context("hi")).await;
        brain.judge(&context("hi")).await;
        assert_eq!(
            flag_value(&fixture.fake.argv(0), "--permission-mode"),
            Some("plan")
        );
        assert_eq!(
            flag_value(&fixture.fake.argv(1), "--permission-mode"),
            Some("plan")
        );
    }

    #[tokio::test]
    async fn the_judge_never_touches_the_rooms_session_file() {
        // A "should I speak?" question that landed in the room's own session
        // would show up in the next reply as something the room never said.
        let fixture = Fixture::new();
        let brain = fixture.brain();
        brain.reply(&context("hi")).await;
        let session = std::fs::read_to_string(brain.session_path(ROOM_ID)).expect("a session file");

        fixture.fake.behave(&serde_json::json!({
            "result": "no: nothing to add",
            "session_id": "ffffffff-1111-2222-3333-444444444444",
        }));
        brain.judge(&context("chatter")).await;
        assert_eq!(
            std::fs::read_to_string(brain.session_path(ROOM_ID)).expect("a session file"),
            session
        );
    }

    #[tokio::test]
    async fn the_judge_is_told_who_it_is_but_asked_only_for_a_verdict() {
        let fixture = Fixture::new();
        let brain = fixture.brain();
        brain
            .judge(&context_in(ROOM_ID, "hi", "You are Alex's agent, bot-a."))
            .await;
        let system_prompt = fixture.fake.calls()[0]["system_prompt"]
            .as_str()
            .expect("a system prompt")
            .to_owned();
        assert!(system_prompt.contains("You are Alex's agent, bot-a."));
        assert!(system_prompt.contains("deciding whether to speak at all"));
        assert!(system_prompt.contains("Do not write the message itself"));
    }

    #[tokio::test]
    async fn anything_that_is_not_a_clean_verdict_is_a_no() {
        // Strict parsing on purpose: a judge that cannot answer the one question
        // it was asked has not earned a turn in the room.
        for answer in [
            "maybe, I could add something",
            "**yes:** with markdown around it",
            "",
            "I think the room would benefit from my view on this",
        ] {
            let fixture = Fixture::new();
            fixture.fake.behave(&serde_json::json!({"result": answer}));
            assert!(
                !fixture.brain().judge(&context("hi")).await.speak,
                "{answer:?} was taken as a yes"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_judge_is_a_no_not_a_crash() {
        let fixture = Fixture::new();
        fixture
            .fake
            .behave(&serde_json::json!({"exit_code": 2, "stderr": "Error: something unexpected"}));
        assert!(!fixture.brain().judge(&context("hi")).await.speak);
    }

    #[tokio::test]
    async fn the_judge_respects_the_usage_limit_cooldown() {
        // The cooldown exists so a limited account is not hammered. A judge call
        // is still a call: it must not be the thing that keeps hammering it.
        let fixture = Fixture::new();
        fixture.fake.behave(&serde_json::json!({
            "exit_code": 1,
            "stderr": "Claude AI usage limit reached: resets 4pm",
        }));
        let brain = fixture.brain_with(|cfg| cfg.rate_limit_backoff_s = 300.0);

        assert!(!brain.judge(&context("hi")).await.speak);
        assert_eq!(fixture.fake.calls().len(), 1);

        fixture
            .fake
            .behave(&serde_json::json!({"result": "score: 9 - I am back"}));
        fixture.clock.advance(120.0);
        assert!(!brain.judge(&context("hi")).await.speak);
        assert_eq!(
            fixture.fake.calls().len(),
            1,
            "claude was spawned during the rate-limit cooldown"
        );
        assert!(
            brain.reply(&context("hi")).await.is_none(),
            "a limit found by the judge must be shared"
        );

        fixture.clock.advance(200.0);
        assert!(brain.judge(&context("hi")).await.speak);
        assert_eq!(fixture.fake.calls().len(), 2);
    }
}
