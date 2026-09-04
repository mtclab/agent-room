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
    /// Ask the judge for an urgency as well as a score (inner thoughts).
    pub want_urgency: bool,
    /// The score at which this agent's judge is a yes
    /// ([`crate::config::PolicyConfig::effective_speak_threshold`]). The brain
    /// reports how much it would add; the operator's configuration is what
    /// turns that into speech.
    pub speak_threshold: u8,
    /// How many people and agents are joined to this room, as the member store
    /// has it. 0 when the member list has not arrived yet. The judge is told,
    /// because "would anybody else answer this?" has a different answer in a
    /// room of three than in a room of thirty.
    pub participants: usize,
}

impl BrainContext {
    /// Whether I am already part of this exchange.
    ///
    /// The thread when there is one, the room's recent lines when there is not.
    /// A deterministic cue for the judge: joining in on a conversation I have
    /// been in is not the same act as walking into somebody else's.
    #[must_use]
    pub fn i_took_part(&self) -> bool {
        let conversation = if self.thread.is_empty() {
            &self.history
        } else {
            &self.thread
        };
        conversation.iter().any(|ev| ev.sender == self.me)
    }
}

/// The most a judge may claim it wants to speak. The scale is deliberately
/// tiny: a model asked for 0-100 answers 70 to everything.
pub const MAX_URGENCY: i32 = 3;

/// The top of the judge's enthusiasm scale, after Webb: directly addressed is a
/// 9, and a 9 never reaches the judge because being addressed is tier 1.
pub const MAX_JUDGE_SCORE: u8 = 9;

/// A brain's answer to "how much would I add here?".
///
/// A SCORE, not a verdict, because the verdict is the connector's to make: the
/// same 7 speaks in a room whose operator asked for a talkative agent and stays
/// quiet in one that did not (`policy.speak_threshold`, `policy.chattiness`).
/// The old contract asked "would you add something nobody has said?" and got
/// "no, the conversation has naturally settled" - a yes/no question about
/// whether to speak is biased to silence, and a room of agents told to converse
/// answered it with silence (the room log, 2026-09-04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub speak: bool,
    /// 0-9: how much this brain would add here. 0 whenever nothing usable came
    /// back, which is why silence stays the safe failure.
    pub score: u8,
    pub why: String,
    /// 0-3: how much this brain wanted to say something, whatever it decided.
    pub urgency: i32,
}

impl Judgement {
    /// A scored answer, and the threshold that turns it into a verdict.
    #[must_use]
    pub fn scored(score: u8, threshold: u8, why: impl Into<String>, urgency: i32) -> Self {
        Self {
            speak: score >= threshold,
            score: score.min(MAX_JUDGE_SCORE),
            why: why.into(),
            urgency,
        }
    }

    /// No, whatever the threshold: nothing usable came back.
    #[must_use]
    pub fn no(why: impl Into<String>) -> Self {
        Self {
            speak: false,
            score: 0,
            why: why.into(),
            urgency: 0,
        }
    }

    /// How the log says what came back: `7 (>= 5)`, `3 (< 5)`.
    #[must_use]
    pub fn says(&self, threshold: u8) -> String {
        let comparison = if self.speak { ">=" } else { "<" };
        format!("{} ({comparison} {threshold})", self.score)
    }
}

/// What every adapter implements.
#[async_trait]
pub trait Brain: Send + Sync {
    /// Return the message to post, or None to stay quiet.
    async fn reply(&self, ctx: &BrainContext) -> Option<String>;

    /// Cheap, fresh-context "how much would I add here?", 0-9.
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
