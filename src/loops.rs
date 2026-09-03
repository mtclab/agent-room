//! Open loops: the things I said I would come back to.
//!
//! The second reason people speak unprompted. You asked a question nobody
//! answered, or you said you would check something - and later, if the person is
//! still around, you come back to it. Nothing in the room triggers that; the
//! reason is inside the agent, in what it said an hour ago.
//!
//! Two ways a loop opens, and they are deliberately different:
//!
//! - **A question.** My message ends with `?`. That is a loop whether I meant it
//!   or not, and no brain has to do anything to get it.
//! - **A promise.** The brain writes `[[followup: check the deploy log]]`
//!   anywhere in its message. The marker is STRIPPED before the message is
//!   posted - the room never sees it - and the text inside it is what the
//!   follow-up is about. It is the only piece of metadata a brain can send the
//!   connector, and it is documented in `docs/BRAIN_CONTRACT.md`.
//!
//! A loop closes when anyone else posts in that thread: they came back to it, so
//! I do not have to. It gets at most ONE follow-up, ever, whatever the judge
//! says - an agent that asks twice is nagging, and an agent that asks a third
//! time is a cron job. The loops themselves live in the ledger, because they
//! are persisted state and a restart must not lose a promise.

use std::sync::LazyLock;

use regex::Regex;

/// How much of a question is kept as the loop's text.
pub const MAX_LOOP_TEXT: usize = 300;

/// The states a loop can be in, as they are written to the ledger.
pub const STATE_OPEN: &str = "open";
pub const STATE_RAISED: &str = "raised";
pub const STATE_CLOSED: &str = "closed";

/// `[[followup: ...]]` anywhere in a brain's message. Non-greedy and
/// dot-matches-newline so a marker can span lines and two markers do not merge
/// into one.
static FOLLOWUP_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\[\[\s*followup\s*:\s*(.*?)\s*\]\]")
        .expect("the followup pattern is a literal")
});
static TRAILING_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+\n").expect("a literal"));
static BLANK_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("a literal"));

/// Split a brain's message into `(what to post, what to follow up)`.
///
/// Every marker is removed from the posted text - a room must never see one -
/// and the first non-empty marker body is the follow-up. An empty marker
/// (`[[followup:]]`) is removed and means nothing, because a promise with no
/// content is not a promise.
#[must_use]
pub fn extract_followup(text: &str) -> (String, String) {
    let followup = FOLLOWUP_MARKER
        .captures_iter(text)
        .map(|caps| caps[1].trim().to_owned())
        .find(|body| !body.is_empty())
        .unwrap_or_default();
    let cleaned = FOLLOWUP_MARKER.replace_all(text, "");
    // Markers usually sit on their own line; collapse what removing one leaves.
    let cleaned = TRAILING_SPACE.replace_all(&cleaned, "\n");
    let cleaned = BLANK_RUN.replace_all(&cleaned, "\n\n");
    (cleaned.trim().to_owned(), followup)
}

/// Whether this message leaves a question hanging.
#[must_use]
pub fn is_open_question(text: &str) -> bool {
    text.trim_end().ends_with('?')
}

/// What the follow-up will be about: the promise, or the question I asked.
#[must_use]
pub fn loop_text(body: &str, followup: &str) -> String {
    let text = if followup.trim().is_empty() {
        body.trim()
    } else {
        followup.trim()
    };
    text.chars().take(MAX_LOOP_TEXT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_without_a_marker_is_untouched() {
        assert_eq!(
            extract_followup("just an answer"),
            ("just an answer".to_owned(), String::new())
        );
    }

    #[test]
    fn the_marker_never_reaches_the_room() {
        let (text, followup) =
            extract_followup("I will look.\n\n[[followup: check the deploy log]]");
        assert_eq!(text, "I will look.");
        assert_eq!(followup, "check the deploy log");
        assert!(!text.contains("followup"));
    }

    #[test]
    fn an_empty_marker_is_removed_and_promises_nothing() {
        let (text, followup) = extract_followup("done [[followup:]]");
        assert_eq!(text, "done");
        assert_eq!(followup, String::new());
    }

    #[test]
    fn two_markers_do_not_merge_and_the_first_real_one_wins() {
        let (text, followup) = extract_followup("a [[followup:]] b [[followup: the second]] c");
        assert_eq!(followup, "the second");
        assert_eq!(text, "a  b  c");
    }

    #[test]
    fn a_marker_may_span_lines_and_is_case_insensitive() {
        let (text, followup) = extract_followup("hi [[FollowUp:\n  the thing\n]] there");
        assert_eq!(followup, "the thing");
        assert_eq!(text, "hi  there");
    }

    #[test]
    fn a_message_that_is_only_a_marker_leaves_nothing_to_post() {
        let (text, followup) = extract_followup("[[followup: check it]]");
        assert!(text.is_empty());
        assert_eq!(followup, "check it");
    }

    #[test]
    fn a_question_is_what_opens_a_loop_without_anybody_asking_for_one() {
        assert!(is_open_question("did anyone try it?"));
        assert!(is_open_question("did anyone try it?  \n"));
        assert!(!is_open_question("nobody tried it."));
        assert!(!is_open_question("a question mark? in the middle"));
    }

    #[test]
    fn the_promise_wins_over_the_question_and_is_kept_short() {
        assert_eq!(loop_text("did it work?", "check the log"), "check the log");
        assert_eq!(loop_text("did it work?", "   "), "did it work?");
        let long = "x".repeat(MAX_LOOP_TEXT + 50);
        assert_eq!(loop_text(&long, "").len(), MAX_LOOP_TEXT);
    }
}
