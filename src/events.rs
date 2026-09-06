//! Normalised room events.
//!
//! [`RoomEvent`] is the only event shape the rest of the connector knows about.
//! It is built from the raw JSON the homeserver sends by [`from_source`], a
//! pure function, and round-trips through JSON for the transcript - the same
//! JSON the Python implementation this was ported from wrote, so a state
//! directory either one left behind is one the other can pick up where
//! the other left off.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use percent_encoding::percent_decode_str;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Matches a matrix.to user pill in an HTML `formatted_body`. Clients write the
/// user id either plainly (`@user:server`) or percent-encoded
/// (`%40user%3Aserver`).
static PILL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)https://matrix\.to/#/((?:@|%40)[^"'\s<>?/]+)"#)
        .expect("the pill pattern is a literal and compiles")
});

/// A Matrix user id written in plain text, e.g. `@bot-b:example.com`. Used to
/// turn "@someone" in a reply into a real mention.
static USER_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@[A-Za-z0-9._=\-/+]+:[A-Za-z0-9.\-]+(?::\d{1,5})?")
        .expect("the user id pattern is a literal and compiles")
});

/// Sentence punctuation a user id can pick up at the end of a line. Server
/// names may contain dots, so the trailing ones have to be shaved off by hand.
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"', '<', '>',
];

pub const TEXT_MSGTYPE: &str = "m.text";
pub const NOTICE_MSGTYPE: &str = "m.notice";
pub const EMOTE_MSGTYPE: &str = "m.emote";
pub const THREAD_REL_TYPE: &str = "m.thread";
pub const REACTION_TYPE: &str = "m.reaction";
pub const ANNOTATION_REL_TYPE: &str = "m.annotation";
pub const REPLACE_REL_TYPE: &str = "m.replace";
pub const MESSAGE_TYPE: &str = "m.room.message";
pub const REDACTION_TYPE: &str = "m.room.redaction";

/// The msgtypes that are a line of conversation. Everything else a room
/// carries - an image, a file, a location - is not something somebody said.
pub const MESSAGE_MSGTYPES: [&str; 3] = [TEXT_MSGTYPE, NOTICE_MSGTYPE, EMOTE_MSGTYPE];

/// Whether this raw event is a line of conversation.
///
/// Everything else a room contains - membership, topic changes, reactions,
/// images, redacted husks - is not something anybody said, and a session
/// reading the room should not have to filter it out itself.
#[must_use]
pub fn is_message_source(source: &Value) -> bool {
    skip_reason(source).is_none()
}

/// Why this raw event is not a line of conversation, or `None` when it is one.
///
/// The other half of [`is_message_source`], and the reason there is only one
/// filter: the connector drops the same events the MCP server hides, and an
/// operator wondering where a picture went finds the answer in the log rather
/// than in this file. Nothing is allocated for an event that passes.
#[must_use]
pub fn skip_reason(source: &Value) -> Option<String> {
    let kind = source
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("no type");
    if kind != MESSAGE_TYPE {
        return Some(format!("{kind} is not a message"));
    }
    let Some(content) = source.get("content").filter(|value| value.is_object()) else {
        return Some("no content (redacted, or not an event at all)".to_owned());
    };
    let Some(msgtype) = content.get("msgtype").and_then(Value::as_str) else {
        return Some("no msgtype (a redacted husk, or state-shaped)".to_owned());
    };
    if !MESSAGE_MSGTYPES.contains(&msgtype) {
        return Some(format!("{msgtype} is not a line of conversation"));
    }
    let body = content.get("body").and_then(Value::as_str).unwrap_or("");
    if body.trim().is_empty() {
        return Some(format!("an empty {msgtype} body"));
    }
    None
}

/// A correction to something that is already in the transcript.
///
/// Neither one is a new line: they change what an OLD line says, or take it
/// away. They are read before the message filter, because an edit is a
/// perfectly ordinary `m.room.message` and a redaction is not a message at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Correction {
    /// An `m.replace`: the target's text becomes this one's.
    Edit(Edit),
    /// An `m.room.redaction`: the target's text goes away.
    Redaction(Redaction),
}

/// One `m.replace`, read as what it does rather than as what it looks like.
///
/// The event's own `body` is the `* corrected text` fallback that clients
/// without edit support show, and it is deliberately not here: the only text an
/// edit carries is `m.new_content`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The edit's own event id, for the consumed ledger.
    pub event_id: String,
    /// The event whose text this replaces.
    pub target: String,
    /// Who sent the edit. Only the original's own author may rewrite it.
    pub sender: String,
    pub body: String,
    pub formatted_body: Option<String>,
    pub mentions: BTreeSet<String>,
}

/// One `m.room.redaction`, and what it takes away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// The redaction's own event id, for the consumed ledger.
    pub event_id: String,
    /// The event whose content the homeserver has just thrown away.
    pub redacts: String,
    pub sender: String,
}

/// Read one raw event as a correction, or `None` when it is not one.
#[must_use]
pub fn correction_from(source: &Value) -> Option<Correction> {
    match source.get("type").and_then(Value::as_str) {
        Some(REDACTION_TYPE) => redaction_from(source).map(Correction::Redaction),
        Some(MESSAGE_TYPE) => edit_from(source).map(Correction::Edit),
        _ => None,
    }
}

/// The `m.replace` half: `m.relates_to` says what it replaces, `m.new_content`
/// says what with.
fn edit_from(source: &Value) -> Option<Edit> {
    let content = source.get("content").filter(|value| value.is_object())?;
    let relates_to = content.get("m.relates_to").and_then(Value::as_object)?;
    if relates_to.get("rel_type").and_then(Value::as_str) != Some(REPLACE_REL_TYPE) {
        return None;
    }
    let target = relates_to
        .get("event_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let new_content = content
        .get("m.new_content")
        .filter(|value| value.is_object())?;
    let body = new_content.get("body").and_then(Value::as_str)?;
    let formatted_body = new_content
        .get("formatted_body")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Some(Edit {
        event_id: event_id_of(source),
        target: target.to_owned(),
        sender: sender_of(source),
        body: body.to_owned(),
        mentions: parse_mentions(new_content, formatted_body.as_deref()),
        formatted_body,
    })
}

/// The redaction half. Room version 11 moved `redacts` into the content and
/// older rooms keep it at the top level, so both are read.
fn redaction_from(source: &Value) -> Option<Redaction> {
    let redacts = source
        .pointer("/content/redacts")
        .and_then(Value::as_str)
        .or_else(|| source.get("redacts").and_then(Value::as_str))
        .filter(|id| !id.is_empty())?;
    Some(Redaction {
        event_id: event_id_of(source),
        redacts: redacts.to_owned(),
        sender: sender_of(source),
    })
}

fn event_id_of(source: &Value) -> String {
    source
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn sender_of(source: &Value) -> String {
    source
        .get("sender")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// `@bot-a:example.com` -> `bot-a`.
#[must_use]
pub fn localpart(user_id: &str) -> &str {
    let head = user_id.split_once(':').map_or(user_id, |(head, _)| head);
    head.strip_prefix('@').unwrap_or(head)
}

/// User ids written out in a message body.
///
/// A brain returns text, not metadata, so "@bot-b:example.com what do you
/// think?" would otherwise reach the room without an `m.mentions` and no agent
/// would ever be addressed by another. Anything with `bot_to_bot: mentions`
/// would then be unreachable by design - the room could only ever answer
/// humans, which is the opposite of the point.
#[must_use]
pub fn mentioned_user_ids(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for m in USER_ID_RE.find_iter(text) {
        let candidate = m.as_str().trim_end_matches(TRAILING);
        if candidate.len() > 1 && candidate[1..].contains(':') {
            found.insert(candidate.to_owned());
        }
    }
    found
}

/// One normalised `m.room.message` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomEvent {
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    #[serde(default)]
    pub sender_display: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub formatted_body: Option<String>,
    #[serde(default = "default_msgtype")]
    pub msgtype: String,
    /// Epoch seconds (the homeserver's `origin_server_ts` / 1000).
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub thread_root: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// True when `m.in_reply_to` is only a thread fallback pointer, not a real
    /// reply.
    #[serde(default)]
    pub reply_is_fallback: bool,
    #[serde(default)]
    pub mentions: BTreeSet<String>,
    #[serde(default)]
    pub is_bot: bool,
}

fn default_msgtype() -> String {
    TEXT_MSGTYPE.to_owned()
}

impl RoomEvent {
    /// Best available name for rendering the event to a brain.
    #[must_use]
    pub fn display(&self) -> &str {
        match &self.sender_display {
            Some(name) if !name.is_empty() => name,
            _ => localpart(&self.sender),
        }
    }

    /// The thread this event belongs to, rooting a new one at itself.
    #[must_use]
    pub fn thread_root_or_self(&self) -> &str {
        self.thread_root.as_deref().unwrap_or(&self.event_id)
    }

    /// A genuine rich reply, not a thread fallback pointer.
    #[must_use]
    pub fn is_direct_reply(&self) -> bool {
        self.reply_to.is_some() && !self.reply_is_fallback
    }
}

/// User ids mentioned by an event.
///
/// MSC3952 `m.mentions.user_ids` is authoritative when present; otherwise fall
/// back to parsing matrix.to pills out of the HTML body (older clients).
#[must_use]
pub fn parse_mentions(content: &Value, formatted_body: Option<&str>) -> BTreeSet<String> {
    if let Some(intentional) = content.get("m.mentions").and_then(Value::as_object) {
        return match intentional.get("user_ids").and_then(Value::as_array) {
            Some(ids) => ids
                .iter()
                .filter_map(|id| id.as_str().map(ToOwned::to_owned))
                .collect(),
            None => BTreeSet::new(),
        };
    }
    parse_pills(formatted_body)
}

/// User ids linked as matrix.to pills in an HTML body.
#[must_use]
pub fn parse_pills(formatted_body: Option<&str>) -> BTreeSet<String> {
    let Some(html) = formatted_body else {
        return BTreeSet::new();
    };
    PILL_RE
        .captures_iter(html)
        .map(|caps| {
            percent_decode_str(&caps[1])
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect()
}

/// `(thread_root, reply_to, reply_is_fallback)` from `m.relates_to`.
#[must_use]
pub fn parse_relations(content: &Value) -> (Option<String>, Option<String>, bool) {
    let Some(relates_to) = content.get("m.relates_to").and_then(Value::as_object) else {
        return (None, None, false);
    };
    let thread_root = if relates_to.get("rel_type").and_then(Value::as_str) == Some(THREAD_REL_TYPE)
    {
        relates_to
            .get("event_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    } else {
        None
    };
    let reply_to = relates_to
        .get("m.in_reply_to")
        .and_then(Value::as_object)
        .and_then(|in_reply_to| in_reply_to.get("event_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    // A threaded message carries an in_reply_to fallback pointer that clients
    // must not render as a reply; only a real rich reply counts as one.
    let fallback = reply_to.is_some()
        && relates_to
            .get("is_falling_back")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    (thread_root, reply_to, fallback)
}

/// Whether this ACCOUNT is a bot, judged without an event to look at.
///
/// [`is_bot_sender`] can use the msgtype (`m.notice` is what every connector
/// posts with); a room's member list cannot, so the identity half is here and
/// both callers share it.
#[must_use]
pub fn is_bot_user(
    user_id: &str,
    bot_user_ids: &[String],
    bot_localpart_patterns: &[Regex],
) -> bool {
    if bot_user_ids.iter().any(|listed| listed == user_id) {
        return true;
    }
    let name = localpart(user_id);
    bot_localpart_patterns
        .iter()
        .any(|pattern| pattern.is_match(name))
}

/// Whether an event came from a bot.
///
/// `m.notice` is the convention every connector posts with, plus an explicit
/// allowlist and localpart patterns for bots that post `m.text`.
#[must_use]
pub fn is_bot_sender(
    sender: &str,
    msgtype: &str,
    bot_user_ids: &[String],
    bot_localpart_patterns: &[Regex],
) -> bool {
    if msgtype == NOTICE_MSGTYPE {
        return true;
    }
    is_bot_user(sender, bot_user_ids, bot_localpart_patterns)
}

/// How an event's sender is classified, and who its display name comes from.
#[derive(Debug, Clone, Copy)]
pub struct BotRules<'a> {
    pub bot_user_ids: &'a [String],
    pub bot_localpart_patterns: &'a [Regex],
}

/// Normalise one raw `m.room.message` event as the homeserver sends it.
///
/// This is the single normalisation path: a `/sync` timeline event (decrypted
/// by the SDK when the room is encrypted) and a `/messages` chunk entry are
/// the same room event, so both come through here.
///
/// `room_id` wins over the one in the event: the caller asked for a specific
/// room, and a sync timeline event does not carry one at all.
#[must_use]
pub fn from_source(
    source: &Value,
    room_id: &str,
    display_name: Option<String>,
    rules: BotRules<'_>,
) -> RoomEvent {
    static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(serde_json::Map::new()));
    let content = match source.get("content") {
        Some(value) if value.is_object() => value,
        _ => &EMPTY,
    };
    let msgtype = content
        .get("msgtype")
        .and_then(Value::as_str)
        .unwrap_or(TEXT_MSGTYPE)
        .to_owned();
    let formatted_body = content
        .get("formatted_body")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let (thread_root, reply_to, reply_is_fallback) = parse_relations(content);
    let sender = sender_of(source);
    let event_room_id = if room_id.is_empty() {
        source
            .get("room_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        room_id.to_owned()
    };
    let ts = source
        .get("origin_server_ts")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        / 1000.0;
    let is_bot = is_bot_sender(
        &sender,
        &msgtype,
        rules.bot_user_ids,
        rules.bot_localpart_patterns,
    );
    RoomEvent {
        event_id: event_id_of(source),
        room_id: event_room_id,
        sender,
        sender_display: display_name,
        body: content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        formatted_body: formatted_body.clone(),
        msgtype,
        ts,
        thread_root,
        reply_to,
        reply_is_fallback,
        mentions: parse_mentions(content, formatted_body.as_deref()),
        is_bot,
    }
}

/// What one message this account sends relates to.
#[derive(Debug, Default, Clone)]
pub struct Relation<'a> {
    /// Puts the message in a thread (`rel_type: m.thread`).
    pub thread_root: Option<&'a str>,
    /// A REAL rich reply to a specific event: clients render it as a quote, and
    /// the receiving connector treats it as "somebody replied to me".
    pub reply_to: Option<&'a str>,
    /// The thread's newest event, which a threaded message points at ONLY so
    /// clients without thread support show something sensible. It is marked
    /// `is_falling_back`, which is exactly what stops it being read as a reply.
    /// Inside a thread `reply_to` wins over it.
    pub thread_fallback: Option<&'a str>,
}

/// The `m.room.message` content for one message this account sends.
///
/// One helper for every path that posts, because the relation shape is the part
/// a room notices and the part that is easy to get subtly wrong. `m.mentions`
/// is written only when there is somebody to mention: a message addressed to
/// nobody must not carry an empty one.
#[must_use]
pub fn build_reply_content(
    body: &str,
    msgtype: &str,
    relation: &Relation<'_>,
    mentions: &[String],
) -> Value {
    let mut content = serde_json::Map::new();
    content.insert("msgtype".to_owned(), Value::String(msgtype.to_owned()));
    content.insert("body".to_owned(), Value::String(body.to_owned()));
    if !mentions.is_empty() {
        content.insert(
            "m.mentions".to_owned(),
            serde_json::json!({ "user_ids": mentions }),
        );
    }
    if let Some(root) = relation.thread_root.filter(|root| !root.is_empty()) {
        let target = relation
            .reply_to
            .or(relation.thread_fallback)
            .unwrap_or(root);
        content.insert(
            "m.relates_to".to_owned(),
            serde_json::json!({
                "rel_type": THREAD_REL_TYPE,
                "event_id": root,
                "is_falling_back": relation.reply_to.is_none(),
                "m.in_reply_to": { "event_id": target },
            }),
        );
    } else if let Some(reply_to) = relation.reply_to {
        content.insert(
            "m.relates_to".to_owned(),
            serde_json::json!({ "m.in_reply_to": { "event_id": reply_to } }),
        );
    }
    Value::Object(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ME: &str = "@bot-a:example.com";
    const HUMAN: &str = "@human:example.com";
    const OTHER_BOT: &str = "@bot-b:example.com";
    const ROOM_ID: &str = "!room:example.com";

    fn rules<'a>(ids: &'a [String], patterns: &'a [Regex]) -> BotRules<'a> {
        BotRules {
            bot_user_ids: ids,
            bot_localpart_patterns: patterns,
        }
    }

    fn source(content: &Value, sender: &str, event_id: &str, ts_ms: u64) -> Value {
        json!({
            "type": "m.room.message",
            "event_id": event_id,
            "sender": sender,
            "origin_server_ts": ts_ms,
            "room_id": ROOM_ID,
            "content": content.clone(),
        })
    }

    fn plain(body: &str) -> Value {
        json!({ "msgtype": "m.text", "body": body })
    }

    fn event(content: &Value, sender: &str) -> RoomEvent {
        from_source(
            &source(content, sender, "$evt", 1_700_000_000_000),
            ROOM_ID,
            None,
            rules(&[], &[]),
        )
    }

    fn thread_relation(root: &str, latest: &str, falling_back: bool) -> Value {
        json!({
            "rel_type": "m.thread",
            "event_id": root,
            "is_falling_back": falling_back,
            "m.in_reply_to": { "event_id": latest },
        })
    }

    #[test]
    fn basic_fields_are_normalised() {
        let ev = from_source(
            &source(&plain("hello there"), HUMAN, "$e1", 1_700_000_000_000),
            ROOM_ID,
            Some("Alex".to_owned()),
            rules(&[], &[]),
        );
        assert_eq!(ev.event_id, "$e1");
        assert_eq!(ev.room_id, ROOM_ID);
        assert_eq!(ev.sender, HUMAN);
        assert_eq!(ev.display(), "Alex");
        assert_eq!(ev.body, "hello there");
        assert_eq!(ev.msgtype, "m.text");
        assert!((ev.ts - 1_700_000_000.0).abs() < f64::EPSILON);
        assert!(ev.mentions.is_empty());
        assert!(ev.thread_root.is_none());
        assert!(ev.reply_to.is_none());
        assert!(!ev.is_bot);
    }

    #[test]
    fn display_falls_back_to_the_localpart() {
        let ev = event(&plain("hi"), "@stranger:example.com");
        assert_eq!(ev.display(), "stranger");
    }

    #[test]
    fn mentions_come_from_m_mentions() {
        let ev = event(
            &json!({ "msgtype": "m.text", "body": "hi", "m.mentions": { "user_ids": [ME, OTHER_BOT] } }),
            HUMAN,
        );
        assert_eq!(
            ev.mentions,
            BTreeSet::from([ME.to_owned(), OTHER_BOT.to_owned()])
        );
    }

    #[test]
    fn an_empty_m_mentions_means_nobody_even_with_pills() {
        let ev = event(
            &json!({
                "msgtype": "m.text",
                "body": "hi",
                "m.mentions": { "user_ids": [] },
                "format": "org.matrix.custom.html",
                "formatted_body": format!("<a href=\"https://matrix.to/#/{ME}\">bot-a</a>"),
            }),
            HUMAN,
        );
        assert!(ev.mentions.is_empty(), "MSC3952 is authoritative");
    }

    #[test]
    fn mentions_fall_back_to_matrix_to_pills() {
        let ev = event(
            &json!({
                "msgtype": "m.text",
                "body": "bot-a: look at this",
                "format": "org.matrix.custom.html",
                "formatted_body": format!("<a href=\"https://matrix.to/#/{ME}\">bot-a</a>: look"),
            }),
            HUMAN,
        );
        assert_eq!(ev.mentions, BTreeSet::from([ME.to_owned()]));
    }

    #[test]
    fn the_pill_fallback_decodes_percent_encoded_ids() {
        assert_eq!(
            parse_pills(Some(
                r#"<a href="https://matrix.to/#/%40bot-a%3Aexample.com">bot A</a>"#
            )),
            BTreeSet::from([ME.to_owned()])
        );
    }

    #[test]
    fn the_pill_fallback_ignores_room_links() {
        assert!(
            parse_pills(Some(
                r#"<a href="https://matrix.to/#/!room:example.com">room</a>"#
            ))
            .is_empty()
        );
    }

    #[test]
    fn a_thread_relation_marks_its_reply_pointer_as_a_fallback() {
        let ev = event(
            &json!({
                "msgtype": "m.text",
                "body": "in thread",
                "m.relates_to": thread_relation("$root", "$latest", true),
            }),
            HUMAN,
        );
        assert_eq!(ev.thread_root.as_deref(), Some("$root"));
        assert_eq!(ev.reply_to.as_deref(), Some("$latest"));
        assert!(ev.reply_is_fallback);
        assert!(!ev.is_direct_reply());
        assert_eq!(ev.thread_root_or_self(), "$root");
    }

    #[test]
    fn a_rich_reply_is_a_real_reply() {
        let ev = event(
            &json!({
                "msgtype": "m.text",
                "body": "about that",
                "m.relates_to": { "m.in_reply_to": { "event_id": "$target" } },
            }),
            HUMAN,
        );
        assert!(ev.thread_root.is_none());
        assert!(ev.is_direct_reply());
        assert_eq!(ev.thread_root_or_self(), "$evt");
    }

    #[test]
    fn a_threaded_reply_without_the_fallback_flag_counts_as_a_reply() {
        let ev = event(
            &json!({
                "msgtype": "m.text",
                "body": "answering you",
                "m.relates_to": thread_relation("$root", "$target", false),
            }),
            HUMAN,
        );
        assert_eq!(ev.thread_root.as_deref(), Some("$root"));
        assert!(ev.is_direct_reply());
    }

    #[test]
    fn a_notice_means_bot_and_a_plain_message_does_not() {
        assert!(event(&json!({ "msgtype": "m.notice", "body": "x" }), HUMAN).is_bot);
        assert!(!event(&plain("x"), OTHER_BOT).is_bot);
    }

    #[test]
    fn bots_are_named_by_id_or_by_localpart_pattern() {
        let ids = vec![HUMAN.to_owned()];
        let listed = from_source(
            &source(&plain("x"), HUMAN, "$e", 0),
            ROOM_ID,
            None,
            rules(&ids, &[]),
        );
        assert!(listed.is_bot);
        let patterns = vec![Regex::new("^bot-").expect("literal")];
        let matched = from_source(
            &source(&plain("x"), OTHER_BOT, "$e", 0),
            ROOM_ID,
            None,
            rules(&[], &patterns),
        );
        assert!(matched.is_bot);
        assert!(is_bot_sender(
            "@friend-agent:example.com",
            "m.text",
            &[],
            &[Regex::new("-agent$").expect("literal")]
        ));
        assert!(!is_bot_sender(
            "@friend-agent:example.com",
            "m.text",
            &[],
            &[]
        ));
    }

    #[test]
    fn it_round_trips_through_json() {
        let ev = event(
            &json!({
                "msgtype": "m.text",
                "body": "hi",
                "m.mentions": { "user_ids": [ME] },
                "m.relates_to": thread_relation("$root", "$latest", true),
            }),
            HUMAN,
        );
        let json = serde_json::to_string(&ev).expect("a RoomEvent serialises");
        let again: RoomEvent = serde_json::from_str(&json).expect("and reads back");
        assert_eq!(again, ev);
    }

    #[test]
    fn user_ids_written_in_a_message_are_found() {
        let one = |s: &str| BTreeSet::from([s.to_owned()]);
        assert_eq!(
            mentioned_user_ids("hi @bot-b:example.com, what do you think?"),
            one("@bot-b:example.com")
        );
        assert_eq!(
            mentioned_user_ids("ask @bot-b:matrix.example.com."),
            one("@bot-b:matrix.example.com")
        );
        assert_eq!(
            mentioned_user_ids("(@a:b.com) and @c:d.com!"),
            BTreeSet::from(["@a:b.com".to_owned(), "@c:d.com".to_owned()])
        );
        assert_eq!(
            mentioned_user_ids("@a:b.example.com:8448 keeps its port"),
            one("@a:b.example.com:8448")
        );
        assert!(mentioned_user_ids("write to alex@example.com about it").is_empty());
        assert!(mentioned_user_ids("no ids here at all").is_empty());
    }

    #[test]
    fn an_unthreaded_message_carries_no_relation_and_no_empty_mentions() {
        let content = build_reply_content(
            "just thinking aloud",
            NOTICE_MSGTYPE,
            &Relation::default(),
            &[],
        );
        assert_eq!(
            content,
            json!({ "msgtype": "m.notice", "body": "just thinking aloud" })
        );
    }

    #[test]
    fn a_threaded_message_points_at_the_thread_and_falls_back_to_its_newest() {
        let content = build_reply_content(
            "an answer",
            NOTICE_MSGTYPE,
            &Relation {
                thread_root: Some("$root"),
                thread_fallback: Some("$latest"),
                reply_to: None,
            },
            &[HUMAN.to_owned()],
        );
        assert_eq!(content["m.mentions"], json!({ "user_ids": [HUMAN] }));
        assert_eq!(
            content["m.relates_to"],
            json!({
                "rel_type": "m.thread",
                "event_id": "$root",
                "is_falling_back": true,
                "m.in_reply_to": { "event_id": "$latest" },
            })
        );
    }

    #[test]
    fn a_real_reply_inside_a_thread_is_not_marked_as_a_fallback() {
        let content = build_reply_content(
            "about that",
            NOTICE_MSGTYPE,
            &Relation {
                thread_root: Some("$root"),
                reply_to: Some("$asked"),
                thread_fallback: None,
            },
            &[],
        );
        assert_eq!(content["m.relates_to"]["is_falling_back"], json!(false));
        assert_eq!(
            content["m.relates_to"]["m.in_reply_to"],
            json!({ "event_id": "$asked" })
        );
    }

    #[test]
    fn a_reply_outside_a_thread_is_a_plain_rich_reply() {
        let content = build_reply_content(
            "about that",
            TEXT_MSGTYPE,
            &Relation {
                reply_to: Some("$asked"),
                ..Relation::default()
            },
            &[],
        );
        assert_eq!(content["msgtype"], json!(TEXT_MSGTYPE));
        assert_eq!(
            content["m.relates_to"],
            json!({ "m.in_reply_to": { "event_id": "$asked" } })
        );
    }

    #[test]
    fn a_threaded_message_with_no_pointer_points_at_the_root() {
        let content = build_reply_content(
            "in the thread",
            NOTICE_MSGTYPE,
            &Relation {
                thread_root: Some("$root"),
                ..Relation::default()
            },
            &[],
        );
        assert_eq!(
            content["m.relates_to"]["m.in_reply_to"],
            json!({ "event_id": "$root" })
        );
    }

    #[test]
    fn the_mention_list_is_written_in_the_order_it_was_given() {
        let content = build_reply_content(
            "hello you two",
            NOTICE_MSGTYPE,
            &Relation::default(),
            &[OTHER_BOT.to_owned(), HUMAN.to_owned()],
        );
        assert_eq!(
            content["m.mentions"],
            json!({ "user_ids": [OTHER_BOT, HUMAN] })
        );
    }

    #[test]
    fn only_a_real_line_of_conversation_counts_as_a_message() {
        let said = json!({
            "type": "m.room.message",
            "content": {"msgtype": "m.text", "body": "hello"},
        });
        assert!(is_message_source(&said));
        // A join, an image, and an empty husk are all things a session reading
        // the room must not be handed as if somebody had said them.
        assert!(!is_message_source(
            &json!({"type": "m.room.member", "content": {}})
        ));
        assert!(!is_message_source(
            &json!({"type": "m.room.message", "content": {"msgtype": "m.image", "body": "cat.png"}})
        ));
        assert!(!is_message_source(
            &json!({"type": "m.room.message", "content": {"msgtype": "m.text", "body": "   "}})
        ));
        assert!(!is_message_source(&json!({"type": "m.room.message"})));
    }

    #[test]
    fn what_is_dropped_says_why_it_was_dropped() {
        // The debug line an operator reads when a picture is met with silence.
        for (source, reason) in [
            (
                json!({"type": "m.room.message", "content": {"msgtype": "m.image", "body": "cat.png"}}),
                "m.image is not a line of conversation",
            ),
            (
                json!({"type": "m.reaction", "content": {}}),
                "m.reaction is not a message",
            ),
            (
                json!({"type": "m.room.message", "content": {}}),
                "no msgtype (a redacted husk, or state-shaped)",
            ),
            (
                json!({"type": "m.room.message", "content": {"msgtype": "m.text", "body": " "}}),
                "an empty m.text body",
            ),
        ] {
            assert_eq!(skip_reason(&source).as_deref(), Some(reason));
        }
        assert_eq!(
            skip_reason(&json!({
                "type": "m.room.message",
                "content": {"msgtype": "m.emote", "body": "waves"},
            })),
            None
        );
    }

    /// One `m.replace` as a client sends it: the fallback body the room sees,
    /// and the real text in `m.new_content`.
    fn edit_source(target: &str, sender: &str, new_body: &str) -> Value {
        json!({
            "type": "m.room.message",
            "event_id": "$edit",
            "sender": sender,
            "content": {
                "msgtype": "m.text",
                "body": format!("* {new_body}"),
                "m.new_content": { "msgtype": "m.text", "body": new_body },
                "m.relates_to": { "rel_type": "m.replace", "event_id": target },
            },
        })
    }

    #[test]
    fn an_edit_is_read_as_the_new_text_and_never_as_the_fallback() {
        let Some(Correction::Edit(edit)) =
            correction_from(&edit_source("$original", HUMAN, "what I meant"))
        else {
            panic!("an m.replace is a correction");
        };
        assert_eq!(edit.event_id, "$edit");
        assert_eq!(edit.target, "$original");
        assert_eq!(edit.sender, HUMAN);
        assert_eq!(
            edit.body, "what I meant",
            "the `* ...` fallback is what the room shows, never what I read"
        );
        assert!(edit.formatted_body.is_none());
        assert!(edit.mentions.is_empty());
    }

    #[test]
    fn an_edit_carries_the_new_content_s_own_mentions_and_html() {
        let source = json!({
            "type": "m.room.message",
            "event_id": "$edit",
            "sender": HUMAN,
            "content": {
                "msgtype": "m.text",
                "body": "* bot-a, look again",
                "m.new_content": {
                    "msgtype": "m.text",
                    "body": "bot-a, look again",
                    "format": "org.matrix.custom.html",
                    "formatted_body": format!("<a href=\"https://matrix.to/#/{ME}\">bot-a</a>, look again"),
                },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$original" },
            },
        });
        let Some(Correction::Edit(edit)) = correction_from(&source) else {
            panic!("an m.replace is a correction");
        };
        assert_eq!(edit.mentions, BTreeSet::from([ME.to_owned()]));
        assert!(
            edit.formatted_body
                .is_some_and(|html| html.contains("bot-a"))
        );
    }

    #[test]
    fn a_plain_message_and_a_reply_are_not_corrections() {
        assert_eq!(
            correction_from(&source(&plain("hello"), HUMAN, "$e", 0)),
            None
        );
        let reply = json!({
            "type": "m.room.message",
            "event_id": "$e",
            "sender": HUMAN,
            "content": {
                "msgtype": "m.text",
                "body": "yes",
                "m.relates_to": { "m.in_reply_to": { "event_id": "$original" } },
            },
        });
        assert_eq!(correction_from(&reply), None);
    }

    #[test]
    fn an_m_replace_without_new_content_corrects_nothing() {
        // A half-written edit must not blank the line it points at.
        let source = json!({
            "type": "m.room.message",
            "event_id": "$edit",
            "sender": HUMAN,
            "content": {
                "msgtype": "m.text",
                "body": "* oops",
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$original" },
            },
        });
        assert_eq!(correction_from(&source), None);
    }

    #[test]
    fn a_redaction_is_read_from_both_places_the_room_version_puts_it() {
        // Room version 11 moved `redacts` into the content; older rooms keep it
        // at the top level, and a connector meets both.
        for source in [
            json!({
                "type": "m.room.redaction",
                "event_id": "$redaction",
                "sender": HUMAN,
                "redacts": "$original",
                "content": { "reason": "wrong room" },
            }),
            json!({
                "type": "m.room.redaction",
                "event_id": "$redaction",
                "sender": HUMAN,
                "content": { "redacts": "$original" },
            }),
        ] {
            let Some(Correction::Redaction(redaction)) = correction_from(&source) else {
                panic!("an m.room.redaction is a correction");
            };
            assert_eq!(redaction.event_id, "$redaction");
            assert_eq!(redaction.redacts, "$original");
            assert_eq!(redaction.sender, HUMAN);
        }
        assert_eq!(
            correction_from(&json!({"type": "m.room.redaction", "content": {}})),
            None,
            "a redaction that names nothing redacts nothing"
        );
    }
}
