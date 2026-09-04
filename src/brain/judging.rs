//! The judge: one cheap call that answers "how much would I add here?".
//!
//! Shared by every adapter that has a model behind it, so the question, the
//! frame and the parsing cannot drift between brains. The contract with the
//! model is deliberately tiny:
//!
//! ```text
//! score: 7 - nobody has answered the deploy question
//! ```
//!
//! and **anything else scores 0**. A judge that is asked for one line and
//! answers with a paragraph has not answered; reading that as enthusiasm is how
//! a room fills up with agents explaining why they are about to speak. Silence
//! is the safe failure everywhere in this project, and it is the safe failure
//! here.
//!
//! # Why a score and not a verdict
//!
//! The old contract asked `yes:`/`no:` - "would you add something here that
//! nobody has said?" - and the first real room answered it exactly as a careful
//! model should: *"no: the conversation has naturally settled"*, to a human who
//! had just said "you should just talk amongst yourselves". A binary question
//! about whether to speak is biased to silence, because silence is always
//! defensible.
//!
//! So the judge is asked for ENTHUSIASM, 0-9, after Webb's multiplayer
//! turn-taking (2025): directly addressed is a 9 - which never reaches this
//! call, because being addressed is tier 1 - so what the judge sees is only the
//! self-selection cases, and its scale is the middle of his:
//!
//! | score | what it means |
//! |---|---|
//! | 9-7 | I clearly should: invited, asked, or squarely my subject |
//! | 6-4 | I could add something |
//! | 3-0 | nothing to add, the thread is closed, or it is somebody else's |
//!
//! Where the line between speaking and not falls is the CONNECTOR's business
//! (`policy.speak_threshold`, shifted by `policy.chattiness`), so two agents
//! with the same brain can be differently talkative without either of them
//! being asked a different question.
//!
//! # What the judge is told
//!
//! Everything deterministic that a model would otherwise have to infer out of
//! prose, and reliably infers wrong: whether the line is a question, whether it
//! was thrown at the ROOM ("you two", "amongst yourselves", "anyone"), how many
//! people are here, whether I am already part of this exchange, and whether the
//! sender is another agent. None of it decides anything - it is what the room
//! looks like, stated rather than guessed.
//!
//! With inner thoughts on, the same call answers one more question - how much
//! it wanted to say something, 0 to 3 - by ending the line with `| urgency N`.
//! A mangled suffix costs the urgency, never the score.

use std::sync::LazyLock;

use regex::Regex;

use crate::addressing::addresses_room;
use crate::brain::rendering::render_conversation;
use crate::brain::{BrainContext, Judgement, MAX_URGENCY, Occasion};

/// Room lines the judge sees. The design says "the last ~20 room events"; more
/// than that is paying for context to answer one question about one line.
pub const JUDGE_HISTORY: usize = 20;

/// The one instruction every occasion ends on. Written out in full for each of
/// them below rather than appended, because a model follows the last thing it
/// read and the scale has to be the last thing it read.
const SCALE: &str = "Answer with exactly one line: `score: N` where N is 0-9 for how much you would add \
     here, then a dash and a short reason. 9-7 you clearly should (you were invited or asked, or \
     it is squarely your subject), 6-4 you could add something, 3-0 you have nothing to add, the \
     thread is closed, or it is somebody else's exchange.";

pub const QUESTION_UNADDRESSED: &str = "Nobody addressed you by name. Given the room below, how much would you add by speaking \
     now?";
pub const QUESTION_HEARTBEAT: &str = "The room has been quiet and nobody addressed you. How much would you add by bringing \
     something up unprompted? Usually little.";
pub const QUESTION_IMPULSE: &str = "Nobody addressed you. Something happened to you: {note}. Given what this room was talking \
     about, how much would these people gain by hearing it, right now, unprompted? Usually \
     little - score it high only if they would want to know.";
pub const QUESTION_FOLLOWUP: &str = "You left this open earlier and nobody came back to it: {note}. How much would you add by \
     following it up yourself now, or has the room moved on?";

/// Appended to the question when the connector is accumulating inner thoughts.
const URGENCY_INSTRUCTION: &str = " Then, on the same line, add ` | urgency N` where N is 0-{max} for how much you wanted to \
     say something here, whatever you scored: 0 nothing at all, 3 you are itching to speak.";

const FRAME: &str = "You are {me} in the Matrix room {room}, deciding whether to speak at all. Do \
     not write the message itself and do not greet anyone. People talk to each other here, so \
     joining in is normal - but say nothing rather than say what has already been said.";

/// `| urgency 2` at the end of a reason, and nowhere else: a reason that
/// happens to contain the word urgency is still a reason.
static URGENCY_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\|\s*urgency\s+(\d+)\s*$").expect("the urgency pattern is a literal")
});

/// `score: 7 - nobody has answered yet`, and nothing else.
///
/// The digit is single, and what follows it may not be another digit or a
/// decimal point: `score: 10` and `score: 7.5` are not on the scale, so they
/// are not scores, and a model that answers off the scale has not answered the
/// question it was asked. Rust's regex has no lookaround, so that boundary is
/// CONSUMED - which is why the reason is captured with its separator still on
/// and stripped below rather than in the pattern.
static SCORE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^score:[ \t]*(\d)([^\d.].*|)$").expect("the score pattern is a literal")
});

/// What may stand between the score and its reason.
const SEPARATORS: [char; 7] = ['-', ':', ',', '.', ' ', '\t', '\u{2013}'];

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
    let mut question = format!("{}\n\n{SCALE}", template.replace("{note}", note));
    if ctx.want_urgency {
        question.push_str(&URGENCY_INSTRUCTION.replace("{max}", &MAX_URGENCY.to_string()));
    }
    question
}

/// What is true about this room and this line, free to read and stated rather
/// than guessed.
///
/// Only for the occasions where the room's last line is what the judge is being
/// asked about. An impulse and a heartbeat are about something that happened to
/// the agent, and telling it "the last line was a question" would be describing
/// a line nobody is answering - except for the room's size, which is true
/// whatever the occasion.
#[must_use]
pub fn judge_cues(ctx: &BrainContext) -> String {
    let mut lines: Vec<String> = Vec::new();
    if ctx.participants > 0 {
        lines.push(format!(
            "- people and agents in this room: {}",
            ctx.participants
        ));
    }
    if ctx.occasion == Occasion::Unaddressed {
        let last = &ctx.trigger;
        lines.push(format!(
            "- the last line is {}",
            if last.body.contains('?') {
                "a question"
            } else {
                "not a question"
            }
        ));
        if addresses_room(&last.body) {
            lines.push(
                "- it is addressed to the room rather than to one person, so it is an \
                 invitation to whoever has something to say"
                    .to_owned(),
            );
        }
        lines.push(format!(
            "- it was written by {}",
            if last.is_bot {
                "another agent"
            } else {
                "a person"
            }
        ));
        lines.push(format!(
            "- you {} part in this exchange",
            if ctx.i_took_part() {
                "have already taken"
            } else {
                "have taken no"
            }
        ));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("What is true right now:\n{}", lines.join("\n"))
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

/// The room as the judge sees it, then what is true about it, ending on the
/// question.
#[must_use]
pub fn judge_prompt(ctx: &BrainContext) -> String {
    let conversation = render_conversation(ctx, Some(JUDGE_HISTORY));
    let cues = judge_cues(ctx);
    let question = judge_question(ctx);
    if cues.is_empty() {
        format!("{conversation}\n\n{question}")
    } else {
        format!("{conversation}\n\n{cues}\n\n{question}")
    }
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

/// Read a `score: N` line. Anything else scores 0, and says so.
///
/// `threshold` is what turns the score into a verdict: it comes from the
/// operator's configuration, never from the model.
#[must_use]
pub fn parse_judgement(text: Option<&str>, threshold: u8) -> Judgement {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Judgement::no("the judge answered nothing");
    };
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let Some(caps) = SCORE_LINE.captures(first) else {
        let head: String = first.chars().take(120).collect();
        return Judgement::no(format!("unscored judgement, taken as 0: {head:?}"));
    };
    let Ok(score) = caps[1].parse::<u8>() else {
        let head: String = first.chars().take(120).collect();
        return Judgement::no(format!("unscored judgement, taken as 0: {head:?}"));
    };
    let (reason, urgency) = split_urgency(caps[2].trim_start_matches(SEPARATORS).trim());
    let reason = if reason.is_empty() {
        "no reason given".to_owned()
    } else {
        reason
    };
    Judgement::scored(score, threshold, reason, urgency)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::MAX_JUDGE_SCORE;

    /// The shipped `policy.speak_threshold`. Written out rather than read from
    /// the config, so a change to the default cannot quietly rewrite what these
    /// rows assert.
    const THRESHOLD: u8 = 5;

    fn context(body: &str) -> BrainContext {
        BrainContext {
            persona: String::new(),
            me: "@bot-a:example.com".to_owned(),
            room_id: "!room:example.com".to_owned(),
            trigger: crate::events::RoomEvent {
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
                mentions: std::collections::BTreeSet::new(),
                is_bot: false,
            },
            history: Vec::new(),
            thread: Vec::new(),
            occasion: Occasion::Unaddressed,
            note: String::new(),
            want_urgency: false,
            speak_threshold: THRESHOLD,
            participants: 0,
        }
    }

    #[test]
    fn a_scored_line_is_read_as_written() {
        let keen = parse_judgement(
            Some("score: 7 - nobody has answered the deploy question"),
            THRESHOLD,
        );
        assert!(keen.speak);
        assert_eq!(keen.score, 7);
        assert_eq!(keen.why, "nobody has answered the deploy question");

        let quiet = parse_judgement(Some("score: 3 - they have it covered"), THRESHOLD);
        assert!(!quiet.speak);
        assert_eq!(quiet.score, 3);
        assert_eq!(quiet.why, "they have it covered");
    }

    #[test]
    fn the_score_is_case_insensitive_and_takes_the_first_real_line() {
        assert_eq!(
            parse_judgement(Some("SCORE: 8 I know the port"), 5).score,
            8
        );
        assert_eq!(
            parse_judgement(Some("\n\n  score: 2: nothing to add\nmore prose"), 5).score,
            2
        );
        // The separator is optional and may be any of the ones a model reaches
        // for; none of them ends up in the reason.
        for line in [
            "score: 6 - it is my subject",
            "score: 6 it is my subject",
            "score: 6, it is my subject",
            "score:6 - it is my subject",
        ] {
            let judged = parse_judgement(Some(line), 5);
            assert_eq!(judged.score, 6, "{line:?}");
            assert_eq!(judged.why, "it is my subject", "{line:?}");
        }
    }

    #[test]
    fn anything_that_is_not_a_score_is_a_zero() {
        // Out of range, off the scale, the old contract, and prose.
        for text in [
            "score: 10 - very keen",
            "score: 12",
            "score: -1 - no",
            "score: seven - I know this",
            "yes: I should speak",
            "no: they have it covered",
            "**score: 9** with markdown",
            "I think I should speak",
            "score",
            "",
        ] {
            let judgement = parse_judgement(Some(text), THRESHOLD);
            assert_eq!(judgement.score, 0, "{text:?} scored something");
            assert!(!judgement.speak, "{text:?} was taken as a yes");
        }
        let nothing = parse_judgement(None, THRESHOLD);
        assert!(!nothing.speak);
        assert_eq!(nothing.why, "the judge answered nothing");
        assert!(
            parse_judgement(Some("maybe"), THRESHOLD)
                .why
                .contains("unscored"),
            "the reason has to say what happened"
        );
    }

    #[test]
    fn an_empty_reason_still_says_something() {
        assert_eq!(
            parse_judgement(Some("score: 7"), THRESHOLD).why,
            "no reason given"
        );
        assert_eq!(
            parse_judgement(Some("score: 0 -  "), THRESHOLD).why,
            "no reason given"
        );
    }

    #[test]
    fn the_threshold_is_what_turns_a_score_into_a_verdict() {
        // The same answer, three agents. Nothing about the judge changed.
        let answer = Some("score: 5 - I could add the port number");
        assert!(parse_judgement(answer, 5).speak, "at the threshold, yes");
        assert!(!parse_judgement(answer, 6).speak, "a quieter agent, no");
        assert!(parse_judgement(answer, 4).speak, "a chattier agent, yes");
        // MAX_JUDGE_SCORE + 1 is "never speaks unprompted": no score reaches it.
        assert!(!parse_judgement(Some("score: 9 - I must"), MAX_JUDGE_SCORE + 1).speak);
        // 0 is "speaks whenever it is asked".
        assert!(parse_judgement(Some("score: 0 - nothing at all"), 0).speak);
    }

    #[test]
    fn the_log_line_says_the_score_and_what_it_was_measured_against() {
        assert_eq!(
            parse_judgement(Some("score: 7 - go on"), 5).says(5),
            "7 (>= 5)"
        );
        assert_eq!(
            parse_judgement(Some("score: 3 - leave it"), 5).says(5),
            "3 (< 5)"
        );
    }

    #[test]
    fn the_urgency_suffix_is_peeled_off_the_reason() {
        let judged = parse_judgement(Some("score: 3 - they have it covered | urgency 2"), 5);
        assert!(!judged.speak);
        assert_eq!(judged.why, "they have it covered");
        assert_eq!(judged.urgency, 2);
    }

    #[test]
    fn a_mangled_urgency_costs_the_urgency_and_never_the_score() {
        let out_of_range = parse_judgement(Some("score: 7 - say it | urgency 9"), 5);
        assert!(out_of_range.speak);
        assert_eq!(out_of_range.score, 7);
        assert_eq!(out_of_range.urgency, 0);
        assert_eq!(out_of_range.why, "say it | urgency 9");

        let prose = parse_judgement(Some("score: 2 - this is about urgency in general"), 5);
        assert_eq!(prose.urgency, 0);
        assert_eq!(prose.why, "this is about urgency in general");
    }

    #[test]
    fn the_urgency_instruction_is_only_asked_for_when_it_is_wanted() {
        let mut ctx = context("hello");
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

    #[test]
    fn every_question_ends_on_the_same_scale() {
        // Four occasions, one contract. A judge asked a different question per
        // occasion but scored on one scale is what keeps the threshold meaning
        // the same thing everywhere.
        let mut ctx = context("anyone around?");
        for occasion in [
            Occasion::Unaddressed,
            Occasion::Heartbeat,
            Occasion::Impulse,
            Occasion::Followup,
        ] {
            ctx.occasion = occasion;
            let question = judge_question(&ctx);
            assert!(
                question.contains("`score: N`") && question.contains("0-9"),
                "{occasion:?} asks for something else"
            );
        }
    }

    #[test]
    fn the_judge_is_told_what_is_free_to_know_about_the_room() {
        let mut ctx = context("you two, talk amongst yourselves about the weather");
        ctx.participants = 3;
        let cues = judge_cues(&ctx);
        assert!(cues.contains("people and agents in this room: 3"));
        assert!(cues.contains("not a question"));
        assert!(cues.contains("addressed to the room"));
        assert!(cues.contains("written by a person"));
        assert!(cues.contains("have taken no part"));

        // A question from another agent, in an exchange I am already in.
        ctx.trigger.body = "so what did the deploy say?".to_owned();
        ctx.trigger.is_bot = true;
        ctx.history = vec![ctx.trigger.clone(), {
            let mut mine = ctx.trigger.clone();
            mine.sender = ctx.me.clone();
            mine
        }];
        let cues = judge_cues(&ctx);
        assert!(cues.contains("the last line is a question"));
        assert!(!cues.contains("addressed to the room"));
        assert!(cues.contains("written by another agent"));
        assert!(cues.contains("have already taken part"));

        // The prompt carries them, and the room, and the question.
        let prompt = judge_prompt(&ctx);
        assert!(prompt.contains("What is true right now:"));
        assert!(prompt.contains("`score: N`"));
    }

    #[test]
    fn an_unprompted_occasion_is_told_the_room_size_and_nothing_about_the_last_line() {
        // The trigger of an impulse is an ANCHOR - where the conversation
        // stands - and describing it as if somebody were waiting on it would be
        // describing a line nobody is answering.
        let mut ctx = context("does anyone know why the build is red?");
        ctx.participants = 12;
        ctx.occasion = Occasion::Impulse;
        ctx.note = "[git] merged PR #5".to_owned();
        let cues = judge_cues(&ctx);
        assert_eq!(
            cues,
            "What is true right now:\n- people and agents in this room: 12"
        );

        // And before the member list has arrived there is nothing to say.
        ctx.participants = 0;
        assert!(judge_cues(&ctx).is_empty());
        assert!(!judge_prompt(&ctx).contains("What is true right now"));
    }
}
