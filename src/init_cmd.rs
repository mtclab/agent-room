//! `agent-room init`: one non-interactive command that writes a working setup.
//!
//! A friend should not have to hand-write YAML to join a room. `init` takes
//! flags only - nothing is prompted, so it works over SSH, in a script and in a
//! gate - and leaves behind exactly two files plus, when it logged in for you,
//! one cached token:
//!
//! ```text
//! <out>/config.yaml    0600, validated by `load_config` before init exits
//! <out>/persona.md     0600, the shipped template with the name filled in
//! <state_dir>/credentials/<user>.access   0600, written by the login
//! ```
//!
//! **The password is never written anywhere.** `--password-from-stdin` reads
//! it, uses it once to log in, and what lands on disk is the access token the
//! homeserver gave back. That is the whole point of the flag: the owner hands a
//! friend a password once, and the friend's machine turns it into a token
//! nobody has to email around.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::{ArgAction, ArgGroup, Args, ValueEnum};
use regex::Regex;
use serde::Serialize;
use tracing::error;

use crate::config::{
    BrainKind, ClaudeCodeBrainConfig, Config, McpConfig, OpenAiCompatBrainConfig, PolicyConfig,
    TlsConfig, default_history_limit, default_transcript_archives, default_transcript_keep, expand,
    load_config, require_private_mode, write_secret_file,
};
use crate::cs_api::{CommandClient, CsError, authenticate};
use crate::events::localpart;

/// Where a person's own agent lives when they say nothing else.
pub const DEFAULT_OUT: &str = "~/.config/agent-room";
/// State (transcripts, ledgers, the cached token) is per agent, not per config
/// directory: two agents on one machine need two of these.
pub const DEFAULT_STATE_DIR: &str = "~/.local/state/agent-room";
pub const CONFIG_NAME: &str = "config.yaml";
pub const PERSONA_NAME: &str = "persona.md";

/// The shipped template, compiled in, so one binary carries it.
pub const PERSONA_TEMPLATE: &str = include_str!("templates/persona.md");

/// What separates the template's instructions from the persona itself.
pub const TEMPLATE_SEPARATOR: &str = "\n---\n";
/// The blank `init` fills in itself, from `--display-name` or the localpart.
pub const NAME_BLANK: &str = "<name>";

/// A blank a person still has to fill in the persona: `<like this>`.
static BLANK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^<>]+>").expect("the blank pattern is a literal"));
static USER_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@[^:\s]+:[^:\s]+$").expect("the user id pattern is a literal"));
// A room ID is `!opaque` with an OPTIONAL `:server` suffix: homeservers on room
// version 12 (Synapse 1.150+) mint ids with no server part at all, and the old
// `!id:server` validator turned away every room they create (readiness walk,
// 2026-09-03). An alias is still `#alias:server`.
static ROOM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:![^:\s]+(?::[^:\s]+)?|#[^:\s]+:[^:\s]+)$")
        .expect("the room pattern is a literal")
});

/// Anything that stops `init` before it writes. Exit code 2, one line said.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InitError(pub String);

impl InitError {
    fn msg(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

type Result<T> = std::result::Result<T, InitError>;

// -- the persona template ----------------------------------------------------

/// Just the persona: everything after the template's `---` separator.
///
/// The instructions above the line are for the person editing the file. They
/// must not reach the model - the connector sends the whole persona file on
/// every message, and "replace every `<...>`" is not something an agent should
/// be reading about itself.
///
/// # Errors
/// When the template has no separator, which would send the instructions to the
/// model rather than dropping them.
pub fn persona_body(template: &str) -> Result<String> {
    let Some((_instructions, body)) = template.split_once(TEMPLATE_SEPARATOR) else {
        return Err(InitError::msg(
            "the shipped persona template has no `---` separator",
        ));
    };
    Ok(format!("{}\n", body.trim()))
}

/// The persona `init` writes: the template with the agent's name in it.
///
/// # Errors
/// When the template has no `---` separator.
pub fn render_persona(name: &str, template: &str) -> Result<String> {
    Ok(persona_body(template)?.replace(NAME_BLANK, name))
}

/// How many `<...>` blanks are left for the person to fill in.
#[must_use]
pub fn count_blanks(text: &str) -> usize {
    BLANK.find_iter(text).count()
}

// -- flags -------------------------------------------------------------------

fn user_id_arg(value: &str) -> std::result::Result<String, String> {
    if USER_ID_RE.is_match(value) {
        return Ok(value.to_owned());
    }
    Err(format!("'{value}' is not a Matrix user id (@name:server)"))
}

fn room_arg(value: &str) -> std::result::Result<String, String> {
    if ROOM_RE.is_match(value) {
        return Ok(value.to_owned());
    }
    Err(format!(
        "'{value}' is not a room id or alias (!id, !id:server or #alias:server)"
    ))
}

fn homeserver_arg(value: &str) -> std::result::Result<String, String> {
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(value.trim_end_matches('/').to_owned());
    }
    Err(format!(
        "'{value}' must start with https:// (or http:// locally)"
    ))
}

/// Which brain the connector this config describes will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BrainChoice {
    /// Any OpenAI-compatible endpoint.
    #[value(name = "openai_compat", alias = "openai-compat")]
    OpenaiCompat,
    /// The Claude Code CLI.
    #[value(name = "claude_code", alias = "claude-code")]
    ClaudeCode,
}

impl BrainChoice {
    fn kind(self) -> BrainKind {
        match self {
            Self::OpenaiCompat => BrainKind::OpenaiCompat,
            Self::ClaudeCode => BrainKind::ClaudeCode,
        }
    }
}

/// Every flag `init` takes. Nothing is ever prompted for.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("credential")
        .required(true)
        .args(["password_from_stdin", "token_file"])
))]
pub struct InitArgs {
    /// https://...
    #[arg(long, value_parser = homeserver_arg)]
    pub homeserver: String,
    /// @your-agent:server
    #[arg(long = "user", value_parser = user_id_arg)]
    pub user: String,
    /// A room to join; repeat for more than one.
    #[arg(long = "room", value_name = "!id[:server]", action = ArgAction::Append,
          required = true, value_parser = room_arg)]
    pub rooms: Vec<String>,
    /// `openai_compat` = any OpenAI-compatible endpoint; `claude_code` = the Claude
    /// Code CLI.
    #[arg(long, value_enum)]
    pub brain: BrainChoice,
    /// Directory for config.yaml and persona.md.
    #[arg(long, default_value = DEFAULT_OUT)]
    pub out: PathBuf,
    /// Transcripts, ledgers and the cached token; one agent per state directory.
    #[arg(long = "state-dir", default_value = DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
    /// Read the account password from stdin, log in once, keep only the token.
    #[arg(long = "password-from-stdin")]
    pub password_from_stdin: bool,
    /// An existing 0600 file holding an access token.
    #[arg(long = "token-file")]
    pub token_file: Option<PathBuf>,
    /// Client certificate (PEM), for an mTLS server.
    #[arg(long = "tls-cert")]
    pub tls_cert: Option<PathBuf>,
    /// Client key (PEM, 0600).
    #[arg(long = "tls-key")]
    pub tls_key: Option<PathBuf>,
    /// CA bundle that signed the homeserver.
    #[arg(long = "tls-ca")]
    pub tls_ca: Option<PathBuf>,
    /// e.g. <http://localhost:11434/v1>
    #[arg(long = "openai-base-url")]
    pub openai_base_url: Option<String>,
    /// The model id that endpoint serves.
    #[arg(long = "openai-model")]
    pub openai_model: Option<String>,
    /// Reply model.
    #[arg(long = "claude-model", default_value = "sonnet")]
    pub claude_model: String,
    /// Where the agent stands (default: the state dir).
    #[arg(long = "claude-cwd")]
    pub claude_cwd: Option<PathBuf>,
    /// What the room calls this agent (setting it talks to the homeserver).
    #[arg(long = "display-name")]
    pub display_name: Option<String>,
    /// Overwrite an existing config.yaml / persona.md.
    #[arg(long)]
    pub force: bool,
}

// -- the file this writes ----------------------------------------------------

/// The `brain:` block: the kind, and that adapter's shipped defaults.
#[derive(Debug, Serialize)]
struct BrainSection {
    kind: BrainKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    openai_compat: Option<OpenAiCompatBrainConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_code: Option<ClaudeCodeBrainConfig>,
}

/// The config file's contents: every knob a person may want to turn.
///
/// The policy block is the shipped `PolicyConfig` rather than a copy written
/// out by hand, so what a friend gets is the default BY CONSTRUCTION and not
/// something that goes stale the first time a default changes.
#[derive(Debug, Serialize)]
struct ConfigFile {
    homeserver: String,
    user_id: String,
    access_token_file: String,
    rooms: Vec<String>,
    persona_file: String,
    state_dir: String,
    history_limit: usize,
    transcript_keep: usize,
    transcript_archives: usize,
    tls: TlsConfig,
    brain: BrainSection,
    policy: PolicyConfig,
}

fn build_brain_section(args: &InitArgs, state_dir: &Path) -> Result<BrainSection> {
    match args.brain {
        BrainChoice::OpenaiCompat => {
            let (Some(base_url), Some(model)) = (&args.openai_base_url, &args.openai_model) else {
                return Err(InitError::msg(
                    "--brain openai_compat needs --openai-base-url and --openai-model \
                     (the endpoint and the model id it serves)",
                ));
            };
            Ok(BrainSection {
                kind: BrainKind::OpenaiCompat,
                openai_compat: Some(OpenAiCompatBrainConfig::shipped(base_url, model)),
                claude_code: None,
            })
        }
        BrainChoice::ClaudeCode => Ok(BrainSection {
            kind: args.brain.kind(),
            openai_compat: None,
            claude_code: Some(ClaudeCodeBrainConfig::shipped(
                &args.claude_model,
                args.claude_cwd.clone().map_or_else(
                    || state_dir.to_path_buf(),
                    |cwd| expand(&cwd.to_string_lossy()),
                ),
            )),
        }),
    }
}

fn build_tls(args: &InitArgs) -> Result<TlsConfig> {
    match (&args.tls_cert, &args.tls_key) {
        (None, None) => {
            if args.tls_ca.is_some() {
                return Err(InitError::msg(
                    "--tls-ca is only meaningful with --tls-cert and --tls-key",
                ));
            }
            Ok(TlsConfig::default())
        }
        (Some(cert), Some(key)) => Ok(TlsConfig {
            enabled: true,
            client_cert: Some(cert.clone()),
            client_key: Some(key.clone()),
            ca_file: args.tls_ca.clone(),
            verify: true,
        }),
        _ => Err(InitError::msg(
            "--tls-cert and --tls-key go together (a client certificate is a pair)",
        )),
    }
}

/// The header every written config carries, so the next person knows what it is.
fn render_config(file: &ConfigFile) -> Result<String> {
    let header = format!(
        "# agent-room connector config, written by `agent-room init` on {}.\n\
         # Mode 0600: it points at the access token, and the token IS this account.\n\
         # What every knob does: docs/ONBOARDING.md.\n",
        today()
    );
    // Plain scalars, never folded block scalars: this is a file a friend opens
    // and changes a number in, and `>-` in front of a long path is noise.
    let options = serde_saphyr::ser_options! { prefer_block_scalars: false };
    let body = serde_saphyr::to_string_with_options(file, options)
        .map_err(|exc| InitError::msg(format!("cannot render the config: {exc}")))?;
    Ok(format!("{header}{body}"))
}

/// Today, as `YYYY-MM-DD`, from the system clock.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01 -> (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// -- what one run produces ---------------------------------------------------

/// Everything decided before anything is touched: paths and sections.
#[derive(Debug)]
pub struct Plan {
    pub out: PathBuf,
    pub state_dir: PathBuf,
    pub config_path: PathBuf,
    pub persona_path: PathBuf,
    tls: TlsConfig,
    brain: BrainSection,
}

/// What `init` did, in the order it did it, for the report at the end.
#[derive(Debug)]
pub struct Written {
    pub config_path: PathBuf,
    pub persona_path: PathBuf,
    pub token_path: PathBuf,
    pub state_dir: PathBuf,
    pub logged_in: bool,
    pub display_name: Option<String>,
    pub display_name_error: Option<String>,
    pub blanks: usize,
}

/// Work the whole config out first: no directory, no network, no writing.
///
/// Everything that can be wrong about the flags is wrong here, where nothing
/// has happened yet - a half-written config directory is worse than none, and
/// an `--out` that already holds somebody's agent must be refused before the
/// first `mkdir`.
///
/// # Errors
/// When the flags do not add up, or the output files already exist.
pub fn prepare(args: &InitArgs) -> Result<Plan> {
    let out = expand(&args.out.to_string_lossy());
    let state_dir = expand(&args.state_dir.to_string_lossy());
    let config_path = out.join(CONFIG_NAME);
    let persona_path = out.join(PERSONA_NAME);
    let existing: Vec<String> = [&config_path, &persona_path]
        .iter()
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect();
    if !existing.is_empty() && !args.force {
        return Err(InitError::msg(format!(
            "{} already exist(s); pass --force to overwrite, or --out to write somewhere else",
            existing.join(" and ")
        )));
    }
    let tls = build_tls(args)?;
    let brain = build_brain_section(args, &state_dir)?;
    Ok(Plan {
        out,
        state_dir,
        config_path,
        persona_path,
        tls,
        brain,
    })
}

/// Where the password comes from.
///
/// Stdin in the product - that is the whole point of `--password-from-stdin`.
/// A test hands one over instead, because a suite that runs its cases side by
/// side cannot own the process's stdin.
#[derive(Debug, Clone)]
pub enum PasswordSource {
    Stdin,
    Given(String),
}

/// The password, stripped of the shell's newline. Never stored.
fn read_password(source: &PasswordSource) -> Result<String> {
    let raw = match source {
        PasswordSource::Given(password) => password.clone(),
        PasswordSource::Stdin => {
            let mut raw = String::new();
            std::io::stdin().read_to_string(&mut raw).map_err(|exc| {
                InitError::msg(format!("cannot read the password from stdin: {exc}"))
            })?;
            raw
        }
    };
    let password = raw.trim().to_owned();
    if password.is_empty() {
        return Err(InitError::msg(
            "--password-from-stdin was given but stdin was empty",
        ));
    }
    Ok(password)
}

/// Authenticate and, if asked, set the display name. Returns any complaint.
///
/// A password login caches the token 0600 (that is `authenticate`'s doing, and
/// the reason the password never has to be stored); a token file is simply
/// checked. Either way the homeserver's answer to "who is this?" has to be the
/// account the flags name - a typo in `--user` otherwise produces a config that
/// connects as somebody else.
async fn connect(
    api: &CommandClient,
    cfg: &Config,
    display_name: Option<&str>,
) -> Result<Option<String>> {
    let me = authenticate(api, cfg).await.map_err(|exc| match exc {
        CsError::Refused(message) => InitError::msg(message),
        CsError::Unreachable(cause) => {
            InitError::msg(format!("{} did not answer: {cause}", cfg.homeserver))
        }
    })?;
    if me != cfg.user_id {
        return Err(InitError::msg(format!(
            "the homeserver says this credential belongs to {me}, not {}",
            cfg.user_id
        )));
    }
    let Some(display_name) = display_name else {
        return Ok(None);
    };
    api.set_display_name(&cfg.user_id, display_name)
        .await
        .map_err(|exc| InitError::msg(format!("{} did not answer: {exc}", cfg.homeserver)))
}

fn login_config(args: &InitArgs, plan: &Plan, credential: Credential) -> Config {
    let (access_token_file, password) = match credential {
        Credential::Password(password) => (None, Some(password)),
        Credential::TokenFile(path) => (Some(path), None),
    };
    Config {
        homeserver: args.homeserver.clone(),
        user_id: args.user.clone(),
        access_token_file,
        password,
        rooms: args.rooms.clone(),
        persona_file: None,
        state_dir: plan.state_dir.clone(),
        brain: None,
        policy: PolicyConfig::default(),
        mcp: McpConfig::default(),
        tls: plan.tls.clone(),
        history_limit: default_history_limit(),
        transcript_keep: default_transcript_keep(),
        transcript_archives: default_transcript_archives(),
        allow_wedged_device: false,
    }
}

enum Credential {
    Password(String),
    TokenFile(PathBuf),
}

/// Get an access token onto disk (or check the one already there).
///
/// Both paths end in the same place: a 0600 file the config points at. Only the
/// password path talks to the homeserver, unless a display name was asked for -
/// that needs an authenticated call whatever the credential is.
async fn credential(
    args: &InitArgs,
    plan: &Plan,
    password: &PasswordSource,
) -> Result<(PathBuf, Option<String>)> {
    let display_name = args.display_name.as_deref();
    if args.password_from_stdin {
        let cfg = login_config(args, plan, Credential::Password(read_password(password)?));
        let api = CommandClient::new(&cfg).map_err(|exc| InitError::msg(exc.to_string()))?;
        let complaint = connect(&api, &cfg, display_name).await?;
        return Ok((cfg.cached_token_path(), complaint));
    }

    let token_path = args
        .token_file
        .as_ref()
        .map(|path| expand(&path.to_string_lossy()))
        .ok_or_else(|| InitError::msg("a credential is required"))?;
    if !token_path.is_file() {
        return Err(InitError::msg(format!(
            "--token-file {} does not exist",
            token_path.display()
        )));
    }
    require_private_mode(&token_path, "--token-file")
        .map_err(|exc| InitError::msg(exc.to_string()))?;
    if display_name.is_none() {
        return Ok((token_path, None));
    }
    let cfg = login_config(args, plan, Credential::TokenFile(token_path.clone()));
    let api = CommandClient::new(&cfg).map_err(|exc| InitError::msg(exc.to_string()))?;
    let complaint = connect(&api, &cfg, display_name).await?;
    Ok((token_path, complaint))
}

/// Write the persona and the config, then prove the config parses.
///
/// Returns how many blanks the person still has to fill in the persona.
///
/// The state directory is created here too, 0700, rather than left to the
/// connector: it ends up holding the room's transcript, and a directory made on
/// the way past by `create_dir_all` gets whatever the umask says.
fn write_files(args: &InitArgs, plan: &Plan, token_path: &Path) -> Result<usize> {
    create_private_dir(&plan.out)?;
    create_private_dir(&plan.state_dir)?;
    let name = args
        .display_name
        .clone()
        .unwrap_or_else(|| localpart(&args.user).to_owned());
    let persona = render_persona(&name, PERSONA_TEMPLATE)?;
    write_secret_file(&plan.persona_path, &persona)
        .map_err(|exc| InitError::msg(exc.to_string()))?;
    let file = ConfigFile {
        homeserver: args.homeserver.clone(),
        user_id: args.user.clone(),
        access_token_file: token_path.display().to_string(),
        rooms: args.rooms.clone(),
        persona_file: plan.persona_path.display().to_string(),
        state_dir: plan.state_dir.display().to_string(),
        history_limit: default_history_limit(),
        transcript_keep: default_transcript_keep(),
        transcript_archives: default_transcript_archives(),
        tls: plan.tls.clone(),
        brain: BrainSection {
            kind: plan.brain.kind,
            openai_compat: plan.brain.openai_compat.clone(),
            claude_code: plan.brain.claude_code.clone(),
        },
        policy: PolicyConfig::default(),
    };
    write_secret_file(&plan.config_path, &render_config(&file)?)
        .map_err(|exc| InitError::msg(exc.to_string()))?;
    // The last word on whether this file is any good belongs to the loader the
    // connector itself uses, not to the code that just wrote it.
    load_config(&plan.config_path).map_err(|exc| InitError::msg(exc.to_string()))?;
    Ok(count_blanks(&persona))
}

fn create_private_dir(dir: &Path) -> Result<()> {
    crate::config::create_private_dir(dir).map_err(|exc| InitError::msg(exc.to_string()))?;
    // `create_private_dir` leaves an existing directory alone; the state dir
    // has to BE 0700 whether init made it or found it.
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|exc| InitError::msg(format!("cannot chmod {}: {exc}", dir.display())))
}

/// What a person sees when it worked.
pub fn report(written: &Written) {
    println!("wrote {} (0600)", written.config_path.display());
    println!("wrote {} (0600)", written.persona_path.display());
    println!(
        "state (transcripts, ledgers) goes in {} (0700)",
        written.state_dir.display()
    );
    if written.logged_in {
        println!(
            "logged in and cached the access token in {} (0600)",
            written.token_path.display()
        );
        println!("the password was used once and written nowhere");
    } else {
        println!("using the access token in {}", written.token_path.display());
    }
    if let Some(name) = &written.display_name
        && written.display_name_error.is_none()
    {
        println!("set this account's display name to \"{name}\"");
    }
    if let Some(complaint) = &written.display_name_error {
        println!("could NOT set the display name: {complaint}");
        println!("  fix: set it in your Matrix client, or re-run init once the server lets you");
    }
    if written.blanks > 0 {
        println!(
            "\n{} blank(s) left in {}: fill them in first.",
            written.blanks,
            written.persona_path.display()
        );
    }
    println!("\nnext:");
    println!(
        "  agent-room doctor --config {}",
        written.config_path.display()
    );
    println!(
        "  agent-room run --config {}",
        written.config_path.display()
    );
}

/// Write a config and a persona, or refuse and say why. 0 = written.
pub async fn run(args: &InitArgs) -> i32 {
    run_with(args, &PasswordSource::Stdin).await
}

/// `run`, with the password handed over rather than read from stdin.
pub async fn run_with(args: &InitArgs, password: &PasswordSource) -> i32 {
    match write_everything(args, password).await {
        Ok(written) => {
            report(&written);
            i32::from(written.display_name_error.is_some()) * 2
        }
        Err(exc) => {
            error!("{exc}");
            2
        }
    }
}

async fn write_everything(args: &InitArgs, password: &PasswordSource) -> Result<Written> {
    let plan = prepare(args)?;
    let (token_path, display_name_error) = credential(args, &plan, password).await?;
    let blanks = write_files(args, &plan, &token_path)?;
    Ok(Written {
        config_path: plan.config_path,
        persona_path: plan.persona_path,
        token_path,
        state_dir: plan.state_dir,
        logged_in: args.password_from_stdin,
        display_name: args.display_name.clone(),
        display_name_error,
        blanks,
    })
}
