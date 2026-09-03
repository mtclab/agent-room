//! Configuration for a connector process.
//!
//! The YAML schema is the Python's, knob for knob, so one config file
//! runs either implementation. Secrets never live in the config file itself:
//! the access token comes from a 0600 file, or from a password login whose
//! token is then cached 0600 under `state_dir`.
//!
//! Every knob in the schema is acted on by this build: `run`, `mcp`, `init` and
//! `doctor` between them cover the whole file, so nothing an operator sets is
//! silently ignored.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::events::localpart;

/// Set to "1" to allow an access-token file whose mode is looser than 0600.
pub const ALLOW_LOOSE_PERMS_ENV: &str = "AGENT_ROOM_ALLOW_LOOSE_PERMS";
/// Fallback for `brain.openai_compat.api_key`.
pub const API_KEY_ENV: &str = "AGENT_ROOM_API_KEY";

static UNSAFE_PATH_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_.-]").expect("the path pattern is a literal"));
static VAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$(\{([A-Za-z_][A-Za-z0-9_]*)\}|([A-Za-z_][A-Za-z0-9_]*))")
        .expect("the variable pattern is a literal")
});
/// Any extra CLI argument matching this is refused: it is the family of
/// "skip every permission check" flags.
static DANGEROUS_ARG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)dangerous").expect("the dangerous pattern is a literal"));

/// Permission modes `claude --permission-mode` accepts (2.1.258).
pub const CLAUDE_PERMISSION_MODES: [&str; 7] = [
    "default",
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "dontAsk",
    "manual",
    "plan",
];
/// Permission modes a room agent may never run under. The room is a social
/// surface, not an operator console: bypassing permissions would make the
/// read-only tool allowlist decorative.
pub const CLAUDE_FORBIDDEN_PERMISSION_MODES: [&str; 1] = ["bypassPermissions"];

/// Raised when the configuration is unusable (bad file, bad permissions).
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
}

impl ConfigError {
    fn msg(text: impl Into<String>) -> Self {
        Self::Invalid(text.into())
    }
}

type Result<T> = std::result::Result<T, ConfigError>;

/// `~` and `$VAR` expansion, the way the Python expanded every path.
#[must_use]
pub fn expand(value: &str) -> PathBuf {
    let expanded = VAR_RE.replace_all(value, |caps: &regex::Captures<'_>| {
        let name = caps
            .get(2)
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or_default();
        std::env::var(name).unwrap_or_else(|_| caps[0].to_owned())
    });
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(expanded.as_ref());
    };
    let home = PathBuf::from(home);
    match expanded.as_ref() {
        "~" => home,
        rest if rest.starts_with("~/") => home.join(&rest[2..]),
        rest => PathBuf::from(rest),
    }
}

/// A room id or user id turned into one safe filename component.
#[must_use]
pub fn sanitize_for_path(value: &str) -> String {
    UNSAFE_PATH_CHARS.replace_all(value, "_").into_owned()
}

/// Path under `state_dir/rooms` for one room's `suffix` file.
#[must_use]
pub fn room_state_path(state_dir: &Path, room_id: &str, suffix: &str) -> PathBuf {
    state_dir
        .join("rooms")
        .join(format!("{}{suffix}", sanitize_for_path(room_id)))
}

fn de_path<'de, D>(de: D) -> std::result::Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(de)?;
    Ok(expand(&raw))
}

fn de_opt_path<'de, D>(de: D) -> std::result::Result<Option<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(de)?;
    Ok(raw.map(|value| expand(&value)))
}

/// An OpenAI-compatible chat-completions endpoint (vLLM behind llama-swap).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiCompatBrainConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// llama-swap boots vLLM on the first request; 3-8 min is normal.
    #[serde(default = "default_cold_start")]
    pub cold_start_timeout_s: f64,
    /// Merged into the request body. `chat_template_kwargs.enable_thinking:
    /// false` is mandatory for Qwen3.8, which otherwise spends the whole budget
    /// thinking.
    #[serde(default)]
    pub extra_body: BTreeMap<String, Value>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Model for the "should I speak" judge. Empty = the same model.
    #[serde(default)]
    pub judge_model: String,
    #[serde(default = "default_judge_max_tokens")]
    pub judge_max_tokens: u32,
    /// Endpoint for the judge. Empty = the same endpoint as the reply.
    #[serde(default)]
    pub judge_base_url: String,
    /// Credentials for `judge_base_url`. Empty = `api_key` when the judge
    /// shares the reply endpoint, and nothing when it does not: a token for one
    /// server has no business being sent to another.
    #[serde(default)]
    pub judge_api_key: String,
    /// Request-body extras for the judge. Empty = `extra_body`, but only when
    /// the judge runs on the same endpoint - one server's knobs are another
    /// server's 400.
    #[serde(default)]
    pub judge_extra_body: BTreeMap<String, Value>,
    /// On-demand models: fire one throwaway 1-token completion when a human
    /// starts typing, so the load happens while nobody is waiting.
    #[serde(default)]
    pub warm_on_intent: bool,
    #[serde(default = "default_warm_cooldown")]
    pub warm_cooldown_s: f64,
}

fn default_cold_start() -> f64 {
    600.0
}
fn default_max_tokens() -> u32 {
    600
}
fn default_judge_max_tokens() -> u32 {
    60
}
fn default_warm_cooldown() -> f64 {
    120.0
}

impl OpenAiCompatBrainConfig {
    /// This adapter at its shipped defaults, pointed at one endpoint.
    ///
    /// `init` writes the block from here rather than spelling the defaults out,
    /// so a friend's first config IS the shipped default and cannot drift.
    #[must_use]
    pub fn shipped(base_url: &str, model: &str) -> Self {
        Self {
            base_url: base_url.to_owned(),
            model: model.to_owned(),
            api_key: String::new(),
            cold_start_timeout_s: default_cold_start(),
            extra_body: BTreeMap::new(),
            max_tokens: default_max_tokens(),
            judge_model: String::new(),
            judge_max_tokens: default_judge_max_tokens(),
            judge_base_url: String::new(),
            judge_api_key: String::new(),
            judge_extra_body: BTreeMap::new(),
            warm_on_intent: false,
            warm_cooldown_s: default_warm_cooldown(),
        }
    }

    #[must_use]
    pub fn resolved_api_key(&self) -> String {
        if self.api_key.is_empty() {
            std::env::var(API_KEY_ENV).unwrap_or_default()
        } else {
            self.api_key.clone()
        }
    }

    /// The endpoint the judge posts to.
    #[must_use]
    pub fn resolved_judge_url(&self) -> String {
        let base = if self.judge_base_url.is_empty() {
            &self.base_url
        } else {
            &self.judge_base_url
        };
        format!("{}/chat/completions", base.trim_end_matches('/'))
    }

    /// The judge's credentials. A separate server gets only its own key.
    #[must_use]
    pub fn resolved_judge_api_key(&self) -> String {
        if !self.judge_base_url.is_empty() {
            return self.judge_api_key.clone();
        }
        if self.judge_api_key.is_empty() {
            self.resolved_api_key()
        } else {
            self.judge_api_key.clone()
        }
    }

    /// Request-body extras for the judge call.
    #[must_use]
    pub fn resolved_judge_body(&self) -> BTreeMap<String, Value> {
        if !self.judge_extra_body.is_empty() {
            return self.judge_extra_body.clone();
        }
        if self.judge_base_url.is_empty() {
            self.extra_body.clone()
        } else {
            BTreeMap::new()
        }
    }
}

/// Claude Code headless (`claude -p`) as a brain (see `brain::claude_code`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCodeBrainConfig {
    #[serde(default = "default_claude_bin")]
    pub claude_bin: String,
    #[serde(default = "default_claude_model")]
    pub model: String,
    #[serde(default = "default_judge_model")]
    pub judge_model: String,
    #[serde(default = "default_judge_timeout")]
    pub judge_timeout_s: f64,
    #[serde(default = "default_setting_sources")]
    pub setting_sources: String,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_allowed_tools")]
    pub allowed_tools: Vec<String>,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_claude_timeout")]
    pub timeout_s: f64,
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    #[serde(default = "default_rate_limit_backoff")]
    pub rate_limit_backoff_s: f64,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub debug_log: Option<PathBuf>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_claude_bin() -> String {
    "claude".to_owned()
}
fn default_claude_model() -> String {
    "sonnet".to_owned()
}
fn default_judge_model() -> String {
    "haiku".to_owned()
}
fn default_judge_timeout() -> f64 {
    90.0
}
fn default_setting_sources() -> String {
    "user,project".to_owned()
}
fn default_allowed_tools() -> Vec<String> {
    ["Read", "Grep", "Glob", "WebSearch"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}
fn default_max_turns() -> u32 {
    3
}
fn default_claude_timeout() -> f64 {
    180.0
}
fn default_permission_mode() -> String {
    "default".to_owned()
}
fn default_rate_limit_backoff() -> f64 {
    300.0
}

impl ClaudeCodeBrainConfig {
    /// This adapter at its shipped defaults, with the model and the working
    /// directory `init` was told to use.
    #[must_use]
    pub fn shipped(model: &str, cwd: PathBuf) -> Self {
        Self {
            claude_bin: default_claude_bin(),
            model: model.to_owned(),
            judge_model: default_judge_model(),
            judge_timeout_s: default_judge_timeout(),
            setting_sources: default_setting_sources(),
            cwd: Some(cwd),
            allowed_tools: default_allowed_tools(),
            max_turns: default_max_turns(),
            timeout_s: default_claude_timeout(),
            permission_mode: default_permission_mode(),
            rate_limit_backoff_s: default_rate_limit_backoff(),
            debug_log: None,
            extra_args: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if CLAUDE_FORBIDDEN_PERMISSION_MODES.contains(&self.permission_mode.as_str()) {
            return Err(ConfigError::msg(format!(
                "brain.claude_code.permission_mode {:?} bypasses permission checks; \
                 the room agent is read-only by construction",
                self.permission_mode
            )));
        }
        if !CLAUDE_PERMISSION_MODES.contains(&self.permission_mode.as_str()) {
            return Err(ConfigError::msg(format!(
                "brain.claude_code.permission_mode {:?} is not one of {}",
                self.permission_mode,
                CLAUDE_PERMISSION_MODES.join(", ")
            )));
        }
        for arg in &self.extra_args {
            if DANGEROUS_ARG.is_match(arg) {
                return Err(ConfigError::msg(format!(
                    "brain.claude_code.extra_args contains {arg:?}; flags that skip \
                     permission checks are never passed to a room agent"
                )));
            }
        }
        Ok(())
    }
}

/// The deterministic test brain. Never a real participant.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EchoBrainConfig {
    /// A user id every echo reply names, so the reply mentions that account.
    #[serde(default)]
    pub mention_back: String,
    /// Appended to every reply, which is what leaves a question hanging.
    #[serde(default)]
    pub ask_back: String,
    /// What the echo judge reports as urgency (the inner-thoughts axis).
    #[serde(default)]
    pub urgency: i32,
}

impl EchoBrainConfig {
    fn validate(&self) -> Result<()> {
        if !self.mention_back.is_empty() && !self.mention_back.starts_with('@') {
            return Err(ConfigError::msg(format!(
                "brain.echo.mention_back {:?} is not a Matrix user id",
                self.mention_back
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    OpenaiCompat,
    ClaudeCode,
    Echo,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrainConfig {
    pub kind: BrainKind,
    #[serde(default)]
    pub openai_compat: Option<OpenAiCompatBrainConfig>,
    #[serde(default)]
    pub claude_code: Option<ClaudeCodeBrainConfig>,
    #[serde(default)]
    pub echo: EchoBrainConfig,
}

impl BrainConfig {
    fn validate(&self) -> Result<()> {
        if self.kind == BrainKind::OpenaiCompat && self.openai_compat.is_none() {
            return Err(ConfigError::msg(
                "brain.kind is openai_compat but brain.openai_compat is missing",
            ));
        }
        if self.kind == BrainKind::ClaudeCode && self.claude_code.is_none() {
            return Err(ConfigError::msg(
                "brain.kind is claude_code but brain.claude_code is missing",
            ));
        }
        if let Some(claude) = &self.claude_code {
            claude.validate()?;
        }
        self.echo.validate()
    }
}

/// How a live session's `room_post` reaches the room.
///
/// A session driven by a person is still a program posting into a shared room,
/// and every connector's `is_bot` test starts with the msgtype - so `notice` is
/// the default and `text` is a deliberate setting for an account that really is
/// a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostAs {
    Text,
    #[default]
    Notice,
}

impl PostAs {
    /// The `msgtype` this setting posts with.
    #[must_use]
    pub fn msgtype(self) -> &'static str {
        match self {
            Self::Text => crate::events::TEXT_MSGTYPE,
            Self::Notice => crate::events::NOTICE_MSGTYPE,
        }
    }
}

/// `agent-room mcp`: the live session's own presence in the room.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub post_as: PostAs,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetsConfig {
    #[serde(default = "default_per_pair_per_minute")]
    pub per_pair_per_minute: u32,
    #[serde(default = "default_pair_cooldown")]
    pub pair_cooldown_s: f64,
    #[serde(default = "default_per_thread_max")]
    pub per_thread_max: u32,
    #[serde(default = "default_per_hour_max")]
    pub per_hour_max: u32,
    /// Unprompted posts per hour, out of `per_hour_max`. Answering when
    /// addressed is the job; speaking uninvited is the luxury.
    #[serde(default = "default_tier2_per_hour_max")]
    pub tier2_per_hour_max: u32,
    /// Consecutive bot-authored messages in a thread before it winds down.
    #[serde(default = "default_bot_only_turns")]
    pub bot_only_turns_before_decay: u32,
}

fn default_per_pair_per_minute() -> u32 {
    3
}
fn default_pair_cooldown() -> f64 {
    60.0
}
fn default_per_thread_max() -> u32 {
    12
}
fn default_per_hour_max() -> u32 {
    30
}
fn default_tier2_per_hour_max() -> u32 {
    10
}
fn default_bot_only_turns() -> u32 {
    6
}

impl Default for BudgetsConfig {
    fn default() -> Self {
        Self {
            per_pair_per_minute: default_per_pair_per_minute(),
            pair_cooldown_s: default_pair_cooldown(),
            per_thread_max: default_per_thread_max(),
            per_hour_max: default_per_hour_max(),
            tier2_per_hour_max: default_tier2_per_hour_max(),
            bot_only_turns_before_decay: default_bot_only_turns(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotToBot {
    None,
    Mentions,
    All,
}

// The switches really are that many independent booleans: each one is a
// separate rule an operator may turn off on its own, and folding them into a
// bitfield or an enum would make the config file worse to read.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default = "yes")]
    pub reply_to_mentions: bool,
    #[serde(default = "yes")]
    pub reply_in_own_threads: bool,
    /// Tier 2: may I answer a human message that addressed nobody?
    #[serde(default = "yes")]
    pub answer_unaddressed: bool,
    #[serde(default = "default_backoff")]
    pub backoff_s: (f64, f64),
    #[serde(default)]
    pub heartbeat_minutes: i64,
    #[serde(default = "default_presence_window")]
    pub presence_window_min: i64,
    #[serde(default = "default_unprompted_max_wait")]
    pub unprompted_max_wait_min: i64,
    #[serde(default = "default_followup_delay")]
    pub followup_delay_s: (f64, f64),
    #[serde(default)]
    pub inner_thoughts: bool,
    #[serde(default = "default_inner_threshold")]
    pub inner_thoughts_threshold: i64,
    #[serde(default = "default_impulse_ttl")]
    pub impulse_ttl_s: f64,
    #[serde(default = "default_bot_to_bot")]
    pub bot_to_bot: BotToBot,
    #[serde(default)]
    pub budgets: BudgetsConfig,
    /// Extra user ids that count as bots regardless of msgtype.
    #[serde(default)]
    pub bot_user_ids: Vec<String>,
    /// Localpart regexes that count as bots. Empty by default: `m.notice` and
    /// `bot_user_ids` are the truth; patterns are a per-deployment convenience.
    #[serde(default)]
    pub bot_localpart_patterns: Vec<String>,
}

fn yes() -> bool {
    true
}
fn default_backoff() -> (f64, f64) {
    (5.0, 40.0)
}
fn default_presence_window() -> i64 {
    30
}
fn default_unprompted_max_wait() -> i64 {
    240
}
fn default_followup_delay() -> (f64, f64) {
    (1200.0, 10800.0)
}
fn default_inner_threshold() -> i64 {
    4
}
fn default_impulse_ttl() -> f64 {
    6.0 * 3600.0
}
fn default_bot_to_bot() -> BotToBot {
    BotToBot::Mentions
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            reply_to_mentions: true,
            reply_in_own_threads: true,
            answer_unaddressed: true,
            backoff_s: default_backoff(),
            heartbeat_minutes: 0,
            presence_window_min: default_presence_window(),
            unprompted_max_wait_min: default_unprompted_max_wait(),
            followup_delay_s: default_followup_delay(),
            inner_thoughts: false,
            inner_thoughts_threshold: default_inner_threshold(),
            impulse_ttl_s: default_impulse_ttl(),
            bot_to_bot: default_bot_to_bot(),
            budgets: BudgetsConfig::default(),
            bot_user_ids: Vec::new(),
            bot_localpart_patterns: Vec::new(),
        }
    }
}

impl PolicyConfig {
    fn validate(&self) -> Result<()> {
        for (name, (low, high)) in [
            ("policy.backoff_s", self.backoff_s),
            ("policy.followup_delay_s", self.followup_delay_s),
        ] {
            if low < 0.0 || high < low {
                return Err(ConfigError::msg(format!(
                    "{name} [{low}, {high}] must be [low, high] with 0 <= low <= high"
                )));
            }
        }
        if self.heartbeat_minutes < 0 {
            return Err(ConfigError::msg(
                "policy.heartbeat_minutes cannot be negative (0 = off)",
            ));
        }
        if self.presence_window_min < 0 {
            return Err(ConfigError::msg(
                "policy.presence_window_min cannot be negative",
            ));
        }
        if self.unprompted_max_wait_min < 0 {
            return Err(ConfigError::msg(
                "policy.unprompted_max_wait_min cannot be negative",
            ));
        }
        if self.inner_thoughts_threshold < 1 {
            return Err(ConfigError::msg(
                "policy.inner_thoughts_threshold must be at least 1 \
                 (0 would make the agent speak on every message)",
            ));
        }
        if self.impulse_ttl_s <= 0.0 {
            return Err(ConfigError::msg(
                "policy.impulse_ttl_s must be positive (it is a lifetime)",
            ));
        }
        for pattern in &self.bot_localpart_patterns {
            Regex::new(pattern).map_err(|exc| {
                ConfigError::msg(format!(
                    "policy.bot_localpart_patterns {pattern:?} is not a regex: {exc}"
                ))
            })?;
        }
        Ok(())
    }

    /// The bot-localpart patterns, compiled once.
    ///
    /// # Errors
    /// When a pattern is not a valid regex - which [`load_config`] has already
    /// refused, so this is the caller's convenience rather than a second gate.
    pub fn compiled_bot_patterns(&self) -> Result<Vec<Regex>> {
        self.bot_localpart_patterns
            .iter()
            .map(|pattern| {
                Regex::new(pattern).map_err(|exc| {
                    ConfigError::msg(format!(
                        "policy.bot_localpart_patterns {pattern:?} is not a regex: {exc}"
                    ))
                })
            })
            .collect()
    }
}

/// Client-side TLS for a homeserver behind mTLS.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub client_cert: Option<PathBuf>,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub client_key: Option<PathBuf>,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub ca_file: Option<PathBuf>,
    #[serde(default = "yes")]
    pub verify: bool,
}

impl Default for TlsConfig {
    /// TLS off, and verification ON. A derived `Default` would say
    /// `verify: false`, which is the opposite of the schema's default and would
    /// be written into every config `init` produces.
    fn default() -> Self {
        Self {
            enabled: false,
            client_cert: None,
            client_key: None,
            ca_file: None,
            verify: true,
        }
    }
}

impl TlsConfig {
    fn validate(&self) -> Result<()> {
        if self.enabled && (self.client_cert.is_none() || self.client_key.is_none()) {
            return Err(ConfigError::msg(
                "tls.enabled is true but client_cert/client_key are not both set",
            ));
        }
        Ok(())
    }

    /// The HTTP client every request goes through: the homeserver's, and the
    /// brain's.
    ///
    /// With TLS off this is a plain client with the platform's roots. With it
    /// on, the client presents the configured certificate and, when a `ca_file`
    /// is given, trusts that CA as well.
    ///
    /// # Errors
    /// When a configured file is missing or unreadable, when the private key is
    /// group/other accessible (the same rule as an access token), or when
    /// rustls refuses the material.
    pub fn build_client(&self) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().use_rustls_tls();
        if !self.enabled {
            return builder
                .build()
                .map_err(|exc| ConfigError::msg(format!("cannot build an HTTP client: {exc}")));
        }
        let (Some(cert), Some(key)) = (&self.client_cert, &self.client_key) else {
            return Err(ConfigError::msg(
                "tls.enabled is true but client_cert/client_key are not both set",
            ));
        };
        for (label, path) in [("tls.client_cert", cert), ("tls.client_key", key)] {
            if !path.is_file() {
                return Err(ConfigError::msg(format!(
                    "{label} not found: {}",
                    path.display()
                )));
            }
        }
        // The private key is a secret: same 0600 rule as the access token.
        require_private_mode(key, "tls.client_key")?;
        let mut pem = read_bytes(cert, "tls.client_cert")?;
        pem.push(b'\n');
        pem.extend_from_slice(&read_bytes(key, "tls.client_key")?);
        let identity = reqwest::Identity::from_pem(&pem).map_err(|exc| {
            ConfigError::msg(format!(
                "cannot load tls.client_cert {} / tls.client_key {}: {exc}",
                cert.display(),
                key.display()
            ))
        })?;
        builder = builder.identity(identity);
        if let Some(ca_file) = &self.ca_file {
            if !ca_file.is_file() {
                return Err(ConfigError::msg(format!(
                    "tls.ca_file not found: {}",
                    ca_file.display()
                )));
            }
            let ca = reqwest::Certificate::from_pem(&read_bytes(ca_file, "tls.ca_file")?).map_err(
                |exc| {
                    ConfigError::msg(format!(
                        "cannot load tls.ca_file {}: {exc}",
                        ca_file.display()
                    ))
                },
            )?;
            builder = builder.add_root_certificate(ca);
        }
        if !self.verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        builder
            .build()
            .map_err(|exc| ConfigError::msg(format!("cannot build an HTTP client: {exc}")))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub homeserver: String,
    pub user_id: String,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub access_token_file: Option<PathBuf>,
    #[serde(default)]
    pub password: Option<String>,
    pub rooms: Vec<String>,
    #[serde(default, deserialize_with = "de_opt_path")]
    pub persona_file: Option<PathBuf>,
    #[serde(deserialize_with = "de_path")]
    pub state_dir: PathBuf,
    /// Required by `agent-room run`; a live session has no brain of its own.
    #[serde(default)]
    pub brain: Option<BrainConfig>,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    /// How many transcript events to hand the brain as history.
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
    /// How many events stay in the live `<room>.jsonl` before it rolls into
    /// `<room>.jsonl.1`. 0 turns rolling off, and the file grows for ever the
    /// way it did before 1.0.0.
    #[serde(default = "default_transcript_keep")]
    pub transcript_keep: usize,
    /// How many rolled `<room>.jsonl.N` files are kept beside the live one.
    /// 0 keeps none: what rolls off is dropped rather than archived.
    #[serde(default = "default_transcript_archives")]
    pub transcript_archives: usize,
    /// Keep running on a device whose encryption keys another store published
    /// (see `matrix::DeviceWedged`). ONLY for rooms that are not encrypted:
    /// such a device can never decrypt anything. Default false: `run` stops
    /// with exit 3 and says why. The live gates set it for their throwaway
    /// connectors, whose test accounts have long-dead devices.
    #[serde(default)]
    pub allow_wedged_device: bool,
}

/// How many transcript events the brain is handed as history, by default.
#[must_use]
pub fn default_history_limit() -> usize {
    40
}

/// How many events the live transcript keeps, by default.
///
/// The number itself belongs to the file format, not to the schema: `transcript`
/// owns it and this is the config's way of asking for it.
#[must_use]
pub fn default_transcript_keep() -> usize {
    crate::transcript::DEFAULT_KEEP
}

/// How many rolled transcripts are kept beside the live one, by default.
#[must_use]
pub fn default_transcript_archives() -> usize {
    crate::transcript::DEFAULT_ARCHIVES
}

impl Config {
    fn validate(&mut self) -> Result<()> {
        self.homeserver = self.homeserver.trim_end_matches('/').to_owned();
        if self.access_token_file.is_none() == self.password.is_none() {
            return Err(ConfigError::msg(
                "configure exactly one of access_token_file or password",
            ));
        }
        if self.rooms.is_empty() {
            return Err(ConfigError::msg("at least one room is required"));
        }
        self.policy.validate()?;
        self.tls.validate()?;
        if let Some(brain) = &self.brain {
            brain.validate()?;
        }
        // Refused for the same reason the Python refused it: it asks the
        // judge about EVERY unaddressed message, and every one of those is a
        // paid Claude call.
        if self.policy.inner_thoughts
            && self.brain.as_ref().map(|b| b.kind) == Some(BrainKind::ClaudeCode)
        {
            return Err(ConfigError::msg(
                "policy.inner_thoughts is not allowed with brain.kind claude_code: it asks the \
                 judge about every unaddressed message in the room, and every one of those is a \
                 paid Claude call. Use it with a local resident model (openai_compat), or run \
                 tier 2 on its own.",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn localpart(&self) -> &str {
        localpart(&self.user_id)
    }

    #[must_use]
    pub fn room_state_path(&self, room_id: &str, suffix: &str) -> PathBuf {
        room_state_path(&self.state_dir, room_id, suffix)
    }

    /// Where a password login's token is cached, 0600.
    #[must_use]
    pub fn cached_token_path(&self) -> PathBuf {
        self.state_dir
            .join("credentials")
            .join(format!("{}.access", sanitize_for_path(&self.user_id)))
    }

    /// The client store: one sqlite directory per account, holding the crypto
    /// identity as well, so E2EE survives a restart.
    #[must_use]
    pub fn store_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("{}.store", sanitize_for_path(&self.user_id)))
    }

    /// Where the recovery key for this account's encryption is written, 0600.
    #[must_use]
    pub fn recovery_key_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("{}.recovery-key", sanitize_for_path(&self.user_id)))
    }

    /// The persona this agent carries, or an empty string when there is none.
    ///
    /// # Errors
    /// When `persona_file` is set but cannot be read.
    pub fn read_persona(&self) -> Result<String> {
        let Some(path) = &self.persona_file else {
            return Ok(String::new());
        };
        fs::read_to_string(path)
            .map(|text| text.trim().to_owned())
            .map_err(|exc| {
                ConfigError::msg(format!(
                    "cannot read persona_file {}: {exc}",
                    path.display()
                ))
            })
    }
}

/// Parse a YAML config file into a validated [`Config`].
///
/// # Errors
/// When the file cannot be read, is not a YAML mapping, carries a key the
/// schema does not know, or fails one of the schema's own rules.
pub fn load_config(path: &Path) -> Result<Config> {
    let expanded = expand(&path.to_string_lossy());
    let raw = fs::read_to_string(&expanded).map_err(|exc| {
        ConfigError::msg(format!("cannot read config {}: {exc}", expanded.display()))
    })?;
    let mut cfg: Config = serde_saphyr::from_str(&raw)
        .map_err(|exc| ConfigError::msg(format!("invalid config {}: {exc}", expanded.display())))?;
    cfg.validate()?;
    Ok(cfg)
}

/// Refuse a secret file that is group/other accessible.
///
/// Set `AGENT_ROOM_ALLOW_LOOSE_PERMS=1` to override (tests, throwaway setups).
///
/// # Errors
/// When the file cannot be stat'ed, or its mode is looser than 0600 and the
/// override is not set.
pub fn require_private_mode(path: &Path, label: &str) -> Result<()> {
    let meta = fs::metadata(path).map_err(|exc| {
        ConfigError::msg(format!("cannot stat {label} {}: {exc}", path.display()))
    })?;
    let mode = meta.permissions().mode() & 0o7777;
    permission_verdict(mode, loose_perms_allowed())
        .map_err(|reason| ConfigError::msg(format!("{label} {} {reason}", path.display())))
}

/// Whether the operator has asked for the 0600 rule to be waived.
#[must_use]
pub fn loose_perms_allowed() -> bool {
    std::env::var(ALLOW_LOOSE_PERMS_ENV).as_deref() == Ok("1")
}

/// The rule itself, without a filesystem: a secret must not be readable by
/// group or other, unless the override says so.
fn permission_verdict(mode: u32, allow_loose: bool) -> std::result::Result<(), String> {
    if mode.trailing_zeros() >= 6 || allow_loose {
        return Ok(());
    }
    Err(format!(
        "has mode {mode:04o}; chmod 600 it (or set {ALLOW_LOOSE_PERMS_ENV}=1 to override)"
    ))
}

fn read_bytes(path: &Path, label: &str) -> Result<Vec<u8>> {
    fs::read(path)
        .map_err(|exc| ConfigError::msg(format!("cannot read {label} {}: {exc}", path.display())))
}

/// Read a secret from `path`, refusing group/other-readable files.
///
/// # Errors
/// When the file is too permissive, unreadable, or empty.
pub fn read_secret_file(path: &Path, label: &str) -> Result<String> {
    require_private_mode(path, label)?;
    let secret = fs::read_to_string(path)
        .map_err(|exc| ConfigError::msg(format!("cannot read {}: {exc}", path.display())))?
        .trim()
        .to_owned();
    if secret.is_empty() {
        return Err(ConfigError::msg(format!("{} is empty", path.display())));
    }
    Ok(secret)
}

/// Write `secret` to `path` with mode 0600, creating parents 0700.
///
/// # Errors
/// When the directory or the file cannot be created or written.
pub fn write_secret_file(path: &Path, secret: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut handle = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|exc| ConfigError::msg(format!("cannot write {}: {exc}", tmp.display())))?;
    handle
        .write_all(secret.as_bytes())
        .and_then(|()| handle.flush())
        .map_err(|exc| ConfigError::msg(format!("cannot write {}: {exc}", tmp.display())))?;
    drop(handle);
    fs::rename(&tmp, path)
        .map_err(|exc| ConfigError::msg(format!("cannot write {}: {exc}", path.display())))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|exc| ConfigError::msg(format!("cannot chmod {}: {exc}", path.display())))
}

/// Create `dir` (and its parents) 0700 if it is not there already.
///
/// # Errors
/// When the directory cannot be created.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)
        .map_err(|exc| ConfigError::msg(format!("cannot create {}: {exc}", dir.display())))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|exc| ConfigError::msg(format!("cannot chmod {}: {exc}", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        let mut handle = fs::File::create(&path).expect("the test can write in its tmpdir");
        handle.write_all(body.as_bytes()).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
        path
    }

    fn minimal(dir: &Path, extra: &str) -> PathBuf {
        let token = write(dir, "token", "syt_dummy", 0o600);
        let body = format!(
            "homeserver: https://matrix.example.com/\n\
             user_id: \"@bot-a:example.com\"\n\
             access_token_file: {}\n\
             rooms:\n  - \"!room:example.com\"\n\
             state_dir: {}\n\
             brain:\n  kind: echo\n{extra}",
            token.display(),
            dir.join("state").display()
        );
        write(dir, "config.yaml", &body, 0o600)
    }

    #[test]
    fn a_minimal_config_parses_with_the_shipped_defaults() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = load_config(&minimal(dir.path(), "")).expect("the minimal config loads");
        assert_eq!(cfg.homeserver, "https://matrix.example.com");
        assert_eq!(cfg.localpart(), "bot-a");
        assert_eq!(cfg.policy.budgets.per_pair_per_minute, 3);
        assert_eq!(cfg.policy.bot_to_bot, BotToBot::Mentions);
        assert!(cfg.policy.answer_unaddressed);
        assert_eq!(cfg.history_limit, 40);
        assert_eq!(cfg.transcript_keep, 5000);
        assert_eq!(cfg.transcript_archives, 4);
        assert_eq!(cfg.mcp.post_as, PostAs::Notice);
    }

    #[test]
    fn the_shipped_example_config_parses() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.example.yaml");
        let cfg = load_config(&example).expect("the annotated example config loads");
        assert_eq!(cfg.user_id, "@my-agent:example.com");
        let brain = cfg.brain.as_ref().expect("it configures a brain");
        assert_eq!(brain.kind, BrainKind::OpenaiCompat);
        let openai = brain.openai_compat.as_ref().expect("with an endpoint");
        assert_eq!(
            openai.extra_body.get("chat_template_kwargs"),
            Some(&serde_json::json!({ "enable_thinking": false }))
        );
        // The example also carries the claude_code section and the S6 knobs at
        // their defaults: a full config has to load on this build.
        assert!(brain.claude_code.is_some());
    }

    #[test]
    fn every_knob_in_the_schema_is_one_this_build_acts_on() {
        // There is no "parsed but ignored" list any more: R4 finished the last
        // of it (`mcp.post_as`), so a config that sets anything the schema
        // knows about runs here.
        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = load_config(&minimal(dir.path(), "policy:\n  heartbeat_minutes: 5\n"))
            .expect("it parses");
        assert_eq!(cfg.policy.heartbeat_minutes, 5);

        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = load_config(&minimal(dir.path(), "mcp:\n  post_as: text\n")).expect("it parses");
        assert_eq!(cfg.mcp.post_as, PostAs::Text);
        assert_eq!(cfg.mcp.post_as.msgtype(), "m.text");

        // And a post_as the schema does not know is refused rather than
        // silently posting as something else.
        let dir = tempfile::tempdir().expect("tmpdir");
        assert!(load_config(&minimal(dir.path(), "mcp:\n  post_as: shout\n")).is_err());

        // The transcript cap and its archives: what is set here is what the
        // connector hands `Transcript::with_rotation`, so a config that asks
        // for a small cap gets a small file.
        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = load_config(&minimal(
            dir.path(),
            "transcript_keep: 20\ntranscript_archives: 2\n",
        ))
        .expect("it parses");
        assert_eq!(cfg.transcript_keep, 20);
        assert_eq!(cfg.transcript_archives, 2);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = load_config(&minimal(dir.path(), "nonsense: 1\n")).expect_err("refused");
        assert!(format!("{err}").contains("nonsense"), "{err}");
    }

    #[test]
    fn exactly_one_credential_is_required() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = load_config(&minimal(dir.path(), "password: hunter2\n")).expect_err("refused");
        assert!(format!("{err}").contains("exactly one"), "{err}");
    }

    #[test]
    fn a_backoff_range_that_is_not_one_is_refused() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = load_config(&minimal(dir.path(), "policy:\n  backoff_s: [40, 5]\n"))
            .expect_err("refused");
        assert!(format!("{err}").contains("backoff_s"), "{err}");
    }

    #[test]
    fn a_permission_mode_that_bypasses_checks_is_refused() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = load_config(&minimal(
            dir.path(),
            "  claude_code:\n    permission_mode: bypassPermissions\n",
        ))
        .expect_err("refused");
        assert!(
            format!("{err}").contains("read-only by construction"),
            "{err}"
        );
    }

    #[test]
    fn permission_skipping_extra_args_are_refused() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = load_config(&minimal(
            dir.path(),
            "  claude_code:\n    extra_args: [\"--dangerously-skip-permissions\"]\n",
        ))
        .expect_err("refused");
        assert!(format!("{err}").contains("permission checks"), "{err}");
    }

    #[test]
    fn inner_thoughts_are_refused_on_a_brain_that_is_billed_per_call() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let token = write(dir.path(), "token", "syt_dummy", 0o600);
        let body = format!(
            "homeserver: https://matrix.example.com\n\
             user_id: \"@bot-a:example.com\"\n\
             access_token_file: {}\n\
             rooms: [\"!room:example.com\"]\n\
             state_dir: {}\n\
             brain:\n  kind: claude_code\n  claude_code: {{}}\n\
             policy:\n  inner_thoughts: true\n",
            token.display(),
            dir.path().join("state").display()
        );
        let path = write(dir.path(), "config.yaml", &body, 0o600);
        let err = load_config(&path).expect_err("refused");
        assert!(format!("{err}").contains("paid Claude call"), "{err}");
    }

    #[test]
    fn a_loose_token_file_is_refused() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let token = write(dir.path(), "token", "syt_dummy", 0o644);
        let err = read_secret_file(&token, "access_token_file").expect_err("refused");
        assert!(format!("{err}").contains("chmod 600"), "{err}");
        assert!(format!("{err}").contains(ALLOW_LOOSE_PERMS_ENV), "{err}");
    }

    #[test]
    fn the_permission_rule_is_group_and_other_and_the_override_waives_it() {
        // The decision itself, without a filesystem or an environment: 0600 and
        // 0700 pass, anything a second person can read does not, and the
        // override is the only thing that lets one through.
        for mode in [0o600, 0o400, 0o700] {
            assert!(
                permission_verdict(mode, false).is_ok(),
                "{mode:04o} is private"
            );
        }
        for mode in [0o640, 0o604, 0o666, 0o644] {
            let refused = permission_verdict(mode, false).expect_err("not private");
            assert!(refused.contains("chmod 600"), "{refused}");
            assert!(
                permission_verdict(mode, true).is_ok(),
                "the override must waive {mode:04o}"
            );
        }
    }

    #[test]
    fn an_empty_token_file_is_refused() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let token = write(dir.path(), "token", "   \n", 0o600);
        let err = read_secret_file(&token, "access_token_file").expect_err("refused");
        assert!(format!("{err}").contains("is empty"), "{err}");
    }

    #[test]
    fn a_written_secret_is_0600_in_a_0700_directory() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("credentials").join("token.access");
        write_secret_file(&path, "syt_new").expect("written");
        assert_eq!(
            read_secret_file(&path, "cached token").expect("read"),
            "syt_new"
        );
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let parent = fs::metadata(path.parent().expect("has a parent"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent, 0o700);
    }

    #[test]
    fn room_state_paths_are_filesystem_safe() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = load_config(&minimal(dir.path(), "")).expect("loads");
        let path = cfg.room_state_path("!abc:example.com", ".ledger.json");
        assert!(
            path.ends_with("rooms/_abc_example.com.ledger.json"),
            "{path:?}"
        );
        assert!(cfg.store_path().ends_with("_bot-a_example.com.store"));
        assert!(
            cfg.recovery_key_path()
                .ends_with("_bot-a_example.com.recovery-key")
        );
    }

    #[test]
    fn paths_are_expanded() {
        let home = std::env::var("HOME").expect("a HOME");
        assert_eq!(expand("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand("~"), PathBuf::from(&home));
        assert_eq!(expand("/absolute"), PathBuf::from("/absolute"));
    }

    /// A throwaway self-signed pair, generated per test: nothing resembling a
    /// private key is ever committed to this repo.
    fn throwaway_pem() -> (String, String) {
        let key = rcgen::KeyPair::generate().expect("a key pair");
        let cert = rcgen::CertificateParams::new(vec!["agent-room.test".to_owned()])
            .expect("params")
            .self_signed(&key)
            .expect("a self-signed certificate");
        (cert.pem(), key.serialize_pem())
    }

    /// The mTLS identity has to survive the rustls-only build.
    ///
    /// `reqwest` has two TLS backends and `Identity::from_pem` exists in both;
    /// only the rustls one is compiled in here, and only the rustls one parses
    /// a PKCS#8 key out of a concatenated PEM. A release that dropped to the
    /// other backend, or a build without `rustls`, would fail exactly here and
    /// nowhere else until a homeserver behind mTLS refused the connector.
    #[test]
    fn an_mtls_identity_loads_into_the_rustls_client() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (cert_pem, key_pem) = throwaway_pem();
        let cert = write(dir.path(), "client.crt", &cert_pem, 0o644);
        let key = write(dir.path(), "client.key", &key_pem, 0o600);
        let tls = TlsConfig {
            enabled: true,
            client_cert: Some(cert.clone()),
            client_key: Some(key.clone()),
            ca_file: None,
            verify: true,
        };
        tls.build_client()
            .expect("rustls accepts a PEM certificate and its PKCS#8 key");

        // ... and with a private CA to trust as well, which is the shape a
        // homeserver behind its own CA actually needs.
        let ca = write(dir.path(), "ca.pem", &cert_pem, 0o644);
        let with_ca = TlsConfig {
            ca_file: Some(ca),
            ..tls.clone()
        };
        with_ca
            .build_client()
            .expect("rustls accepts an extra root certificate");

        // Teeth: the key half is really parsed. Hand it the certificate twice
        // and rustls has no private key, so the identity - and the client -
        // must be refused rather than silently built without one.
        let no_key = write(dir.path(), "not-a-key", &cert_pem, 0o600);
        let error = TlsConfig {
            client_key: Some(no_key),
            ..tls
        }
        .build_client()
        .expect_err("a PEM with no private key in it is not an identity");
        assert!(error.to_string().contains("tls.client_key"), "{error}");
    }

    #[test]
    fn a_loose_tls_private_key_is_refused() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (cert_pem, key_pem) = throwaway_pem();
        let tls = TlsConfig {
            enabled: true,
            client_cert: Some(write(dir.path(), "client.crt", &cert_pem, 0o644)),
            client_key: Some(write(dir.path(), "client.key", &key_pem, 0o644)),
            ca_file: None,
            verify: true,
        };
        let error = tls
            .build_client()
            .expect_err("a 0644 private key is refused");
        assert!(error.to_string().contains("tls.client_key"), "{error}");
    }

    #[test]
    fn a_judge_on_another_endpoint_gets_its_own_key_and_body() {
        let brain = OpenAiCompatBrainConfig {
            base_url: "http://big:8002/v1".to_owned(),
            model: "qwen3.8-27b".to_owned(),
            api_key: "big-key".to_owned(),
            cold_start_timeout_s: 600.0,
            extra_body: BTreeMap::from([("a".to_owned(), Value::from(1))]),
            max_tokens: 600,
            judge_model: "qwen3:4b".to_owned(),
            judge_max_tokens: 60,
            judge_base_url: "http://small:3000/v1".to_owned(),
            judge_api_key: "small-key".to_owned(),
            judge_extra_body: BTreeMap::new(),
            warm_on_intent: false,
            warm_cooldown_s: 120.0,
        };
        assert_eq!(
            brain.resolved_judge_url(),
            "http://small:3000/v1/chat/completions"
        );
        assert_eq!(brain.resolved_judge_api_key(), "small-key");
        assert!(
            brain.resolved_judge_body().is_empty(),
            "one server's body knobs are another server's 400"
        );
    }

    #[test]
    fn a_judge_on_the_same_endpoint_inherits_what_that_endpoint_needs() {
        let brain = OpenAiCompatBrainConfig {
            base_url: "http://big:8002/v1/".to_owned(),
            model: "qwen3.8-27b".to_owned(),
            api_key: "big-key".to_owned(),
            cold_start_timeout_s: 600.0,
            extra_body: BTreeMap::from([("a".to_owned(), Value::from(1))]),
            max_tokens: 600,
            judge_model: String::new(),
            judge_max_tokens: 60,
            judge_base_url: String::new(),
            judge_api_key: String::new(),
            judge_extra_body: BTreeMap::new(),
            warm_on_intent: false,
            warm_cooldown_s: 120.0,
        };
        assert_eq!(
            brain.resolved_judge_url(),
            "http://big:8002/v1/chat/completions"
        );
        assert_eq!(brain.resolved_judge_api_key(), "big-key");
        assert_eq!(brain.resolved_judge_body().len(), 1);
    }
}
