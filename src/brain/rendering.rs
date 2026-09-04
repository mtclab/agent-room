//! How a room turn is rendered for a brain.
//!
//! Every adapter shows the model the same thing: the thread the trigger belongs
//! to (or the recent room history when it is not threaded), one line per
//! message, and one line saying why it is being asked at all. Kept here rather
//! than in one adapter so no two adapters can drift apart.

use crate::brain::{BrainContext, Occasion};
use crate::events::RoomEvent;

/// What a brain is told when the room has said nothing yet. An empty prompt is
/// not a prompt.
pub const EMPTY_ROOM: &str = "(nobody has said anything here yet)";

/// The one piece of metadata a brain may send back, and the only reason the
/// frame mentions it: without being told, no model would ever write it. Left
/// off a follow-up, where a new promise would be a lie.
pub const FOLLOWUP_HINT: &str = "If you say you will come back to something, you may end your \
     message with [[followup: the thing you will check]]: it is removed before the message is \
     posted, and you may be asked about it later, once. Use it rarely and only when you meant it.";

/// One line per occasion, telling the brain what it is being asked FOR. The
/// unprompted ones carry `ctx.note`: the thing that happened, the loop I left
/// open, the thought that kept coming back - because none of that is in the
/// room.
fn task_template(occasion: Occasion) -> &'static str {
    match occasion {
        Occasion::Reply => "Answer the last line.",
        Occasion::Unaddressed => {
            "Nobody addressed you. Say the one thing that is worth adding, or nothing at all."
        }
        Occasion::Heartbeat => {
            "Nobody asked you anything: say the one thing you wanted to bring up, in one short \
             message."
        }
        Occasion::Impulse => {
            "Nobody asked you anything. Something happened to you: {note}. Mention it in one \
             short message, in your own words, only the part that matters to this room."
        }
        Occasion::Followup => {
            "You left this open earlier and nobody came back to it: {note}. Come back to it \
             yourself, in one short message."
        }
        Occasion::InnerThought => {
            "Nobody addressed you, but you have been following this and kept wanting to say \
             something: {note}. Say it in one short message."
        }
    }
}

/// The one line that says why this brain is being asked to speak.
#[must_use]
pub fn render_task(ctx: &BrainContext) -> String {
    let note = ctx.note.trim();
    let note = if note.is_empty() {
        "something you noticed"
    } else {
        note
    };
    let task = task_template(ctx.occasion).replace("{note}", note);
    if ctx.occasion == Occasion::Followup {
        task
    } else {
        format!("{task} {FOLLOWUP_HINT}")
    }
}

/// One transcript line: `<display or localpart>: <body>`.
#[must_use]
pub fn render_event(ev: &RoomEvent) -> String {
    format!("{}: {}", ev.display(), ev.body)
}

/// The rendered conversation, oldest first.
///
/// A threaded trigger is answered with its thread; anything else with the
/// recent room history. When the occasion is an ANSWER the trigger is appended
/// last even if the history already holds it, so the model can never be asked
/// to answer a middle line. When it is not, the trigger is only an anchor and
/// is left where it happened, because moving it to the end reads as "reply to
/// this" and that is exactly what the agent is not doing.
///
/// `limit` caps how many earlier lines come with it (the judge runs on a short
/// window to stay cheap); the trigger is always kept.
#[must_use]
pub fn render_conversation(ctx: &BrainContext, limit: Option<usize>) -> String {
    let source = if ctx.thread.is_empty() {
        &ctx.history
    } else {
        &ctx.thread
    };
    if !ctx.occasion.is_answering() {
        let mut lines: Vec<String> = source.iter().map(render_event).collect();
        if let Some(limit) = limit {
            let start = lines.len().saturating_sub(limit);
            lines = lines[start..].to_vec();
        }
        if lines.is_empty() {
            return EMPTY_ROOM.to_owned();
        }
        return lines.join("\n");
    }
    let mut lines: Vec<String> = source
        .iter()
        .filter(|ev| ev.event_id != ctx.trigger.event_id)
        .map(render_event)
        .collect();
    if let Some(limit) = limit {
        let start = lines.len().saturating_sub(limit.saturating_sub(1));
        lines = lines[start..].to_vec();
    }
    lines.push(render_event(&ctx.trigger));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn event(event_id: &str, body: &str, display: &str) -> RoomEvent {
        RoomEvent {
            event_id: event_id.to_owned(),
            room_id: "!room:example.com".to_owned(),
            sender: "@human:example.com".to_owned(),
            sender_display: Some(display.to_owned()),
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

    fn context(occasion: Occasion) -> BrainContext {
        let earlier = event("$earlier", "earlier line", "Alex");
        let trigger = event("$trigger", "ping", "Alex");
        BrainContext {
            persona: "You are bot-a.".to_owned(),
            me: "@bot-a:example.com".to_owned(),
            room_id: "!room:example.com".to_owned(),
            trigger: trigger.clone(),
            history: vec![earlier, trigger],
            thread: Vec::new(),
            occasion,
            note: String::new(),
            want_urgency: false,
            speak_threshold: 5,
            participants: 0,
        }
    }

    #[test]
    fn an_answer_puts_the_trigger_last_exactly_once() {
        let rendered = render_conversation(&context(Occasion::Reply), None);
        assert_eq!(rendered, "Alex: earlier line\nAlex: ping");
    }

    #[test]
    fn a_thread_replaces_the_room_history() {
        let mut ctx = context(Occasion::Reply);
        ctx.history = vec![event("$noise", "unrelated room chatter", "Alex")];
        ctx.thread = vec![event("$root", "thread start", "Alex"), ctx.trigger.clone()];
        let rendered = render_conversation(&ctx, None);
        assert_eq!(rendered, "Alex: thread start\nAlex: ping");
        assert!(!rendered.contains("unrelated"));
    }

    #[test]
    fn an_unprompted_occasion_leaves_the_anchor_where_it_happened() {
        let mut ctx = context(Occasion::Impulse);
        ctx.note = "[git] merged PR #5".to_owned();
        let rendered = render_conversation(&ctx, None);
        assert_eq!(rendered, "Alex: earlier line\nAlex: ping");
        let task = render_task(&ctx);
        assert!(
            task.contains("Something happened to you: [git] merged PR #5"),
            "{task}"
        );
        assert!(task.contains("Mention it in one short message"), "{task}");
    }

    #[test]
    fn an_empty_room_still_renders_something() {
        let mut ctx = context(Occasion::Impulse);
        ctx.history.clear();
        assert_eq!(render_conversation(&ctx, None), EMPTY_ROOM);
    }

    #[test]
    fn the_limit_keeps_the_trigger_and_trims_the_rest() {
        let mut ctx = context(Occasion::Unaddressed);
        ctx.history = (0..10)
            .map(|i| event(&format!("$e{i}"), &format!("line {i}"), "Alex"))
            .chain(std::iter::once(ctx.trigger.clone()))
            .collect();
        let rendered = render_conversation(&ctx, Some(3));
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "Alex: ping");
        assert_eq!(lines[0], "Alex: line 8");
    }

    #[test]
    fn the_frame_tells_the_brain_about_the_one_marker_it_may_send_back() {
        let task = render_task(&context(Occasion::Reply));
        assert!(task.contains("[[followup:"), "{task}");
        assert!(task.contains("Use it rarely"), "{task}");

        // Not on a follow-up: an agent that follows up on its follow-up is a
        // cron job with a persona, so inviting a promise there would be a lie.
        let mut followup = context(Occasion::Followup);
        followup.note = "check the log".to_owned();
        let task = render_task(&followup);
        assert!(!task.contains("[[followup:"), "{task}");
        assert!(task.contains("You left this open earlier"), "{task}");
    }

    #[test]
    fn a_display_name_wins_over_the_localpart() {
        let mut ev = event("$e", "hi", "Alex");
        assert_eq!(render_event(&ev), "Alex: hi");
        ev.sender_display = None;
        assert_eq!(render_event(&ev), "human: hi");
    }
}
