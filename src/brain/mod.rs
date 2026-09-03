//! The brain contract.
//!
//! A brain answers two questions: given what happened in the room, what should
//! I say ([`Brain::reply`]), and - when nobody addressed me - is there anything
//! here worth saying at all ([`Brain::judge`]). Everything else (when to speak,
//! back-offs, budgets, threading, receipts) is the connector's job, never the
//! brain's.
//!
//! `judge` has a default implementation that always says no, so an adapter that
//! only knows how to talk stays a valid brain: it simply never speaks
//! unprompted.

pub mod claude_code;
pub mod echo;
pub mod judging;
pub mod openai_compat;
pub mod rendering;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{BrainConfig, BrainKind, ConfigError};
use crate::events::RoomEvent;
use crate::ledger::Clock;

pub use claude_code::ClaudeCodeBrain;
pub use echo::EchoBrain;
pub use judging::{judge_prompt, judge_system_prompt, parse_judgement};
pub use openai_compat::OpenAiCompatBrain;
pub use rendering::{render_conversation, render_event};

/// A verdict written the way the Python this was ported from wrote it.
///
/// The live gates grep the connector's own decision lines (`speak=False` in
/// G7), and Rust would print `false`. The wording of a decision line is part of
/// the contract with those gates, not decoration.
#[must_use]
pub fn python_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

/// Why a brain is being asked to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Occasion {
    /// Somebody addressed me, the answer is going to be posted.
    #[default]
    Reply,
    /// Tier 2 - nobody addressed me; would I add anything?
    Unaddressed,
    /// The room has been quiet on a timer; anything worth raising?
    Heartbeat,
    /// Something happened to ME (`ctx.note`); is it worth telling them?
    Impulse,
    /// I left something open (`ctx.note`); do I come back to it?
    Followup,
    /// I have been following this without saying anything, and the wanting-to
    /// -speak has added up past the threshold.
    InnerThought,
}

impl Occasion {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Unaddressed => "unaddressed",
            Self::Heartbeat => "heartbeat",
            Self::Impulse => "impulse",
            Self::Followup => "followup",
            Self::InnerThought => "inner_thought",
        }
    }

    /// Occasions where the trigger is a message being ANSWERED.
    ///
    /// Everywhere else the trigger is only an anchor - the room's last line, so
    /// the brain knows where the conversation stands - and rendering must not
    /// put it last as if it were a question waiting for a reply.
    #[must_use]
    pub fn is_answering(self) -> bool {
        matches!(self, Self::Reply | Self::Unaddressed)
    }
}

/// Everything a brain gets for one turn.
#[derive(Debug, Clone)]
pub struct BrainContext {
    pub persona: String,
    pub me: String,
    pub room_id: String,
    /// The message being answered. For an unprompted turn: the room's last one.
    pub trigger: RoomEvent,
    /// Recent room events, oldest first, including the trigger.
    pub history: Vec<RoomEvent>,
    /// Events of the trigger's thread, oldest first. Empty outside a thread.
    pub thread: Vec<RoomEvent>,
    pub occasion: Occasion,
    /// What this occasion is about when the room is not. Empty for `reply` and
    /// `unaddressed`, where the room IS the subject.
    pub note: String,
    /// Ask the judge for an urgency as well as a verdict (inner thoughts).
    pub want_urgency: bool,
}

/// The most a judge may claim it wants to speak. The scale is deliberately
/// tiny: a model asked for 0-100 answers 70 to everything.
pub const MAX_URGENCY: i32 = 3;

/// A brain's answer to "should I speak here?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub speak: bool,
    pub why: String,
    /// 0-3: how much this brain wanted to say something, whatever it decided.
    pub urgency: i32,
}

impl Judgement {
    #[must_use]
    pub fn new(speak: bool, why: impl Into<String>, urgency: i32) -> Self {
        Self {
            speak,
            why: why.into(),
            urgency,
        }
    }

    #[must_use]
    pub fn no(why: impl Into<String>) -> Self {
        Self::new(false, why, 0)
    }
}

/// What every adapter implements.
#[async_trait]
pub trait Brain: Send + Sync {
    /// Return the message to post, or None to stay quiet.
    async fn reply(&self, ctx: &BrainContext) -> Option<String>;

    /// Cheap, fresh-context "do I add anything nobody has said?".
    ///
    /// The default is silence: an adapter that does not implement it never
    /// speaks unprompted, which is the safe failure everywhere in this project.
    async fn judge(&self, _ctx: &BrainContext) -> Judgement {
        Judgement::no("this brain has no judge, so it never speaks unprompted")
    }

    /// A turn is probably coming. Get ready if that means anything to you.
    ///
    /// Called when a human starts typing in a watched room. It must return
    /// immediately: it is a hint about the future, not a step in a turn.
    async fn warm(&self, _reason: &str) {}

    /// Release any resources (HTTP clients, subprocesses).
    async fn close(&self) {}
}

/// Instantiate the brain named by `brain.kind`.
///
/// `state_dir` and `clock` are the Claude Code brain's: it keeps one session
/// file per room beside that room's ledger, and its usage-limit cooldown is on
/// the same injected clock everything else in this crate uses.
///
/// # Errors
/// When the section the kind names is missing, or when the brain refuses its
/// own configuration (a `cwd` that is not a directory).
pub fn build_brain(
    cfg: &BrainConfig,
    http: reqwest::Client,
    state_dir: &Path,
    clock: Clock,
) -> Result<Arc<dyn Brain>, ConfigError> {
    match cfg.kind {
        BrainKind::Echo => Ok(Arc::new(EchoBrain::new(cfg.echo.clone()))),
        BrainKind::OpenaiCompat => {
            let section = cfg.openai_compat.as_ref().ok_or_else(|| {
                ConfigError::Invalid("brain.openai_compat section is missing".to_owned())
            })?;
            Ok(Arc::new(OpenAiCompatBrain::new(section.clone(), http)))
        }
        BrainKind::ClaudeCode => {
            let section = cfg.claude_code.as_ref().ok_or_else(|| {
                ConfigError::Invalid("brain.claude_code section is missing".to_owned())
            })?;
            Ok(Arc::new(ClaudeCodeBrain::new(
                section.clone(),
                state_dir.to_path_buf(),
                clock,
            )?))
        }
    }
}
