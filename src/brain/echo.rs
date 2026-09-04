//! Deterministic test brain.
//!
//! Replies `echo: <body>`, so a live journey can assert on the exact text
//! without a model in the loop. Markers in the trigger body steer it:
//!
//! - `[[silent]]` - reply returns None (the "brain declined to speak" path).
//! - `[[speak]]`  - the judge scores 9; without it, [`EchoBrainConfig::score`]
//!   (0 by default). That is what makes the judged gates deterministic: no
//!   model, no coin flips, the same score every run.
//! - `[[score: N]]` - the judge scores exactly N, for a gate that needs an
//!   answer either side of `policy.speak_threshold`.
//!
//! **The markers never reach the room.** An echo of a marked line used to carry
//! the marker into its own reply, so in a two-agent gate the next judge saw it
//! too and every hop said yes for ever - which made "the thread winds down"
//! untestable next to "the room answered at all". They are the harness's
//! channel to the brain, and one hop is all they get.
//!
//! Five config options exist for the same reason, because a marker in the body
//! can only say things about THIS message and the gates need standing
//! behaviour: `mention_back` names a user id every reply ends with (so one echo
//! bot can address another through `m.mentions`), `name_back` ends every reply
//! with a NAME instead (`..., qwen?` - the typed form, which is all another
//! agent's model can actually produce), `ask_back` ends every reply with a
//! question, `urgency` is what the judge reports on the inner-thoughts axis,
//! and `score` is what it reports when no marker says otherwise.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;

use crate::brain::{Brain, BrainContext, Judgement, MAX_JUDGE_SCORE};
use crate::config::EchoBrainConfig;

pub const SILENT_MARKER: &str = "[[silent]]";
pub const SPEAK_MARKER: &str = "[[speak]]";
/// What [`SPEAK_MARKER`] scores: the top of the scale, so a gate that wants an
/// agent to speak does not have to know the threshold.
pub const SPEAK_SCORE: u8 = MAX_JUDGE_SCORE;

/// `[[score: 4]]` anywhere in the trigger or the note.
static SCORE_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[\[score:\s*(\d)\]\]").expect("a literal pattern"));

/// Every marker, for stripping them out of what is echoed back.
static ANY_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[[^\]]*\]\]").expect("a literal pattern"));

/// What the room sees of a marked line: the line, without the harness's
/// markers and without the double spaces taking one out would leave.
#[must_use]
pub fn without_markers(text: &str) -> String {
    ANY_MARKER
        .replace_all(text, " ")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

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
        let mut text = format!("echo: {}", without_markers(subject));
        if !self.cfg.mention_back.is_empty() && self.cfg.mention_back != ctx.me {
            text = format!("{text} {}", self.cfg.mention_back);
        }
        // A vocative, because that is what makes it an address: a name on the
        // end of a sentence with nothing between them is talk ABOUT somebody.
        if !self.cfg.name_back.is_empty() {
            text = format!("{text}, {}?", self.cfg.name_back);
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
        let threshold = ctx.speak_threshold;
        for text in [&ctx.trigger.body, &ctx.note] {
            if let Some(caps) = SCORE_MARKER.captures(text)
                && let Ok(score) = caps[1].parse::<u8>()
            {
                return Judgement::scored(
                    score,
                    threshold,
                    format!("{} said so", &caps[0]),
                    urgency,
                );
            }
        }
        if ctx.trigger.body.contains(SPEAK_MARKER) || ctx.note.contains(SPEAK_MARKER) {
            return Judgement::scored(
                SPEAK_SCORE,
                threshold,
                format!("the trigger carries {SPEAK_MARKER}"),
                urgency,
            );
        }
        Judgement::scored(
            self.cfg.score,
            threshold,
            format!(
                "no marker in the trigger, so the configured score ({})",
                self.cfg.score
            ),
            urgency,
        )
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
            speak_threshold: 5,
            participants: 0,
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
            ..EchoBrainConfig::default()
        });
        assert_eq!(
            brain.reply(&context("hi")).await.as_deref(),
            Some("echo: hi @bot-b:example.com and you?")
        );
    }

    #[tokio::test]
    async fn name_back_is_a_vocative_and_carries_no_user_id() {
        // The typed form: what another agent's MODEL can produce, and what
        // arrives with no `m.mentions` anywhere.
        let brain = EchoBrain::new(EchoBrainConfig {
            name_back: "bot-b".to_owned(),
            ..EchoBrainConfig::default()
        });
        let text = brain.reply(&context("hi")).await.expect("a reply");
        assert_eq!(text, "echo: hi, bot-b?");
        assert!(
            crate::events::mentioned_user_ids(&text).is_empty(),
            "a typed name must not become a mention"
        );
        let names = crate::addressing::Names::new(&["bot-b".to_owned()], Vec::new());
        assert!(
            names.addresses_me(&text, false).is_some(),
            "the other agent has to read it as an address"
        );
    }

    #[tokio::test]
    async fn the_markers_never_reach_the_room() {
        // One hop is all a marker gets: an echo that carried it would answer
        // for every agent downstream of it as well.
        let brain = EchoBrain::default();
        assert_eq!(
            brain
                .reply(&context("[[speak]] what is the state of the build?"))
                .await
                .as_deref(),
            Some("echo: what is the state of the build?")
        );
        assert_eq!(
            brain
                .reply(&context("the deploy [[score: 4]] is done"))
                .await
                .as_deref(),
            Some("echo: the deploy is done")
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
    async fn the_judge_scores_zero_unless_the_trigger_asks_for_more() {
        let brain = EchoBrain::default();
        let quiet = brain.judge(&context("just talking")).await;
        assert_eq!(quiet.score, 0);
        assert!(!quiet.speak);
        let keen = brain.judge(&context("[[speak]] now")).await;
        assert_eq!(keen.score, SPEAK_SCORE);
        assert!(keen.speak);
    }

    #[tokio::test]
    async fn a_scored_marker_answers_either_side_of_the_threshold() {
        // What makes a threshold gate possible without a model: the same brain
        // answering 4 and 6 to two lines, and the config deciding which speaks.
        let brain = EchoBrain::default();
        let four = brain.judge(&context("[[score: 4]] hm")).await;
        assert_eq!(four.score, 4);
        assert!(!four.speak, "4 is under the shipped threshold of 5");
        let six = brain.judge(&context("[[score: 6]] hm")).await;
        assert_eq!(six.score, 6);
        assert!(six.speak);
        // And the threshold is the connector's, not the brain's.
        let mut chatty = context("[[score: 4]] hm");
        chatty.speak_threshold = 3;
        assert!(brain.judge(&chatty).await.speak);
    }

    #[tokio::test]
    async fn a_configured_score_is_what_an_unmarked_line_gets() {
        let brain = EchoBrain::new(EchoBrainConfig {
            score: 9,
            ..EchoBrainConfig::default()
        });
        let judged = brain.judge(&context("nothing in particular")).await;
        assert_eq!(judged.score, 9);
        assert!(judged.speak);
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
            !without_markers("[git] merged PR #5").is_empty(),
            "an impulse's own square brackets are not a marker"
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
