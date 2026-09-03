//! The judge: one cheap call that answers "should I speak here?".
//!
//! Shared by every adapter that has a model behind it, so the question, the
//! frame and the parsing cannot drift between brains. The contract with the
//! model is deliberately tiny:
//!
//! ```text
//! yes: <one line why>
//! no: <one line why>
//! ```
//!
//! and **anything else is a no**. A judge that is asked to be terse and answers
//! with a paragraph has not answered; treating that as a yes is how a room
//! fills up with agents explaining why they are about to speak. Silence is the
//! safe failure everywhere in this project, and it is the safe failure here.
//!
//! With inner thoughts on, the same call answers one more question - how much
//! it wanted to say something, 0 to 3 - by ending the line with `| urgency N`.
//! A mangled suffix costs the urgency, never the verdict.

use std::sync::LazyLock;

use regex::Regex;

use crate::brain::rendering::render_conversation;
use crate::brain::{BrainContext, Judgement, MAX_URGENCY, Occasion};

/// Room lines the judge sees. The design says "the last ~20 room events"; more
/// than that is paying for context to answer a yes/no question.
pub const JUDGE_HISTORY: usize = 20;

pub const QUESTION_UNADDRESSED: &str = "Nobody addressed you. Would you, as this participant, add something here that nobody has \
     said? Answer exactly `yes: <one line>` or `no: <one line>`.";
pub const QUESTION_HEARTBEAT: &str = "The room has been quiet and nobody addressed you. Anything worth bringing up unprompted? \
     Usually no. Answer exactly `yes: <one line>` or `no: <one line>`.";
pub const QUESTION_IMPULSE: &str = "Nobody addressed you. Something happened to you: {note}. Given what this room was talking \
     about, is that worth telling them, right now, unprompted? Usually no - say yes only if \
     these people would want to know. Answer exactly `yes: <one line>` or `no: <one line>`.";
pub const QUESTION_FOLLOWUP: &str = "You left this open earlier and nobody came back to it: {note}. Is it worth following it up \
     yourself now, or has the room moved on? Answer exactly `yes: <one line>` or \
     `no: <one line>`.";

/// Appended to the question when the connector is accumulating inner thoughts.
const URGENCY_INSTRUCTION: &str = " Then, on the same line, add ` | urgency N` where N is 0-{max} for how much you wanted to \
     say something here, whatever your answer was: 0 nothing at all, 3 you are itching to speak.";

const FRAME: &str = "You are {me} in the Matrix room {room}, deciding whether to speak at all. Do \
     not write the message itself and do not greet anyone. Say yes only if you would add \
     something the room does not already have. Answer on ONE line, exactly `yes: <reason>` or \
     `no: <reason>`.";

/// `| urgency 2` at the end of a verdict line, and nowhere else: a reason that
/// happens to contain the word urgency is still a reason.
static URGENCY_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\|\s*urgency\s+(\d+)\s*$").expect("the urgency pattern is a literal")
});

/// The question this occasion asks.
#[must_use]
pub fn judge_question(ctx: &BrainContext) -> String {
    let template = match ctx.occasion {
        Occasion::Heartbeat => QUESTION_HEARTBEAT,
        Occasion::Impulse => QUESTION_IMPULSE,
        Occasion::Followup => QUESTION_FOLLOWUP,
        _ => QUESTION_UNADDRESSED,
    };
    let note = ctx.note.trim();
    let note = if note.is_empty() {
        "something you noticed"
    } else {
        note
    };
    let mut question = template.replace("{note}", note);
    if ctx.want_urgency {
        question.push_str(&URGENCY_INSTRUCTION.replace("{max}", &MAX_URGENCY.to_string()));
    }
    question
}

/// Persona plus the judging frame, as one system prompt.
#[must_use]
pub fn judge_system_prompt(ctx: &BrainContext) -> String {
    let frame = FRAME
        .replace("{me}", &ctx.me)
        .replace("{room}", &ctx.room_id);
    let persona = ctx.persona.trim();
    if persona.is_empty() {
        frame
    } else {
        format!("{persona}\n\n{frame}")
    }
}

/// The room as the judge sees it, ending on the question.
#[must_use]
pub fn judge_prompt(ctx: &BrainContext) -> String {
    let conversation = render_conversation(ctx, Some(JUDGE_HISTORY));
    format!("{conversation}\n\n{}", judge_question(ctx))
}

/// Peel `| urgency N` off a reason. Anything else leaves urgency at 0.
///
/// Strict on the number and forgiving about the rest: an N outside 0-3 is not
/// an urgency, and a model that wrote something else entirely still gets its
/// reason back untouched rather than half a sentence.
#[must_use]
pub fn split_urgency(reason: &str) -> (String, i32) {
    let Some(caps) = URGENCY_SUFFIX.captures(reason) else {
        return (reason.to_owned(), 0);
    };
    let Ok(value) = caps[1].parse::<i32>() else {
        return (reason.to_owned(), 0);
    };
    if value > MAX_URGENCY {
        return (reason.to_owned(), 0);
    }
    let start = caps.get(0).map_or(reason.len(), |m| m.start());
    (reason[..start].trim().to_owned(), value)
}

/// Read a `yes:`/`no:` verdict. Anything else is a no, and says so.
#[must_use]
pub fn parse_judgement(text: Option<&str>) -> Judgement {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Judgement::no("the judge answered nothing");
    };
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let low = first.to_lowercase();
    if low.starts_with("yes:") {
        let (reason, urgency) = split_urgency(first[4..].trim());
        let reason = if reason.is_empty() {
            "no reason given".to_owned()
        } else {
            reason
        };
        return Judgement::new(true, reason, urgency);
    }
    if low.starts_with("no:") {
        let (reason, urgency) = split_urgency(first[3..].trim());
        let reason = if reason.is_empty() {
            "no reason given".to_owned()
        } else {
            reason
        };
        return Judgement::new(false, reason, urgency);
    }
    let head: String = first.chars().take(120).collect();
    Judgement::no(format!("unparseable judgement, taken as no: {head:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_verdict_is_read_as_written() {
        let yes = parse_judgement(Some("yes: nobody has answered the deploy question"));
        assert!(yes.speak);
        assert_eq!(yes.why, "nobody has answered the deploy question");
        let no = parse_judgement(Some("no: they have it covered"));
        assert!(!no.speak);
        assert_eq!(no.why, "they have it covered");
    }

    #[test]
    fn the_verdict_is_case_insensitive_and_takes_the_first_real_line() {
        assert!(parse_judgement(Some("YES: I know the port")).speak);
        assert!(!parse_judgement(Some("\n\n  no: nothing to add\nmore prose")).speak);
    }

    #[test]
    fn anything_that_is_not_a_verdict_is_a_no() {
        for text in [
            "maybe",
            "**yes:** with markdown",
            "I think I should speak",
            "yes",
            "",
        ] {
            let judgement = parse_judgement(Some(text));
            assert!(!judgement.speak, "{text:?} was taken as a yes");
        }
        let nothing = parse_judgement(None);
        assert!(!nothing.speak);
        assert_eq!(nothing.why, "the judge answered nothing");
        assert!(
            parse_judgement(Some("maybe")).why.contains("unparseable"),
            "the reason has to say what happened"
        );
    }

    #[test]
    fn an_empty_reason_still_says_something() {
        assert_eq!(parse_judgement(Some("yes:")).why, "no reason given");
        assert_eq!(parse_judgement(Some("no:  ")).why, "no reason given");
    }

    #[test]
    fn the_urgency_suffix_is_peeled_off_the_reason() {
        let judged = parse_judgement(Some("no: they have it covered | urgency 2"));
        assert!(!judged.speak);
        assert_eq!(judged.why, "they have it covered");
        assert_eq!(judged.urgency, 2);
    }

    #[test]
    fn a_mangled_urgency_costs_the_urgency_and_never_the_verdict() {
        let out_of_range = parse_judgement(Some("yes: say it | urgency 9"));
        assert!(out_of_range.speak);
        assert_eq!(out_of_range.urgency, 0);
        assert_eq!(out_of_range.why, "yes: say it | urgency 9"[5..].to_owned());

        let prose = parse_judgement(Some("no: this is about urgency in general"));
        assert_eq!(prose.urgency, 0);
        assert_eq!(prose.why, "this is about urgency in general");
    }

    #[test]
    fn the_urgency_instruction_is_only_asked_for_when_it_is_wanted() {
        let mut ctx = BrainContext {
            persona: String::new(),
            me: "@bot-a:example.com".to_owned(),
            room_id: "!room:example.com".to_owned(),
            trigger: crate::events::RoomEvent {
                event_id: "$t".to_owned(),
                room_id: "!room:example.com".to_owned(),
                sender: "@human:example.com".to_owned(),
                sender_display: None,
                body: "hello".to_owned(),
                formatted_body: None,
                msgtype: "m.text".to_owned(),
                ts: 1.0,
                thread_root: None,
                reply_to: None,
                reply_is_fallback: false,
                mentions: std::collections::BTreeSet::new(),
                is_bot: false,
            },
            history: Vec::new(),
            thread: Vec::new(),
            occasion: Occasion::Unaddressed,
            note: String::new(),
            want_urgency: false,
        };
        assert!(!judge_question(&ctx).contains("urgency"));
        ctx.want_urgency = true;
        assert!(judge_question(&ctx).contains("| urgency N"));

        ctx.occasion = Occasion::Impulse;
        ctx.note = "[git] merged PR #5".to_owned();
        assert!(judge_question(&ctx).contains("[git] merged PR #5"));

        // The frame carries who and where, and the persona when there is one.
        assert!(judge_system_prompt(&ctx).contains("deciding whether to speak at all"));
        ctx.persona = "You are bot-a.".to_owned();
        assert!(judge_system_prompt(&ctx).starts_with("You are bot-a."));
    }
}
