//! Speaking policy: should I answer this event, and on whose invitation?
//!
//! Guards run in a fixed order and each one is separate, so a log line always
//! names the single rule that decided. The answer is one of four verdicts:
//!
//! - `reply`    - tier 1. I was addressed; answer now.
//! - `judge`    - tier 1 in a thread that has run out of energy: a bot
//!   mentioned me in a conversation that has been bot-only for a while, so the
//!   reply happens only if the cheap judge agrees. No back-off; I was asked.
//! - `consider` - tier 2. Nobody addressed me. Back off for a random moment,
//!   re-read the room, and speak only if nobody covered it and the judge
//!   agrees.
//! - `silent`   - no.
//!
//! This module is pure and synchronous, so the whole decision table is a unit
//! test and the reason strings - which the docs and the live gates quote - are
//! asserted here rather than guessed at.
//!
//! Three of the guards read the message BODY, and only for names
//! ([`crate::addressing`]): a vocative of one of my names is an invitation, a
//! vocative of somebody else's is an instruction to stay out of it, and the
//! bot-to-bot switch reads the first of those too, because no model can make a
//! Matrix mention and a name typed by an agent is the same address it is when a
//! person types it. A fourth reads the body for nothing but a pre-score, which
//! decides no verdict at all - only how long tier 2 waits before asking the
//! judge. Everything they need arrives in [`Cues`], so the decision stays a
//! pure function of the event, the ledger and what the room is called.

use std::collections::HashSet;

use crate::addressing::{Names, PreScore, pre_score};
use crate::config::{BotToBot, PolicyConfig};
use crate::events::RoomEvent;
use crate::ledger::Ledger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Reply,
    Judge,
    Consider,
    Silent,
}

impl Verdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Judge => "judge",
            Self::Consider => "consider",
            Self::Silent => "silent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub reason: String,
    /// True when nobody was talking to me. Tier 2 lives here.
    pub unaddressed: bool,
    /// The free read of the body ([`crate::addressing::pre_score`]), and empty
    /// everywhere it was never taken. It decides no verdict: what it changes is
    /// how long tier 2 backs off before asking the judge, and - when the cues
    /// say the room was asked something - whether the judge is asked at all.
    /// The cues travel with the score because the log line that acts on them
    /// has to say which ones they were.
    pub prescore: PreScore,
}

impl Decision {
    fn new(verdict: Verdict, reason: String, unaddressed: bool) -> Self {
        Self {
            verdict,
            reason,
            unaddressed,
            prescore: PreScore::default(),
        }
    }

    fn with_prescore(mut self, prescore: PreScore) -> Self {
        self.prescore = prescore;
        self
    }

    /// True only for an unconditional tier-1 answer.
    #[must_use]
    pub fn reply(&self) -> bool {
        self.verdict == Verdict::Reply
    }

    /// True when a brain has to agree before anything is posted.
    #[must_use]
    pub fn needs_judge(&self) -> bool {
        matches!(self.verdict, Verdict::Judge | Verdict::Consider)
    }
}

/// Who spoke last in one conversation, and when.
///
/// What arm 3f ("I was the last speaker here and the human came back within the
/// window") is built on. Built from the transcript by the caller, because that
/// is where the room's own order of events is; the guard here only reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct LastSpeaker {
    pub sender: String,
    /// The homeserver's timestamp, in epoch seconds.
    pub ts: f64,
    /// Thread root, or the room's timeline for an unthreaded line.
    pub conversation: String,
}

/// The room around one event: everything a guard needs that is not in the
/// event, the ledger or the config.
///
/// Borrowed rather than owned because it is built per message, and the names
/// are compiled regexes that must not be rebuilt for every line of chat.
#[derive(Debug, Clone)]
pub struct Cues<'a> {
    /// What the people in this room are called.
    pub names: &'a Names,
    /// Who spoke last here (arm 3f). None when the room has no history yet.
    pub last_speaker: Option<LastSpeaker>,
}

/// The names of a room nobody has told us anything about. `Cues::default()` is
/// a policy decision as much as a convenience: with no names, the body is not
/// read and the agent waits to be mentioned.
static NO_NAMES: Names = Names::empty();

impl Default for Cues<'_> {
    fn default() -> Self {
        Self {
            names: &NO_NAMES,
            last_speaker: None,
        }
    }
}

/// What the BODY says about who is being spoken to: guards 3c and 3d.
enum Named {
    /// One of MY names, in a position that means it: the reason to answer.
    Me(String),
    /// Somebody else's, which decides the whole thing - the line is theirs.
    SomebodyElse(Decision),
    Nobody,
}

/// Read the body for names (3c, 3d). The only guards that read what was said.
///
/// 3c is turn allocation: somebody typed my name, which is how people hand each
/// other the turn, and it must cost no back-off and no model call. 3d is the
/// other half of the same rule and the reason a room full of agents does not
/// pile onto one line: a name selects ONE speaker, so a vocative of somebody
/// else's name is silence - and a silence that costs nothing, because it is not
/// `unaddressed` and so wakes neither tier 2 nor an inner-thought probe.
fn read_names(ev: &RoomEvent, cfg: &PolicyConfig, cues: &Cues<'_>) -> Named {
    if !cfg.reply_to_names {
        return Named::Nobody;
    }
    if let Some(address) = cues.names.addresses_me(&ev.body, cfg.bare_name_addresses) {
        return Named::Me(format!(
            "named in the body ({}, {})",
            address.matched,
            address.form.as_str()
        ));
    }
    if let Some((user_id, address)) = cues
        .names
        .addresses_other(&ev.body, cfg.bare_name_addresses)
    {
        return Named::SomebodyElse(Decision::new(
            Verdict::Silent,
            format!("addressed to {} ({user_id}), not me", address.matched),
            false,
        ));
    }
    Named::Nobody
}

/// The vocative of one of MY names in this body, for the log line that has to
/// say which one.
///
/// `None` when the body names me in no position that means it, and whenever
/// `reply_to_names` is off - an operator who has turned the body off has turned
/// it off for bots as well, and the guard that reads it (2) and the guard that
/// acts on it (3c) must never disagree about that.
fn names_me(ev: &RoomEvent, cfg: &PolicyConfig, cues: &Cues<'_>) -> Option<String> {
    if !cfg.reply_to_names {
        return None;
    }
    cues.names
        .addresses_me(&ev.body, cfg.bare_name_addresses)
        .map(|address| format!("{}, {}", address.matched, address.form.as_str()))
}

/// Decide whether to answer `ev`, and say why.
#[must_use]
pub fn should_reply<S: std::hash::BuildHasher>(
    ev: &RoomEvent,
    me: &str,
    ledger: &Ledger,
    thread_memberships: &HashSet<String, S>,
    cfg: &PolicyConfig,
    cues: &Cues<'_>,
) -> Decision {
    // (1) Self-echo. Outside every config branch, before anything else: no
    // setting may ever make an agent answer itself.
    if ev.sender == me {
        return Decision::new(
            Verdict::Silent,
            "self-echo: the event is mine".to_owned(),
            false,
        );
    }

    // (2) Bot-to-bot switch.
    let mut bot_note: Option<String> = None;
    if ev.is_bot {
        match cfg.bot_to_bot {
            BotToBot::None => {
                return Decision::new(
                    Verdict::Silent,
                    format!("bot_to_bot=none: {} is a bot", ev.sender),
                    false,
                );
            }
            BotToBot::Mentions if !ev.mentions.contains(me) => {
                // A model cannot make a Matrix mention. It writes "@qwen", or
                // "Qwen, what do you think?", and both arrive as plain text -
                // so `mentions` reading `m.mentions` alone made every OTHER
                // agent unreachable by construction (the room log, 2026-09-04:
                // every bot-to-bot line refused as "did not mention me"). A
                // name typed by a bot is the same address it is when a person
                // types it, and it is read by the same code.
                let Some(named) = names_me(ev, cfg, cues) else {
                    return Decision::new(
                        Verdict::Silent,
                        format!("bot_to_bot=mentions: bot {} did not mention me", ev.sender),
                        false,
                    );
                };
                bot_note = Some(format!(
                    "bot_to_bot=mentions: bot {} named me ({named})",
                    ev.sender
                ));
            }
            _ => {}
        }
    }

    // (3) Am I addressed? A mention, a real reply to one of my events, my name
    // in the body, or a thread I have already spoken in - in that order, so a
    // log line names the strongest reason and not the first one that happened
    // to be checked.
    let addressed: Option<String> = if cfg.reply_to_mentions && ev.mentions.contains(me) {
        Some("mentioned".to_owned())
    } else if cfg.reply_to_mentions
        && ev.is_direct_reply()
        && ledger.is_my_event(ev.reply_to.as_deref())
    {
        Some(format!(
            "reply to my event {}",
            ev.reply_to.as_deref().unwrap_or_default()
        ))
    } else {
        match read_names(ev, cfg, cues) {
            Named::Me(reason) => Some(reason),
            // The line went to somebody else and the whole decision is made:
            // this beats thread stickiness, checked just below.
            Named::SomebodyElse(decision) => return decision,
            Named::Nobody => {
                if cfg.reply_in_own_threads
                    && ev
                        .thread_root
                        .as_ref()
                        .is_some_and(|root| thread_memberships.contains(root))
                {
                    Some(format!(
                        "thread {} I have posted in",
                        ev.thread_root.as_deref().unwrap_or_default()
                    ))
                } else {
                    // (3f) The human came straight back to something I said.
                    follow_up(ev, me, ledger, cfg, cues)
                }
            }
        }
    };
    let Some(addressed) = addressed else {
        return unaddressed(ev, ledger, cfg, cues);
    };
    // The guard that let this bot in says so in the same line as the arm that
    // decided, because "another agent typed my name" is the thing an operator
    // is looking for in the log when they ask why the two of them are talking.
    let addressed = match bot_note {
        Some(note) => format!("{note}, {addressed}"),
        None => addressed,
    };

    // (4) Budgets. Checked last so the log says what was refused and why. The
    // pair budget and the thread cap exist to stop bot-to-bot ping-pong; a
    // human is never throttled by them. The hourly cap is the cost guard and
    // applies to everyone.
    let now = ledger.now();
    if ev.is_bot {
        let pair = ledger.pair_allows(&ev.sender, now);
        if !pair.allowed {
            return Decision::new(Verdict::Silent, pair.reason, false);
        }
        let thread = ledger.thread_allows(ev.thread_root_or_self());
        if !thread.allowed {
            return Decision::new(Verdict::Silent, thread.reason, false);
        }
    }
    let hour = ledger.hour_allows(now);
    if !hour.allowed {
        return Decision::new(Verdict::Silent, hour.reason, false);
    }

    // (5) Energy decay, only for bots. A thread that has been bot-only for
    // `bot_only_turns_before_decay` messages gets the judge as a gate: the two
    // agents keep the right to answer each other, but have to have something to
    // say. A human in the thread resets the count and this branch with it.
    if ev.is_bot {
        let energy = ledger.energy_allows(ev.thread_root_or_self());
        if !energy.allowed {
            return Decision::new(
                Verdict::Judge,
                format!("{addressed}, but {}: the judge decides", energy.reason),
                false,
            );
        }
    }

    Decision::new(Verdict::Reply, addressed, false)
}

/// Arm 3f: the human came straight back to something I said.
///
/// "and why is that?" is a whole turn in a conversation and it names nobody,
/// quotes nothing and starts no thread, so every other guard reads it as a line
/// thrown at the room. What makes it mine is that I am the one who spoke last
/// here, and it arrived while that was still true. People take turns this way -
/// the prior art calls it follow-up recognition, and it is the one turn
/// allocation signal that is in the ORDER of the conversation rather than in
/// its words.
///
/// Three things keep it from becoming a licence to answer everything:
///
/// - only a HUMAN line, because two agents following each other up is a loop
///   with no human in it;
/// - only inside `followup_window_s`, and `0` turns the arm off entirely;
/// - and only when the LEDGER agrees I posted in that conversation: the
///   transcript records what the room said, the ledger records what I sent.
///
/// Anybody else speaking in between defeats it by construction - the last
/// speaker is then not me - and the budgets below still apply, because being
/// answered is not the same as being owed an answer.
fn follow_up(
    ev: &RoomEvent,
    me: &str,
    ledger: &Ledger,
    cfg: &PolicyConfig,
    cues: &Cues<'_>,
) -> Option<String> {
    #[allow(clippy::cast_precision_loss)]
    let window = cfg.followup_window_s as f64;
    if window <= 0.0 || ev.is_bot {
        return None;
    }
    let last = cues.last_speaker.as_ref()?;
    if last.sender != me {
        return None;
    }
    // Clock skew, not time travel: the transcript's ORDER is what says this
    // event came afterwards, and the two timestamps are read off two clocks.
    let dt = (ev.ts - last.ts).max(0.0);
    if dt > window {
        return None;
    }
    if !ledger
        .posts
        .iter()
        .any(|post| post.thread_root == last.conversation)
    {
        return None;
    }
    Some(format!("follow-up: I spoke last here {dt:.0} s ago"))
}

/// Tier 2: nobody addressed me. May I even consider speaking?
///
/// Order matters: the cheap, certain refusals come first, so an agent never
/// draws a back-off, wakes up and calls a model only to find out it had no
/// budget to speak anyway.
fn unaddressed(ev: &RoomEvent, ledger: &Ledger, cfg: &PolicyConfig, cues: &Cues<'_>) -> Decision {
    let quiet = |reason: String| Decision::new(Verdict::Silent, reason, true);
    if !cfg.answer_unaddressed {
        return quiet("unaddressed: answer_unaddressed is off".to_owned());
    }
    if ev.is_bot {
        if !cfg.bot_to_bot.tier2_on_bots() {
            // Bots never trigger tier 2. Two agents that answer each other's
            // unaddressed lines are a loop with no human in it - unless the
            // operator has asked for exactly that (`bot_to_bot:
            // conversational`), which is the one mode that lets two agents
            // talk to each other the way they talk to people.
            return quiet(format!(
                "unaddressed: tier 2 never triggers on a bot ({})",
                ev.sender
            ));
        }
        // The bounds that make a bot-to-bot conversation a conversation and
        // not a runaway. They are the same two the addressed path spends
        // below, checked here as well because this path never reaches it: a
        // loop that ping-pongs through tier 2 is still a loop.
        let now = ledger.now();
        let pair = ledger.pair_allows(&ev.sender, now);
        if !pair.allowed {
            return quiet(format!("unaddressed: {}", pair.reason));
        }
        let thread = ledger.thread_allows(ev.thread_root_or_self());
        if !thread.allowed {
            return quiet(format!("unaddressed: {}", thread.reason));
        }
    }
    let energy = ledger.energy_allows(ev.thread_root_or_self());
    if !energy.allowed {
        return quiet(format!("unaddressed: {}", energy.reason));
    }
    let now = ledger.now();
    let hour = ledger.hour_allows(now);
    if !hour.allowed {
        return quiet(format!("unaddressed: {}", hour.reason));
    }
    let tier2 = ledger.tier2_hour_allows(now);
    if !tier2.allowed {
        return quiet(format!("unaddressed: {}", tier2.reason));
    }
    // The free read of the line. It decides nothing - the judge still does,
    // and the stand-down re-read still runs - but a question with "you" in it
    // is one somebody is waiting on, and waiting forty seconds to start
    // thinking about it is the difference between a conversation and a form.
    let pre = pre_score(ev, cues, cfg);
    if pre.score >= cfg.prescore_fast {
        let reason = format!(
            "unaddressed: tier 2 candidate (pre-score {}: {}), short back-off",
            pre.score,
            pre.listed()
        );
        return Decision::new(Verdict::Consider, reason, true).with_prescore(pre);
    }
    Decision::new(
        Verdict::Consider,
        "unaddressed: tier 2 candidate, backing off before I decide".to_owned(),
        true,
    )
    .with_prescore(pre)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use crate::config::BudgetsConfig;
    use crate::events::{BotRules, from_source};
    use crate::ledger::Clock;
    use serde_json::{Value, json};

    const ME: &str = "@bot-a:example.com";
    const HUMAN: &str = "@human:example.com";
    const OTHER_BOT: &str = "@bot-b:example.com";
    const ROOM_ID: &str = "!room:example.com";
    /// The homeserver timestamp every event built here carries, in seconds:
    /// the follow-up arm measures against the EVENT's clock, not the ledger's.
    const EVENT_TS: f64 = 1_700_000_000.0;
    /// The conversation the follow-up tests speak in: the thread my answer to
    /// the first question opened, which is what `record_post` stores.
    const CONVERSATION: &str = "$conv";

    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<f64>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(1_000_000.0)))
        }
        fn advance(&self, seconds: f64) {
            *self.0.lock().expect("never poisoned") += seconds;
        }
        fn as_clock(&self) -> Clock {
            let inner = Arc::clone(&self.0);
            Arc::new(move || *inner.lock().expect("never poisoned"))
        }
    }

    struct EventSpec {
        body: String,
        sender: String,
        msgtype: String,
        mentions: Option<Vec<String>>,
        formatted_body: Option<String>,
        relates_to: Option<Value>,
    }

    impl Default for EventSpec {
        fn default() -> Self {
            Self {
                body: "hello".to_owned(),
                sender: HUMAN.to_owned(),
                msgtype: "m.text".to_owned(),
                mentions: None,
                formatted_body: None,
                relates_to: None,
            }
        }
    }

    fn event(spec: EventSpec) -> RoomEvent {
        let mut content = serde_json::Map::new();
        content.insert("msgtype".to_owned(), Value::String(spec.msgtype));
        content.insert("body".to_owned(), Value::String(spec.body));
        if let Some(mentions) = spec.mentions {
            content.insert("m.mentions".to_owned(), json!({ "user_ids": mentions }));
        }
        if let Some(html) = spec.formatted_body {
            content.insert(
                "format".to_owned(),
                Value::String("org.matrix.custom.html".to_owned()),
            );
            content.insert("formatted_body".to_owned(), Value::String(html));
        }
        if let Some(relation) = spec.relates_to {
            content.insert("m.relates_to".to_owned(), relation);
        }
        let source = json!({
            "type": "m.room.message",
            "event_id": "$evt",
            "sender": spec.sender,
            "origin_server_ts": 1_700_000_000_000u64,
            "room_id": ROOM_ID,
            "content": Value::Object(content),
        });
        from_source(
            &source,
            ROOM_ID,
            None,
            BotRules {
                bot_user_ids: &[],
                bot_localpart_patterns: &[],
            },
        )
    }

    fn mention(who: &str) -> EventSpec {
        EventSpec {
            mentions: Some(vec![who.to_owned()]),
            ..EventSpec::default()
        }
    }

    fn bot(spec: EventSpec) -> EventSpec {
        EventSpec {
            sender: OTHER_BOT.to_owned(),
            msgtype: "m.notice".to_owned(),
            ..spec
        }
    }

    fn thread_relation(root: &str, latest: &str) -> Value {
        json!({
            "rel_type": "m.thread",
            "event_id": root,
            "is_falling_back": true,
            "m.in_reply_to": { "event_id": latest },
        })
    }

    fn reply_relation(target: &str) -> Value {
        json!({ "m.in_reply_to": { "event_id": target } })
    }

    fn ledger(dir: &Path, clock: &FakeClock) -> Ledger {
        Ledger::load(
            &dir.join("ledger.json"),
            BudgetsConfig::default(),
            clock.as_clock(),
        )
    }

    fn decide(ev: &RoomEvent, led: &Ledger, cfg: &PolicyConfig) -> Decision {
        should_reply(ev, ME, led, &led.thread_roots(), cfg, &Cues::default())
    }

    /// The same decision, in a room where everybody has a name.
    fn decide_named(ev: &RoomEvent, led: &Ledger, cfg: &PolicyConfig) -> Decision {
        let names = room_names();
        let cues = Cues {
            names: &names,
            ..Cues::default()
        };
        should_reply(ev, ME, led, &led.thread_roots(), cfg, &cues)
    }

    /// The same decision, in a room where somebody spoke last (arm 3f).
    fn decide_after(
        ev: &RoomEvent,
        led: &Ledger,
        cfg: &PolicyConfig,
        last: LastSpeaker,
    ) -> Decision {
        let names = room_names();
        let cues = Cues {
            names: &names,
            last_speaker: Some(last),
        };
        should_reply(ev, ME, led, &led.thread_roots(), cfg, &cues)
    }

    /// `who` spoke `ago` seconds before the event under test, in `$conv`.
    fn spoke_last(who: &str, ago: f64) -> LastSpeaker {
        LastSpeaker {
            sender: who.to_owned(),
            ts: EVENT_TS - ago,
            conversation: CONVERSATION.to_owned(),
        }
    }

    /// Me, the other bot and the human, by the names a room would know them by.
    fn room_names() -> Names {
        Names::new(
            &["bot-a".to_owned(), "qwen".to_owned()],
            vec![
                (
                    OTHER_BOT.to_owned(),
                    vec!["bot-b".to_owned(), "alex".to_owned()],
                ),
                (HUMAN.to_owned(), vec!["human".to_owned()]),
            ],
        )
    }

    fn policy() -> PolicyConfig {
        PolicyConfig::default()
    }

    // -- the decision table ---------------------------------------------
    //
    // Each case names the guard that must decide, the verdict it must reach,
    // and a fragment of the reason string, so a later guard cannot silently
    // take over.

    #[test]
    fn self_echo_wins_over_every_switch_and_a_mention_of_me() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let ev = event(EventSpec {
            sender: ME.to_owned(),
            msgtype: "m.notice".to_owned(),
            mentions: Some(vec![ME.to_owned()]),
            ..EventSpec::default()
        });
        let mut cfg = policy();
        cfg.bot_to_bot = BotToBot::All;
        let decision = decide(&ev, &led, &cfg);
        assert_eq!(decision.verdict, Verdict::Silent);
        assert!(
            decision.reason.starts_with("self-echo"),
            "{}",
            decision.reason
        );
        // ... and it is the self-echo guard that says so, not bot_to_bot=none.
        cfg.bot_to_bot = BotToBot::None;
        assert!(decide(&ev, &led, &cfg).reason.starts_with("self-echo"));
    }

    #[test]
    fn the_bot_switch_is_decided_before_the_addressing_check() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let mentioned = event(bot(mention(ME)));
        let mut cfg = policy();

        cfg.bot_to_bot = BotToBot::None;
        let refused = decide(&mentioned, &led, &cfg);
        assert_eq!(refused.verdict, Verdict::Silent);
        assert!(
            refused.reason.contains("bot_to_bot=none"),
            "{}",
            refused.reason
        );

        cfg.bot_to_bot = BotToBot::Mentions;
        let unnamed = decide(&event(bot(EventSpec::default())), &led, &cfg);
        assert_eq!(unnamed.verdict, Verdict::Silent);
        assert!(
            unnamed.reason.contains("bot_to_bot=mentions"),
            "{}",
            unnamed.reason
        );

        let answered = decide(&mentioned, &led, &cfg);
        assert_eq!(answered.verdict, Verdict::Reply);
        assert_eq!(answered.reason, "mentioned");
    }

    #[test]
    fn bot_to_bot_all_still_needs_a_bot_to_address_me() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let mut cfg = policy();
        cfg.bot_to_bot = BotToBot::All;
        let decision = decide(&event(bot(EventSpec::default())), &led, &cfg);
        assert_eq!(decision.verdict, Verdict::Silent);
        assert!(
            decision.reason.contains("unaddressed"),
            "{}",
            decision.reason
        );
    }

    #[test]
    fn a_bot_that_types_my_name_has_addressed_me() {
        // No model can make a Matrix mention. The friend's agent wrote "@Qwen"
        // and ours answered "bot ... did not mention me" to every line of it
        // (the room log, 2026-09-04) - so `mentions` reads the body for my name
        // exactly as tier 1 does, and by the same code.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let mut cfg = policy();
        cfg.bot_to_bot = BotToBot::Mentions;

        let typed = event(bot(EventSpec {
            body: "hello bot-a, nice to meet you".to_owned(),
            ..EventSpec::default()
        }));
        assert!(
            typed.mentions.is_empty(),
            "the point of this line is that it carries no m.mentions"
        );
        let answered = decide_named(&typed, &led, &cfg);
        assert_eq!(answered.verdict, Verdict::Reply);
        assert!(
            answered
                .reason
                .contains("bot_to_bot=mentions: bot @bot-b:example.com named me (bot-a, leading)"),
            "the guard that let it in has to say so: {}",
            answered.reason
        );
        assert!(
            answered.reason.contains("named in the body"),
            "and so does the arm that decided: {}",
            answered.reason
        );

        // Somebody ELSE's name in a bot's line is still not mine.
        let theirs = event(bot(EventSpec {
            body: "alex, what do you think?".to_owned(),
            ..EventSpec::default()
        }));
        let refused = decide_named(&theirs, &led, &cfg);
        assert_eq!(refused.verdict, Verdict::Silent);
        assert!(
            refused.reason.contains("did not mention me"),
            "{}",
            refused.reason
        );

        // `none` is none: a name changes nothing about a mode that says no.
        cfg.bot_to_bot = BotToBot::None;
        let never = decide_named(&typed, &led, &cfg);
        assert_eq!(never.verdict, Verdict::Silent);
        assert!(never.reason.contains("bot_to_bot=none"), "{}", never.reason);

        // And an operator who turned the body off has turned it off here too.
        cfg.bot_to_bot = BotToBot::Mentions;
        cfg.reply_to_names = false;
        assert!(
            decide_named(&typed, &led, &cfg)
                .reason
                .contains("did not mention me")
        );
    }

    #[test]
    fn only_conversational_lets_a_bots_unaddressed_line_reach_tier_two() {
        // The whole difference between the modes, in one line nobody addressed.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let chatting = event(bot(EventSpec {
            body: "the deploy finished about an hour ago".to_owned(),
            ..EventSpec::default()
        }));
        let mut cfg = policy();

        for mode in [BotToBot::Mentions, BotToBot::All] {
            cfg.bot_to_bot = mode;
            let refused = decide_named(&chatting, &led, &cfg);
            assert_eq!(refused.verdict, Verdict::Silent, "{mode:?}");
            assert!(
                !refused.reason.contains("tier 2 candidate"),
                "{mode:?} let a bot's line reach tier 2: {}",
                refused.reason
            );
        }
        // `all` is the one that gets as far as the unaddressed guard, and is
        // refused BY it rather than by the switch.
        cfg.bot_to_bot = BotToBot::All;
        assert!(
            decide_named(&chatting, &led, &cfg)
                .reason
                .contains("tier 2 never triggers on a bot")
        );

        cfg.bot_to_bot = BotToBot::Conversational;
        let considered = decide_named(&chatting, &led, &cfg);
        assert_eq!(considered.verdict, Verdict::Consider);
        assert!(considered.unaddressed);
        assert!(
            considered.reason.contains("tier 2 candidate"),
            "{}",
            considered.reason
        );
    }

    #[test]
    fn a_conversation_between_bots_still_answers_to_the_loop_bounds() {
        // Tier 2 on a bot's line is a new way into the room, so it takes the
        // same bounds the addressed path spends: the pair budget, the thread
        // cap and the thread's energy. Without them, `conversational` would be
        // two agents with a fresh licence to ping-pong.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        let mut cfg = policy();
        cfg.bot_to_bot = BotToBot::Conversational;
        let chatting = event(bot(EventSpec {
            body: "and the build is green again".to_owned(),
            ..EventSpec::default()
        }));
        assert_eq!(
            decide_named(&chatting, &led, &cfg).verdict,
            Verdict::Consider
        );

        // The pair budget: three answers to this bot inside a minute.
        for i in 0..cfg.budgets.per_pair_per_minute {
            led.record_post(&format!("$pair{i}"), "$other", OTHER_BOT, None, 2);
        }
        let spent = decide_named(&chatting, &led, &cfg);
        assert_eq!(spent.verdict, Verdict::Silent);
        assert!(spent.reason.contains("pair budget"), "{}", spent.reason);
        assert!(spent.unaddressed, "it is still a line nobody addressed");

        // The thread cap, on a fresh ledger so the pair budget is not what
        // answers (a ledger is a file, and these two share a clock, not a
        // state).
        let capped = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(capped.path(), &clock);
        for i in 0..cfg.budgets.per_thread_max {
            led.record_post(&format!("$thread{i}"), "$evt", HUMAN, None, 1);
        }
        let full = decide_named(&chatting, &led, &cfg);
        assert_eq!(full.verdict, Verdict::Silent);
        assert!(full.reason.contains("thread budget"), "{}", full.reason);

        // And the energy: a thread that has run out stops taking new lines
        // from bots at all, which is what winds a bot-only exchange down.
        let flat_dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(flat_dir.path(), &clock);
        for _ in 0..cfg.budgets.bot_only_turns_before_decay {
            led.note_event("$evt", true);
        }
        let flat = decide_named(&chatting, &led, &cfg);
        assert_eq!(flat.verdict, Verdict::Silent);
        assert!(flat.reason.contains("energy decay"), "{}", flat.reason);
    }

    #[test]
    fn a_mention_a_pill_a_reply_and_a_thread_all_address_me() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        let cfg = policy();

        assert_eq!(
            decide(&event(mention(ME)), &led, &cfg).verdict,
            Verdict::Reply
        );

        let pill = event(EventSpec {
            formatted_body: Some(format!("<a href=\"https://matrix.to/#/{ME}\">qa</a> ping")),
            ..EventSpec::default()
        });
        assert_eq!(decide(&pill, &led, &cfg).verdict, Verdict::Reply);

        led.record_post("$mine", "$root", HUMAN, None, 1);
        let rich_reply = event(EventSpec {
            relates_to: Some(reply_relation("$mine")),
            ..EventSpec::default()
        });
        let answered = decide(&rich_reply, &led, &cfg);
        assert_eq!(answered.verdict, Verdict::Reply);
        assert!(
            answered.reason.contains("reply to my event"),
            "{}",
            answered.reason
        );

        let in_thread = event(EventSpec {
            relates_to: Some(thread_relation("$root", "$latest")),
            ..EventSpec::default()
        });
        let sticky = decide(&in_thread, &led, &cfg);
        assert_eq!(sticky.verdict, Verdict::Reply);
        assert!(sticky.reason.contains("thread $root"), "{}", sticky.reason);
    }

    #[test]
    fn the_tier_one_switches_can_be_turned_off_one_at_a_time() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", "$root", HUMAN, None, 1);
        let mut cfg = policy();
        cfg.answer_unaddressed = false;

        cfg.reply_to_mentions = false;
        let silenced = decide(&event(mention(ME)), &led, &cfg);
        assert_eq!(silenced.verdict, Verdict::Silent);
        assert!(
            silenced.reason.contains("unaddressed"),
            "{}",
            silenced.reason
        );

        cfg.reply_to_mentions = true;
        cfg.reply_in_own_threads = false;
        let in_thread = event(EventSpec {
            relates_to: Some(thread_relation("$root", "$latest")),
            ..EventSpec::default()
        });
        assert_eq!(decide(&in_thread, &led, &cfg).verdict, Verdict::Silent);
    }

    #[test]
    fn a_thread_fallback_pointer_at_my_event_is_not_a_reply_on_its_own() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", "$root", HUMAN, None, 1);
        let mut cfg = policy();
        cfg.answer_unaddressed = false;
        let ev = event(EventSpec {
            relates_to: Some(thread_relation("$someone-elses-thread", "$mine")),
            ..EventSpec::default()
        });
        let decision = decide(&ev, &led, &cfg);
        assert_eq!(decision.verdict, Verdict::Silent);
        assert!(
            decision.reason.contains("unaddressed"),
            "{}",
            decision.reason
        );
    }

    #[test]
    fn a_rich_reply_to_somebody_elses_event_is_not_mine_to_answer() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", "$root", HUMAN, None, 1);
        let mut cfg = policy();
        cfg.answer_unaddressed = false;
        let ev = event(EventSpec {
            relates_to: Some(reply_relation("$not-mine")),
            ..EventSpec::default()
        });
        assert_eq!(decide(&ev, &led, &cfg).verdict, Verdict::Silent);
    }

    #[test]
    fn an_unaddressed_human_line_is_a_tier_two_candidate() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let decision = decide(
            &event(EventSpec {
                body: "just thinking aloud".to_owned(),
                ..EventSpec::default()
            }),
            &led,
            &policy(),
        );
        assert_eq!(decision.verdict, Verdict::Consider);
        assert!(decision.unaddressed);
        assert!(decision.needs_judge());
        assert!(
            decision.reason.contains("tier 2 candidate"),
            "{}",
            decision.reason
        );
    }

    #[test]
    fn every_way_tier_two_is_refused_names_itself() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let unaddressed_line = event(EventSpec {
            body: "just thinking aloud".to_owned(),
            ..EventSpec::default()
        });

        let mut cfg = policy();
        cfg.answer_unaddressed = false;
        let led = ledger(dir.path(), &clock);
        assert!(
            decide(&unaddressed_line, &led, &cfg)
                .reason
                .contains("answer_unaddressed is off")
        );

        let mut cfg = policy();
        cfg.bot_to_bot = BotToBot::All;
        let from_bot = event(bot(EventSpec {
            body: "just thinking aloud".to_owned(),
            ..EventSpec::default()
        }));
        assert!(
            decide(&from_bot, &led, &cfg)
                .reason
                .contains("tier 2 never triggers on a bot")
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(dir.path(), &clock);
        for _ in 0..led.budgets.bot_only_turns_before_decay {
            led.note_event("$root", true);
        }
        let in_dead_thread = event(EventSpec {
            body: "still going".to_owned(),
            relates_to: Some(thread_relation("$root", "$latest")),
            ..EventSpec::default()
        });
        assert!(
            decide(&in_dead_thread, &led, &policy())
                .reason
                .contains("energy decay")
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(dir.path(), &clock);
        for index in 0..led.budgets.per_hour_max {
            led.record_post(&format!("$mine{index}"), &format!("$t{index}"), "", None, 1);
            clock.advance(1.0);
        }
        assert!(
            decide(&unaddressed_line, &led, &policy())
                .reason
                .contains("hour budget")
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(dir.path(), &clock);
        for index in 0..led.budgets.tier2_per_hour_max {
            led.record_post(
                &format!("$mine{index}"),
                &format!("$t{index}"),
                HUMAN,
                None,
                2,
            );
            clock.advance(1.0);
        }
        assert!(
            decide(&unaddressed_line, &led, &policy())
                .reason
                .contains("tier-2 budget")
        );
    }

    #[test]
    fn the_budgets_refuse_a_bot_and_never_a_human() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        for index in 0..3 {
            led.record_post(
                &format!("$mine{index}"),
                "$other-thread",
                OTHER_BOT,
                None,
                1,
            );
        }
        let from_bot = decide(&event(bot(mention(ME))), &led, &policy());
        assert_eq!(from_bot.verdict, Verdict::Silent);
        assert!(
            from_bot.reason.contains("pair budget"),
            "{}",
            from_bot.reason
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(dir.path(), &clock);
        for index in 0..3 {
            led.record_post(&format!("$mine{index}"), "$other-thread", HUMAN, None, 1);
        }
        assert_eq!(
            decide(&event(mention(ME)), &led, &policy()).verdict,
            Verdict::Reply,
            "the pair budget is a bot-to-bot rule and must never throttle a human"
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(dir.path(), &clock);
        for index in 0..led.budgets.per_thread_max {
            led.record_post(
                &format!("$mine{index}"),
                "$root",
                "@nobody:example.com",
                None,
                1,
            );
        }
        let threaded_bot = event(bot(EventSpec {
            mentions: Some(vec![ME.to_owned()]),
            relates_to: Some(thread_relation("$root", "$latest")),
            ..EventSpec::default()
        }));
        let refused = decide(&threaded_bot, &led, &policy());
        assert_eq!(refused.verdict, Verdict::Silent);
        assert!(
            refused.reason.contains("thread budget"),
            "{}",
            refused.reason
        );
        let threaded_human = event(EventSpec {
            mentions: Some(vec![ME.to_owned()]),
            relates_to: Some(thread_relation("$root", "$latest")),
            ..EventSpec::default()
        });
        assert_eq!(
            decide(&threaded_human, &led, &policy()).verdict,
            Verdict::Reply
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut led = ledger(dir.path(), &clock);
        for index in 0..led.budgets.per_hour_max {
            led.record_post(&format!("$mine{index}"), &format!("$t{index}"), "", None, 1);
            clock.advance(1.0);
        }
        let busy = decide(
            &event(EventSpec {
                sender: "@someone-else:example.com".to_owned(),
                mentions: Some(vec![ME.to_owned()]),
                ..EventSpec::default()
            }),
            &led,
            &policy(),
        );
        assert_eq!(busy.verdict, Verdict::Silent);
        assert!(busy.reason.contains("hour budget"), "{}", busy.reason);
    }

    #[test]
    fn a_bot_mention_in_a_wound_down_thread_has_to_convince_the_judge() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        for _ in 0..led.budgets.bot_only_turns_before_decay {
            led.note_event("$root", true);
        }
        let threaded = |spec: EventSpec| {
            event(EventSpec {
                mentions: Some(vec![ME.to_owned()]),
                relates_to: Some(thread_relation("$root", "$latest")),
                ..spec
            })
        };
        let from_bot = decide(&threaded(bot(EventSpec::default())), &led, &policy());
        assert_eq!(from_bot.verdict, Verdict::Judge);
        assert!(from_bot.needs_judge());
        assert!(!from_bot.reply());
        assert!(
            from_bot.reason.contains("energy decay"),
            "{}",
            from_bot.reason
        );

        let from_human = decide(&threaded(EventSpec::default()), &led, &policy());
        assert_eq!(
            from_human.verdict,
            Verdict::Reply,
            "a human mention in a wound-down thread is answered as normal"
        );
    }

    #[test]
    fn unaddressed_is_decided_before_the_budgets() {
        // Guard order proof: with the PAIR budget exhausted AND no address, the
        // reason is the addressing guard - the pair budget is a bot-to-bot rule
        // and has no say over a human's unaddressed line.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        for index in 0..3 {
            led.record_post(
                &format!("$mine{index}"),
                "$other-thread",
                OTHER_BOT,
                None,
                1,
            );
        }
        let decision = decide(&event(EventSpec::default()), &led, &policy());
        assert_eq!(decision.verdict, Verdict::Consider);
        assert!(
            decision.reason.contains("unaddressed"),
            "{}",
            decision.reason
        );
    }

    #[test]
    fn tier_two_refusals_come_in_the_cheap_first_order() {
        // With EVERY tier-2 refusal true at once, the reason names the first
        // one. It matters because the later checks are the ones that would
        // otherwise have an agent sleep through a back-off and pay for a judge
        // call before finding out it was never allowed to speak.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        for _ in 0..led.budgets.bot_only_turns_before_decay {
            led.note_event("$root", true);
        }
        for index in 0..led.budgets.per_hour_max {
            led.record_post(&format!("$mine{index}"), &format!("$t{index}"), "", None, 1);
            clock.advance(1.0);
        }
        for index in 0..led.budgets.tier2_per_hour_max {
            led.record_post(
                &format!("$t2-{index}"),
                &format!("$u{index}"),
                HUMAN,
                None,
                2,
            );
            clock.advance(1.0);
        }
        let ev = event(EventSpec {
            body: "thinking aloud".to_owned(),
            relates_to: Some(thread_relation("$root", "$latest")),
            ..EventSpec::default()
        });

        let mut off = policy();
        off.answer_unaddressed = false;
        assert!(
            decide(&ev, &led, &off)
                .reason
                .contains("answer_unaddressed is off")
        );

        let on = decide(&ev, &led, &policy());
        assert_eq!(on.verdict, Verdict::Silent);
        assert!(
            on.reason.contains("energy decay"),
            "the decay check must come before the budgets: {}",
            on.reason
        );
    }

    #[test]
    fn a_mention_i_am_told_to_ignore_can_still_reach_tier_two() {
        // `reply_to_mentions: false` switches off the tier-1 branch, not the
        // agent. With tier 2 on, the message is unaddressed as far as tier 1 is
        // concerned and the judge gets to decide.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let mut cfg = policy();
        cfg.reply_to_mentions = false;
        assert_eq!(
            decide(&event(mention(ME)), &led, &cfg).verdict,
            Verdict::Consider
        );
    }

    // -- 3c and 3d: the body, read for names -----------------------------

    #[test]
    fn a_vocative_of_my_name_is_a_turn_allocation() {
        // The line that started all this: a typed name, no pill, no thread.
        // It must be tier 1 - no back-off, no judge, no model call.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let ev = event(EventSpec {
            body: "qwen, why are you so quiet?".to_owned(),
            ..EventSpec::default()
        });
        let decision = decide_named(&ev, &led, &policy());
        assert_eq!(decision.verdict, Verdict::Reply);
        assert!(!decision.needs_judge());
        assert!(!decision.unaddressed);
        assert_eq!(decision.reason, "named in the body (qwen, leading)");

        // ... and with the same event in a room that has never heard of me, it
        // is tier 2 - which is exactly what the shipped build did before.
        assert_eq!(decide(&ev, &led, &policy()).verdict, Verdict::Consider);
    }

    #[test]
    fn a_vocative_of_somebody_else_is_silent_with_no_judge() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let ev = event(EventSpec {
            body: "alex, what do you think?".to_owned(),
            ..EventSpec::default()
        });
        let decision = decide_named(&ev, &led, &policy());
        assert_eq!(decision.verdict, Verdict::Silent);
        assert_eq!(
            decision.reason,
            format!("addressed to alex ({OTHER_BOT}), not me")
        );
        assert!(!decision.needs_judge(), "a line for somebody else is free");
        assert!(
            !decision.unaddressed,
            "the line DID address somebody, so tier 2 and the inner probe have              nothing to want here"
        );
    }

    #[test]
    fn guard_order_other_vocative_before_thread_stickiness() {
        // I am in this thread, so without 3d I would answer. A name in the
        // line selects one speaker and that beats being in the room.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", "$root", HUMAN, None, 1);
        let threaded = |body: &str| {
            event(EventSpec {
                body: body.to_owned(),
                relates_to: Some(thread_relation("$root", "$latest")),
                ..EventSpec::default()
            })
        };
        assert_eq!(
            decide_named(&threaded("still with us?"), &led, &policy()).verdict,
            Verdict::Reply,
            "the thread is mine to answer while nobody else is named"
        );
        let elsewhere = decide_named(&threaded("alex, what do you think?"), &led, &policy());
        assert_eq!(elsewhere.verdict, Verdict::Silent);
        assert!(elsewhere.reason.contains("not me"), "{}", elsewhere.reason);

        // ... but it never beats a mention of me, or a reply to my event.
        let mentioned = event(EventSpec {
            body: "alex, what do you think?".to_owned(),
            mentions: Some(vec![ME.to_owned()]),
            ..EventSpec::default()
        });
        assert_eq!(
            decide_named(&mentioned, &led, &policy()).verdict,
            Verdict::Reply
        );
        let replied = event(EventSpec {
            body: "alex, what do you think?".to_owned(),
            relates_to: Some(reply_relation("$mine")),
            ..EventSpec::default()
        });
        let answered = decide_named(&replied, &led, &policy());
        assert_eq!(answered.verdict, Verdict::Reply);
        assert!(answered.reason.contains("reply to my event"));
    }

    #[test]
    fn two_bots_one_name_only_the_named_one_replies() {
        // The whole point of 3d, as the room sees it: one line, two agents,
        // exactly one answer - and no model call on either side.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let ev = event(EventSpec {
            body: "bot-a, how was your night?".to_owned(),
            ..EventSpec::default()
        });
        let names = room_names();
        let cues = Cues {
            names: &names,
            ..Cues::default()
        };
        let answered = should_reply(&ev, ME, &led, &led.thread_roots(), &policy(), &cues);
        assert_eq!(answered.verdict, Verdict::Reply);

        // The other agent, in the same room, with the names the other way round.
        let theirs = Names::new(
            &["bot-b".to_owned(), "alex".to_owned()],
            vec![
                (ME.to_owned(), vec!["bot-a".to_owned(), "qwen".to_owned()]),
                (HUMAN.to_owned(), vec!["human".to_owned()]),
            ],
        );
        let cues = Cues {
            names: &theirs,
            ..Cues::default()
        };
        let other = should_reply(&ev, OTHER_BOT, &led, &led.thread_roots(), &policy(), &cues);
        assert_eq!(other.verdict, Verdict::Silent);
        assert!(other.reason.contains(&format!("addressed to bot-a ({ME})")));
        assert!(!other.needs_judge());
    }

    #[test]
    fn bare_name_needs_you_or_goes_to_tier_two() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let bare = event(EventSpec {
            body: "somebody should ask qwen about it".to_owned(),
            ..EventSpec::default()
        });
        let considered = decide_named(&bare, &led, &policy());
        assert_eq!(
            considered.verdict,
            Verdict::Consider,
            "a bare name mid-sentence is talk ABOUT me: tier 2 decides, {}",
            considered.reason
        );
        assert!(considered.unaddressed);

        let asked = event(EventSpec {
            body: "what do you make of it qwen".to_owned(),
            ..EventSpec::default()
        });
        let answered = decide_named(&asked, &led, &policy());
        assert_eq!(answered.verdict, Verdict::Reply);
        assert_eq!(
            answered.reason,
            "named in the body (qwen, bare with second person)"
        );

        // With the knob on, the bare name is enough on its own.
        let mut cfg = policy();
        cfg.bare_name_addresses = true;
        let knob = decide_named(&bare, &led, &cfg);
        assert_eq!(knob.verdict, Verdict::Reply);
        assert_eq!(knob.reason, "named in the body (qwen, bare)");
    }

    #[test]
    fn reply_to_names_off_leaves_the_body_unread() {
        // One switch, both arms: an operator who turns it off gets the
        // mention-only agent they had before, in both directions.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let mut cfg = policy();
        cfg.reply_to_names = false;
        cfg.answer_unaddressed = false;
        for body in ["qwen, why so quiet?", "alex, what do you think?"] {
            let decision = decide_named(
                &event(EventSpec {
                    body: body.to_owned(),
                    ..EventSpec::default()
                }),
                &led,
                &cfg,
            );
            assert_eq!(decision.verdict, Verdict::Silent, "{body:?}");
            assert!(
                decision.reason.contains("unaddressed"),
                "{body:?}: {}",
                decision.reason
            );
        }
    }

    #[test]
    fn a_named_line_still_answers_to_the_budgets() {
        // 3c is an invitation, not an exemption: the hourly cap is the cost
        // guard and it applies to everyone who asks.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        for index in 0..led.budgets.per_hour_max {
            led.record_post(&format!("$mine{index}"), &format!("$t{index}"), "", None, 1);
            clock.advance(1.0);
        }
        let decision = decide_named(
            &event(EventSpec {
                body: "qwen, are you there?".to_owned(),
                ..EventSpec::default()
            }),
            &led,
            &policy(),
        );
        assert_eq!(decision.verdict, Verdict::Silent);
        assert!(
            decision.reason.contains("hour budget"),
            "{}",
            decision.reason
        );
    }

    // -- 3f: the follow-up, and the pre-score below it --------------------

    #[test]
    fn a_follow_up_within_the_window_is_mine_after_it_is_not() {
        // The second half of a conversation: I answered, and the human came
        // straight back with a line that names nobody and quotes nothing.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", CONVERSATION, HUMAN, None, 1);
        let ev = event(EventSpec {
            body: "and why is that?".to_owned(),
            ..EventSpec::default()
        });

        let mine = decide_after(&ev, &led, &policy(), spoke_last(ME, 41.0));
        assert_eq!(mine.verdict, Verdict::Reply);
        assert_eq!(mine.reason, "follow-up: I spoke last here 41 s ago");
        assert!(
            !mine.needs_judge(),
            "a follow-up is tier 1: no judge, no wait"
        );
        assert!(!mine.unaddressed);

        // Past the window it is an ordinary line thrown at the room, which is
        // what it looks like to every other guard.
        let stale = decide_after(&ev, &led, &policy(), spoke_last(ME, 121.0));
        assert_eq!(stale.verdict, Verdict::Consider);
        assert!(stale.unaddressed);
    }

    #[test]
    fn a_follow_up_is_broken_by_any_other_speaker() {
        // Somebody else spoke in between, so the human was answering them and
        // not me. There is no window in which that is my line.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", CONVERSATION, HUMAN, None, 1);
        let ev = event(EventSpec {
            body: "and why is that?".to_owned(),
            ..EventSpec::default()
        });
        for other in [OTHER_BOT, HUMAN] {
            let decision = decide_after(&ev, &led, &policy(), spoke_last(other, 5.0));
            assert_eq!(decision.verdict, Verdict::Consider, "{other}");
            assert!(decision.unaddressed, "{other}");
        }

        // I spoke last, but somewhere else: the ledger is the record of what I
        // actually sent, and it has nothing in THIS conversation.
        let elsewhere = LastSpeaker {
            conversation: "$another".to_owned(),
            ..spoke_last(ME, 5.0)
        };
        assert_eq!(
            decide_after(&ev, &led, &policy(), elsewhere).verdict,
            Verdict::Consider
        );

        // And a bot following up on my line is a loop with no human in it.
        let mut cfg = policy();
        cfg.bot_to_bot = BotToBot::All;
        let from_a_bot = decide_after(
            &event(bot(EventSpec {
                body: "and why is that?".to_owned(),
                ..EventSpec::default()
            })),
            &led,
            &cfg,
            spoke_last(ME, 5.0),
        );
        assert_eq!(from_a_bot.verdict, Verdict::Silent);
        assert!(
            from_a_bot.reason.contains("never triggers on a bot"),
            "{}",
            from_a_bot.reason
        );
    }

    #[test]
    fn followup_window_zero_turns_the_arm_off() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", CONVERSATION, HUMAN, None, 1);
        let ev = event(EventSpec {
            body: "and why is that?".to_owned(),
            ..EventSpec::default()
        });
        let mut cfg = policy();
        cfg.followup_window_s = 0;
        let decision = decide_after(&ev, &led, &cfg, spoke_last(ME, 1.0));
        assert_eq!(
            decision.verdict,
            Verdict::Consider,
            "with the window off the line is tier 2's, {}",
            decision.reason
        );
        assert!(decision.unaddressed);
    }

    #[test]
    fn guard_order_other_vocative_before_the_follow_up() {
        // 3f sits AFTER 3d: a line naming somebody else, arriving right after I
        // spoke, is still theirs. The follow-up window is not a way round the
        // one guard that keeps several agents off one line.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock);
        led.record_post("$mine", CONVERSATION, HUMAN, None, 1);
        let theirs = decide_after(
            &event(EventSpec {
                body: "alex, what do you think?".to_owned(),
                ..EventSpec::default()
            }),
            &led,
            &policy(),
            spoke_last(ME, 5.0),
        );
        assert_eq!(theirs.verdict, Verdict::Silent);
        assert!(theirs.reason.contains(", not me"), "{}", theirs.reason);
    }

    #[test]
    fn a_pre_scored_line_says_so_and_takes_the_short_back_off() {
        // Nothing here decides to speak - the verdict is `consider` either way
        // and the judge still answers it. What the pre-score changes is the
        // reason string the log carries and the back-off drawn from it.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let obvious = decide_named(
            &event(EventSpec {
                body: "does anyone know why the build is red?".to_owned(),
                ..EventSpec::default()
            }),
            &led,
            &policy(),
        );
        assert_eq!(obvious.verdict, Verdict::Consider);
        assert_eq!(obvious.prescore.score, 8);
        assert_eq!(
            obvious.reason,
            "unaddressed: tier 2 candidate (pre-score 8: question, asked of the room, an \
             invitation to the room), short back-off"
        );

        // Below the threshold nothing about the line changed: same verdict,
        // same reason as every unaddressed line before the pre-score existed.
        let flat = event(EventSpec {
            body: "the build finished".to_owned(),
            ..EventSpec::default()
        });
        let quiet = decide_named(&flat, &led, &policy());
        assert_eq!(quiet.verdict, Verdict::Consider);
        assert_eq!(quiet.prescore.score, 0);
        assert_eq!(
            quiet.reason,
            "unaddressed: tier 2 candidate, backing off before I decide"
        );

        // ... and a topic is worth two points without reaching the threshold.
        let cfg = PolicyConfig {
            topics: vec!["build".to_owned()],
            ..policy()
        };
        assert_eq!(decide_named(&flat, &led, &cfg).prescore.score, 2);
    }

    #[test]
    fn a_mention_of_somebody_else_is_not_a_mention_of_me() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let led = ledger(dir.path(), &clock);
        let mut cfg = policy();
        cfg.answer_unaddressed = false;
        let ev = event(mention(OTHER_BOT));
        assert_eq!(ev.mentions, BTreeSet::from([OTHER_BOT.to_owned()]));
        assert_eq!(decide(&ev, &led, &cfg).verdict, Verdict::Silent);
    }
}
