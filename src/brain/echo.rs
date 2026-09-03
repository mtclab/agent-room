//! Deterministic test brain.
//!
//! Replies `echo: <body>`, so a live journey can assert on the exact text
//! without a model in the loop. Markers in the trigger body steer it:
//!
//! - `[[silent]]` - reply returns None (the "brain declined to speak" path).
//! - `[[speak]]`  - the judge says yes; without it the judge says no. That is
//!   what makes the judged gates deterministic: no model, no coin flips, the
//!   same verdict every run.
//!
//! Three config options exist for the same reason, because a marker in the body
//! can only say things about THIS message and the gates need standing
//! behaviour: `mention_back` names a user id every reply ends with (so one echo
//! bot can address another), `ask_back` ends every reply with a question, and
//! `urgency` is what the judge reports on the inner-thoughts axis.

use async_trait::async_trait;

use crate::brain::{Brain, BrainContext, Judgement};
use crate::config::EchoBrainConfig;

pub const SILENT_MARKER: &str = "[[silent]]";
pub const SPEAK_MARKER: &str = "[[speak]]";

/// The brain used by the live gates: no network, no model, no surprises.
#[derive(Debug, Clone, Default)]
pub struct EchoBrain {
    pub cfg: EchoBrainConfig,
}

impl EchoBrain {
    #[must_use]
    pub fn new(cfg: EchoBrainConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Brain for EchoBrain {
    async fn reply(&self, ctx: &BrainContext) -> Option<String> {
        if ctx.trigger.body.contains(SILENT_MARKER) {
            return None;
        }
        // Unprompted occasions are not about the trigger, they are about the
        // note (the impulse, the loop, the thought), so that is what is echoed.
        let subject = if ctx.note.is_empty() {
            &ctx.trigger.body
        } else {
            &ctx.note
        };
        let mut text = format!("echo: {subject}");
        if !self.cfg.mention_back.is_empty() && self.cfg.mention_back != ctx.me {
            text = format!("{text} {}", self.cfg.mention_back);
        }
        if !self.cfg.ask_back.is_empty() {
            text = format!("{text} {}", self.cfg.ask_back);
        }
        Some(text)
    }

    async fn judge(&self, ctx: &BrainContext) -> Judgement {
        let urgency = if ctx.want_urgency {
            self.cfg.urgency
        } else {
            0
        };
        if ctx.trigger.body.contains(SPEAK_MARKER) || ctx.note.contains(SPEAK_MARKER) {
            return Judgement::new(true, format!("the trigger carries {SPEAK_MARKER}"), urgency);
        }
        Judgement::new(false, format!("no {SPEAK_MARKER} in the trigger"), urgency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::Occasion;
    use crate::events::RoomEvent;
    use std::collections::BTreeSet;

    fn context(body: &str) -> BrainContext {
        BrainContext {
            persona: String::new(),
            me: "@bot-a:example.com".to_owned(),
            room_id: "!room:example.com".to_owned(),
            trigger: RoomEvent {
                event_id: "$t".to_owned(),
                room_id: "!room:example.com".to_owned(),
                sender: "@human:example.com".to_owned(),
                sender_display: None,
                body: body.to_owned(),
                formatted_body: None,
                msgtype: "m.text".to_owned(),
                ts: 1.0,
                thread_root: None,
                reply_to: None,
                reply_is_fallback: false,
                mentions: BTreeSet::new(),
                is_bot: false,
            },
            history: Vec::new(),
            thread: Vec::new(),
            occasion: Occasion::Reply,
            note: String::new(),
            want_urgency: false,
        }
    }

    #[tokio::test]
    async fn it_echoes_the_body() {
        let brain = EchoBrain::default();
        assert_eq!(
            brain.reply(&context("hello there")).await.as_deref(),
            Some("echo: hello there")
        );
    }

    #[tokio::test]
    async fn the_silent_marker_is_the_declined_to_speak_path() {
        let brain = EchoBrain::default();
        assert!(brain.reply(&context("[[silent]] please")).await.is_none());
    }

    #[tokio::test]
    async fn mention_back_and_ask_back_are_appended() {
        let brain = EchoBrain::new(EchoBrainConfig {
            mention_back: "@bot-b:example.com".to_owned(),
            ask_back: "and you?".to_owned(),
            urgency: 0,
        });
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("echo: hi @bot-b:example.com and you?")
        );
    }

    #[tokio::test]
    async fn mention_back_never_names_me() {
        let brain = EchoBrain::new(EchoBrainConfig {
            mention_back: "@bot-a:example.com".to_owned(),
            ..EchoBrainConfig::default()
        });
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("echo: hi")
        );
    }

    #[tokio::test]
    async fn the_judge_says_no_unless_the_trigger_asks_for_a_yes() {
        let brain = EchoBrain::default();
        assert!(!brain.judge(&context("just talking")).await.speak);
        assert!(brain.judge(&context("[[speak]] now")).await.speak);
    }

    #[tokio::test]
    async fn the_note_is_echoed_when_the_room_is_not_the_subject() {
        let brain = EchoBrain::default();
        let mut ctx = context("the last thing said");
        ctx.occasion = Occasion::Impulse;
        ctx.note = "[git] merged PR #5".to_owned();
        assert_eq!(
            brain.reply(&ctx).await.as_deref(),
            Some("echo: [git] merged PR #5")
        );
        assert!(
            brain
                .judge(&{
                    let mut speaking = ctx.clone();
                    speaking.note = "[[speak]] now".to_owned();
                    speaking
                })
                .await
                .speak
        );
    }

    #[tokio::test]
    async fn the_urgency_is_only_reported_when_it_is_asked_for() {
        let brain = EchoBrain::new(EchoBrainConfig {
            urgency: 2,
            ..EchoBrainConfig::default()
        });
        assert_eq!(brain.judge(&context("x")).await.urgency, 0);
        let mut ctx = context("x");
        ctx.want_urgency = true;
        assert_eq!(brain.judge(&ctx).await.urgency, 2);
    }
}
