//! Who a line of chat is talking TO.
//!
//! Matrix only reports an address it can prove: `m.mentions` when the sender
//! picked the pill out of the completion list, or a rich reply to one of my
//! events. People do not talk like that. "Qwen, why so quiet?" is the same
//! request as "@Qwen why so quiet?", and only the second one was ever answered
//! at once - the first fell through to tier 2, a random back-off and a model
//! call, and usually to silence.
//!
//! So this module reads the BODY, and only for names: whether a line is a
//! vocative of one of MY names (somebody selected me as the next speaker) or of
//! somebody ELSE's (they selected them, and I stay out of it). Both answers are
//! deterministic and free, which is the point - the research is unanimous that
//! turn allocation must not wait for a model, and that explicit addressees are
//! the only ones any recogniser gets right (arXiv 2501.16643: they are ~20% of
//! turns, and even a large model is near chance on the implicit rest).
//!
//! The forms that count, and one that does not:
//!
//! | form | example |
//! |---|---|
//! | leading | `qwen, why so quiet?` - also `hey qwen - what now?` and `qwen why so quiet` |
//! | at | `what does @qwen think?` |
//! | trailing | `what do you think, qwen?` |
//! | parenthetical | `so, qwen, what now?` |
//! | bare with second person | `what do you think qwen` |
//! | bare | `ask qwen about it` - NOT an address (owner, 2026-09-04) |
//!
//! Three things stop a name from meaning "I am talking to you":
//!
//! - a floor of three characters, so an initial or a two-letter handle cannot
//!   pick up a syllable in the middle of a sentence;
//! - consumed boundaries around the name - Rust's regex has no lookaround - in
//!   which the hyphen counts as part of a word, so `bot` does not match inside
//!   `bot-a` and `qwen's` is not a vocative;
//! - a next-token filter on the bare leading form: `qwen is depressed` is
//!   ABOUT qwen, `qwen why are you depressed` is TO qwen.
//!
//! The module is pure and synchronous, so the whole table of what counts and
//! what does not is a unit test rather than a live gate's guess.
//!
//! Two more things live here for the same reason. [`pre_score`] is the free
//! read of a line nobody addressed: not addressing, and never a verdict - it
//! says how obviously somebody is waiting for an answer, so that tier 2 can
//! wait five seconds over a question instead of forty. [`addresses_room`] is
//! the one cue inside it that the judge is told about as well: "you two, talk
//! amongst yourselves" selects nobody, so every guard here reads it as a line
//! thrown at the room - and a judge asked to infer an invitation out of prose
//! infers silence instead.
//!
//! [`invites_an_answer`] is the half of that which is turn allocation rather
//! than a cue: the room was thrown open AND asked something. Selecting the
//! floor at large is how people hand the turn to whoever wants it, so that
//! line is answered by the first agent to reach it - deterministically, with
//! no judge in the way (`policy.room_invitations`).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::PolicyConfig;
use crate::events::{RoomEvent, localpart};
use crate::policy::Cues;

/// The shortest name that may address anybody. Two characters would find
/// initials and syllables; three is the floor the display-name risk is bought
/// off with (see `docs/DESIGN.md`, "Addressing").
pub const MIN_NAME_CHARS: usize = 3;

/// The hyphen and its two typographic cousins, written as escapes so that no
/// such character appears in this tree. They are treated as WORD characters at
/// a boundary: localparts are full of hyphens, and a name that stopped at one
/// would match the `bot` inside `bot-a`.
const DASHES: &str = r"\-\x{2013}\x{2014}";

/// Greetings a vocative may hide behind: "hey qwen", "ok so qwen". Longest
/// first, because the regex crate prefers the first alternative that matches.
const FILLER: &str = r"(?:thank[ \t]+you|thanks|hello|hey|hi|okay|ok|yo|oi|sorry|so|please)";

/// Words that, right after a name at the start of a line, mean the sentence is
/// about that person rather than addressed to them.
const ABOUT_NOT_TO: [&str; 21] = [
    "is", "was", "isn't", "wasn't", "has", "had", "seems", "looks", "said", "says", "will",
    "would", "can", "could", "does", "did", "keeps", "kept", "and", "or", "also",
];

/// Second person, in the forms people actually type. Not a name, so the
/// three-character floor does not apply to it.
static SECOND_PERSON: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i){}(?:you['\x{{2019}}]re|youre|yours|your|you|u){}",
        boundary_start(),
        boundary_end()
    ))
    .ok()
});

/// Words that ask the ROOM rather than a person: "anyone around?", "who knows
/// this?". Not an address - nobody was selected - but the strongest hint there
/// is that somebody is waiting for an answer from whoever has one.
static OPEN_ADDRESS: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:anyone|someone|who)\b").ok());

/// An invitation handed to the ROOM: "you two", "amongst yourselves",
/// "everyone". Nobody is selected, so it is not an address - but somebody has
/// asked for an answer from whoever is here, and the one thing an agent must
/// not do with that is decide the conversation has settled.
static ROOM_INVITATION: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let phrases = [
        // second person, plural: who is being spoken to is "all of you"
        r"you[ \t]+(?:all|two|three|both|guys|lot|folks|people)",
        r"y['\x{2019}]all",
        r"yall",
        r"(?:all|both|any|each|some|one|none)[ \t]+of[ \t]+you",
        // reciprocal: the invitation is to talk to EACH OTHER
        r"(?:among|amongst|between)[ \t]+yourselves",
        r"each[ \t]+other",
        r"one[ \t]+another",
        // the room as a body
        r"every(?:one|body)",
        r"any(?:one|body)",
        r"some(?:one|body)",
        r"the[ \t]+room",
    ];
    Regex::new(&format!(
        "(?i){}(?:{}){}",
        boundary_start(),
        phrases.join("|"),
        boundary_end()
    ))
    .ok()
});

/// An imperative thrown at whoever is listening, at the start of a line:
/// "talk about the weather", "tell me what you think". Only at the start,
/// because that is where an instruction stands - "I did tell him" is not one.
static ROOM_IMPERATIVE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let lead =
        r"(?:so|ok|okay|now|just|please|hey|hi|and|then|you[ \t]+(?:should|could|can|two|all))";
    let verbs = r"(?:talk|chat|discuss|tell|say|speak|introduce|carry[ \t]+on|keep[ \t]+(?:going|talking)|go[ \t]+ahead)";
    Regex::new(&format!(
        r"(?im)^[ \t]*(?:{lead}[ \t,]+){{0,3}}{verbs}{}",
        boundary_end()
    ))
    .ok()
});

/// A boundary BEFORE a name: the start of the text, or one character that
/// cannot be part of a name.
fn boundary_start() -> String {
    format!(r"(?:^|[^\p{{L}}\p{{N}}_{DASHES}])")
}

/// A boundary AFTER a name. The apostrophes are excluded as well, so that
/// "qwen's day" is about qwen and never to qwen.
fn boundary_end() -> String {
    format!(r"(?:$|[^\p{{L}}\p{{N}}_{DASHES}'\x{{2019}}])")
}

/// A separator that may stand to the LEFT of a name: a comma, a semicolon, a
/// colon, or a dash with a space in front of it. The space matters - without it
/// every hyphenated word in the room would read as a separator.
fn separator_left() -> String {
    format!(r"(?:[,;:]|[ \t]+[{DASHES}])")
}

/// A separator that may stand to the RIGHT of a name.
fn separator_right() -> String {
    format!(r"(?:[ \t]*[,;:!?]|[ \t]+[{DASHES}])")
}

/// The shape of an address, for the log line that has to say WHY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vocative {
    Leading,
    At,
    Trailing,
    Parenthetical,
    Bare,
    BareSecondPerson,
}

impl Vocative {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leading => "leading",
            Self::At => "at",
            Self::Trailing => "trailing",
            Self::Parenthetical => "parenthetical",
            Self::Bare => "bare",
            Self::BareSecondPerson => "bare with second person",
        }
    }
}

/// One address found in a body: which form, and the text that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub form: Vocative,
    pub matched: String,
}

impl Address {
    fn new(form: Vocative, matched: String) -> Self {
        Self { form, matched }
    }
}

/// The names one participant answers to, compiled once.
///
/// Compiled once because the alternative is compiling six regexes per message
/// per member, and a room's names change when somebody joins or renames - which
/// is rare enough to rebuild on.
#[derive(Debug)]
pub struct Vocab {
    names: Vec<String>,
    at: Regex,
    leading_punct: Regex,
    leading_bare: Regex,
    trailing: Regex,
    parenthetical: Regex,
    bare: Regex,
}

impl Vocab {
    /// Compile the patterns for `names`, or None when there is nothing usable
    /// to compile.
    ///
    /// Names are trimmed, dropped under [`MIN_NAME_CHARS`], deduplicated
    /// case-insensitively and sorted longest first, so that the longer of two
    /// overlapping names wins the match. None also covers the case where the
    /// pattern does not compile at all, which for escaped literals means it was
    /// too large: an agent that cannot read names is one that waits to be
    /// mentioned, and that is the safe direction to fail in.
    #[must_use]
    pub fn new(names: &[String]) -> Option<Self> {
        let names = usable(names);
        if names.is_empty() {
            return None;
        }
        let group = format!(
            "({})",
            names
                .iter()
                .map(|name| regex::escape(name))
                .collect::<Vec<String>>()
                .join("|")
        );
        let (start, end) = (boundary_start(), boundary_end());
        let (left, right) = (separator_left(), separator_right());
        let leading = format!(r"(?im)^[ \t]*(?:{FILLER}[ \t,]+){{0,2}}{group}");
        Some(Self {
            at: Regex::new(&format!("(?i){start}@{group}{end}")).ok()?,
            leading_punct: Regex::new(&format!("{leading}{right}")).ok()?,
            leading_bare: Regex::new(&format!("{leading}{end}")).ok()?,
            trailing: Regex::new(&format!(r"(?im){left}[ \t]*{group}[ \t]*[.!?]*[ \t]*$")).ok()?,
            parenthetical: Regex::new(&format!(r"(?i){left}[ \t]*{group}{right}")).ok()?,
            bare: Regex::new(&format!("(?i){start}{group}{end}")).ok()?,
            names,
        })
    }

    /// The names as they were compiled: cleaned, longest first.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

/// Trim, drop the too-short, deduplicate case-insensitively, longest first.
fn usable(names: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let name = name.trim();
        if name.chars().count() < MIN_NAME_CHARS {
            continue;
        }
        let key = name.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(name.to_owned());
    }
    // Longest first so "bot-a" beats "bot" inside one alternation; the
    // secondary key is only there to make the order deterministic.
    out.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then_with(|| a.cmp(b))
    });
    out
}

/// The names a display name yields: the whole of it, and its first word.
///
/// Both, because people are addressed by either ("Alex Smith" answers to
/// "Alex"), and the caller does not have to know which is which - the floor and
/// the deduplication in [`Vocab::new`] sort it out.
#[must_use]
pub fn names_from_display(display: &str) -> Vec<String> {
    let display = display.trim();
    if display.is_empty() {
        return Vec::new();
    }
    let mut out = vec![display.to_owned()];
    if let Some(first) = display.split_whitespace().next()
        && first != display
    {
        out.push(first.to_owned());
    }
    out
}

/// The vocative forms of a name in `body`, strongest first, or None.
///
/// "Strongest" is the order the log reports and nothing else: any of them is an
/// address. The bare form is deliberately NOT here - see [`named_bare`].
#[must_use]
pub fn vocative(body: &str, vocab: &Vocab) -> Option<Address> {
    if let Some(matched) = first_group(&vocab.at, body) {
        return Some(Address::new(Vocative::At, matched));
    }
    if let Some(matched) = first_group(&vocab.leading_punct, body) {
        return Some(Address::new(Vocative::Leading, matched));
    }
    if let Some(matched) = leading_bare(body, vocab) {
        return Some(Address::new(Vocative::Leading, matched));
    }
    if let Some(matched) = first_group(&vocab.trailing, body) {
        return Some(Address::new(Vocative::Trailing, matched));
    }
    if let Some(matched) = first_group(&vocab.parenthetical, body) {
        return Some(Address::new(Vocative::Parenthetical, matched));
    }
    None
}

/// A name of `vocab` anywhere in `body`, in no particular position.
///
/// On its own this is NOT an address: "ask qwen about it" is about qwen. What
/// it means is the policy's business (`policy.bare_name_addresses`, and the
/// second-person rule).
#[must_use]
pub fn named_bare(body: &str, vocab: &Vocab) -> Option<String> {
    first_group(&vocab.bare, body)
}

/// Whether this line hands the turn to the ROOM rather than to a person.
///
/// "you two, talk amongst yourselves", "does anyone know?", "tell me what you
/// think". Nobody is selected, so it is not an address and it decides no
/// verdict - what it does is say, deterministically and for free, that somebody
/// asked for an answer and is waiting for one. Two things read it: the
/// pre-score, which stops the agent sitting out a long back-off on a line the
/// room is waiting on, and the judge prompt, which would otherwise have to
/// infer an invitation from prose and reliably infers silence instead (the room
/// log, 2026-09-04: "you should just talk amongst yourselves" -> "no, the
/// conversation has naturally settled").
#[must_use]
pub fn addresses_room(body: &str) -> bool {
    matched(&ROOM_INVITATION, body) || matched(&ROOM_IMPERATIVE, body)
}

/// Whether this line hands the turn to the room AND asks it for an answer.
///
/// [`addresses_room`] says the turn was thrown open; this says somebody is
/// waiting at the end of it. Two shapes qualify, and both are already
/// recognised above:
///
/// - a line thrown at the room that is a QUESTION - "so, anyone here got an
///   opinion on whether weekends should be three days long?";
/// - an imperative handed to whoever is listening - "tell me what you think",
///   "you two, talk amongst yourselves" - which is a request with no question
///   mark on it.
///
/// A line that merely mentions the room ("everyone is welcome to weigh in") is
/// not one: nobody is waiting, so nobody has to answer.
///
/// This is turn ALLOCATION, not self-selection. Webb's rule is that the current
/// speaker selects the next, and selecting the floor at large is one of the
/// ways they do it: whoever wants it takes it, and the quickest self-selector
/// gets it. A judge asked about such a line reads "addressed to nobody in
/// particular" as "not for me" and scores it out (the room log, 2026-09-05: a
/// 27B model on "anyone here got an opinion...?" answered *"2: it's a general
/// opinion question not directed at me"*), which is exactly the inference the
/// deterministic tier exists to stop anybody making.
#[must_use]
pub fn invites_an_answer(body: &str) -> bool {
    addresses_room(body) && (body.contains('?') || matched(&ROOM_IMPERATIVE, body))
}

/// One of the room patterns against a body, `false` when it did not compile.
fn matched(pattern: &LazyLock<Option<Regex>>, body: &str) -> bool {
    pattern
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(body))
}

/// Whether the body says "you" in any of the forms people type.
#[must_use]
pub fn second_person(body: &str) -> bool {
    SECOND_PERSON
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(body))
}

fn first_group(pattern: &Regex, body: &str) -> Option<String> {
    pattern
        .captures(body)
        .and_then(|caps| caps.get(1))
        .map(|matched| matched.as_str().to_owned())
}

/// The leading form without punctuation after the name, filtered by what comes
/// next: "qwen is depressed" is about qwen, "qwen why so quiet" is to qwen.
fn leading_bare(body: &str, vocab: &Vocab) -> Option<String> {
    // Every line, not just the first: a body that opens by talking ABOUT
    // somebody may still go on to talk TO them.
    for caps in vocab.leading_bare.captures_iter(body) {
        let Some(name) = caps.get(1) else {
            continue;
        };
        // From the END OF THE NAME, not of the match: the match has eaten the
        // boundary character, and the word after it is what decides.
        if !is_about_not_to(&body[name.end()..]) {
            return Some(name.as_str().to_owned());
        }
    }
    None
}

/// Whether the text right after a name turns it into a subject.
fn is_about_not_to(rest: &str) -> bool {
    let word: String = rest
        .chars()
        .skip_while(|c| !c.is_alphabetic())
        .take_while(|c| c.is_alphabetic() || *c == '\'' || *c == '\u{2019}')
        .collect();
    if word.is_empty() {
        return false;
    }
    let word = word.to_lowercase().replace('\u{2019}', "'");
    ABOUT_NOT_TO.contains(&word.as_str())
}

/// Every name this room can call somebody by: mine, and everybody else's.
///
/// Built from the member store rather than from configuration, because that is
/// where display names live and they change without anyone editing a config.
/// A name of mine is never also somebody else's: whoever else answers to it,
/// the line naming it is one I may answer, and "somebody else was addressed"
/// must never be said about my own name.
#[derive(Debug)]
pub struct Names {
    me: Option<Vocab>,
    others: Option<Vocab>,
    /// Lowercased name -> the user id that answers to it. First registration
    /// wins: two people who share a first name cannot both be selected by it,
    /// and either way the line is not addressed to ME, which is all arm 3d
    /// says.
    owners: BTreeMap<String, String>,
}

impl Default for Names {
    fn default() -> Self {
        Self::empty()
    }
}

impl Names {
    /// Nobody has a name yet: what a room looks like before the first sync.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            me: None,
            others: None,
            owners: BTreeMap::new(),
        }
    }

    /// Compile `me`'s names and every other member's.
    #[must_use]
    pub fn new(me: &[String], others: Vec<(String, Vec<String>)>) -> Self {
        let me = Vocab::new(me);
        let mine: Vec<String> = me
            .as_ref()
            .map(|vocab| vocab.names().iter().map(|n| n.to_lowercase()).collect())
            .unwrap_or_default();
        let mut owners: BTreeMap<String, String> = BTreeMap::new();
        let mut names: Vec<String> = Vec::new();
        for (user_id, theirs) in others {
            for name in usable(&theirs) {
                let key = name.to_lowercase();
                if mine.contains(&key) || owners.contains_key(&key) {
                    continue;
                }
                owners.insert(key, user_id.clone());
                names.push(name);
            }
        }
        Self {
            me,
            others: Vocab::new(&names),
            owners,
        }
    }

    /// How this body addresses ME, if it does.
    ///
    /// `bare_addresses` is `policy.bare_name_addresses`. Without it a bare name
    /// still addresses me when the body also says "you" - "what do you think
    /// qwen" is a question to qwen however it is punctuated.
    #[must_use]
    pub fn addresses_me(&self, body: &str, bare_addresses: bool) -> Option<Address> {
        let vocab = self.me.as_ref()?;
        if let Some(address) = vocative(body, vocab) {
            return Some(address);
        }
        let matched = named_bare(body, vocab)?;
        if bare_addresses {
            return Some(Address::new(Vocative::Bare, matched));
        }
        if second_person(body) {
            return Some(Address::new(Vocative::BareSecondPerson, matched));
        }
        None
    }

    /// How this body addresses somebody ELSE, if it does, and who.
    ///
    /// The second-person rule of [`Self::addresses_me`] deliberately has no
    /// twin here. "you should ask alex" names alex and asks ME; reading it as
    /// alex's line would silence the agent on a line addressed to it, and
    /// silence is the one failure this arm must not cause.
    #[must_use]
    pub fn addresses_other(&self, body: &str, bare_addresses: bool) -> Option<(&str, Address)> {
        let vocab = self.others.as_ref()?;
        let address = match vocative(body, vocab) {
            Some(address) => address,
            None if bare_addresses => Address::new(Vocative::Bare, named_bare(body, vocab)?),
            None => return None,
        };
        let user_id = self.owners.get(&address.matched.to_lowercase())?;
        Some((user_id.as_str(), address))
    }

    /// One of MY names anywhere in the body, in any position at all.
    ///
    /// NOT an address - that is [`Self::addresses_me`], which is about
    /// position and is the thing tier 1 acts on. This is what the pre-score
    /// means by "my name came up in it".
    #[must_use]
    pub fn named_me(&self, body: &str) -> Option<String> {
        named_bare(body, self.me.as_ref()?)
    }

    /// The names I answer to, longest first. Empty when I have none.
    #[must_use]
    pub fn mine(&self) -> &[String] {
        self.me.as_ref().map_or(&[], Vocab::names)
    }

    /// Everybody else's names, longest first.
    #[must_use]
    pub fn theirs(&self) -> &[String] {
        self.others.as_ref().map_or(&[], Vocab::names)
    }
}

/// The names one member answers to: their localpart, their display name and its
/// first word.
#[must_use]
pub fn names_for(user_id: &str, display: Option<&str>) -> Vec<String> {
    let mut names = vec![localpart(user_id).to_owned()];
    if let Some(display) = display {
        names.extend(names_from_display(display));
    }
    names
}

// -- the pre-score: how quickly is this line worth getting to? -------------

/// A question mark. The single strongest free signal that somebody is waiting.
const SCORE_QUESTION: u8 = 3;
/// "you", or a line asked of the room ("anyone around?").
const SCORE_SECOND_PERSON: u8 = 2;
/// One of my names came up, in a position that did not address me.
const SCORE_MY_NAME: u8 = 2;
/// A word from `policy.topics`: the subject this agent is in the room for.
const SCORE_TOPIC: u8 = 2;
/// An invitation handed to the room ("you two, talk amongst yourselves"). The
/// biggest of the free cues on purpose: it is the one line nobody is going to
/// repeat, and an agent that waits out a long back-off on it has missed it.
const SCORE_ROOM_INVITATION: u8 = 3;

/// What a line looks like before anybody has thought about it.
///
/// Tier 2 is a back-off and then a judge call, and the back-off is there so
/// that several agents do not answer at once. It costs nothing to notice that
/// a line is a QUESTION, that it says "you", that it named me in passing, or
/// that it is about the thing this agent is here for - and a line with those
/// in it is worth getting to sooner.
///
/// Nothing here decides whether to speak. The judge still does, exactly as
/// before, and the stand-down re-read still runs: this only decides how long
/// the room waits to find out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreScore {
    pub score: u8,
    /// The cues that were found, in the order they are scored - the log has to
    /// be able to say WHY a line was in a hurry.
    pub cues: Vec<&'static str>,
}

impl PreScore {
    fn add(&mut self, points: u8, cue: &'static str) {
        self.score = self.score.saturating_add(points);
        self.cues.push(cue);
    }

    /// The cues as the reason strings print them.
    ///
    /// A line can reach the fast path with nothing in it at all - that is what
    /// `prescore_fast: 0` means - and a log line reading "(pre-score 0: )" is
    /// a log line with a hole in it, so the empty list says so in words.
    #[must_use]
    pub fn listed(&self) -> String {
        if self.cues.is_empty() {
            return "nothing in particular".to_owned();
        }
        self.cues.join(", ")
    }
}

/// Score one unaddressed line. Deterministic, free, and never a verdict.
#[must_use]
pub fn pre_score(ev: &RoomEvent, cues: &Cues<'_>, cfg: &PolicyConfig) -> PreScore {
    let body = ev.body.as_str();
    let mut score = PreScore::default();
    if body.contains('?') {
        score.add(SCORE_QUESTION, "question");
    }
    if second_person(body) {
        score.add(SCORE_SECOND_PERSON, "second person");
    } else if OPEN_ADDRESS
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(body))
    {
        score.add(SCORE_SECOND_PERSON, "asked of the room");
    }
    if addresses_room(body) {
        score.add(SCORE_ROOM_INVITATION, "an invitation to the room");
    }
    if cues.names.named_me(body).is_some() {
        score.add(SCORE_MY_NAME, "my name");
    }
    if topic_word(body, &cfg.topics) {
        score.add(SCORE_TOPIC, "my subject");
    }
    score
}

/// Whether any `policy.topics` word stands as a word in the body.
fn topic_word(body: &str, topics: &[String]) -> bool {
    if topics.is_empty() {
        return false;
    }
    let haystack = body.to_lowercase();
    topics.iter().any(|topic| {
        let needle = topic.trim().to_lowercase();
        !needle.is_empty() && word_present(&haystack, &needle)
    })
}

/// `needle` as a whole word in `haystack`, both already lowercased.
///
/// The same boundary rule the names use, hyphen included as a word character:
/// a topic of "deploy" is not found in "deploy-bot", for the same reason a
/// member called "gate" is not addressed by a line naming `gate-bot-a`.
fn word_present(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(found) = haystack[from..].find(needle) {
        let start = from + found;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        if !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char) {
            return true;
        }
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '\u{2013}' || c == '\u{2014}'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab(names: &[&str]) -> Vocab {
        let owned: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        Vocab::new(&owned).expect("the test names compile")
    }

    fn names() -> Names {
        Names::new(
            &["qwen".to_owned(), "bot-a".to_owned()],
            vec![
                (
                    "@bot-b:example.com".to_owned(),
                    vec!["alex".to_owned(), "bot-b".to_owned()],
                ),
                (
                    "@human:example.com".to_owned(),
                    vec!["sam".to_owned(), "human".to_owned()],
                ),
            ],
        )
    }

    // -- the table -------------------------------------------------------
    //
    // Every row is a body somebody could type. The first column is what the
    // agent called "qwen" must conclude, and the reason strings the docs and
    // the live gates quote are built from the second.

    #[test]
    fn the_forms_that_address_me_and_the_ones_that_do_not() {
        let me = vocab(&["qwen", "bot-a"]);
        let cases: [(&str, Option<Vocative>); 28] = [
            // leading, with and without punctuation
            ("qwen, why so quiet?", Some(Vocative::Leading)),
            ("Qwen why are you so depressed", Some(Vocative::Leading)),
            ("qwen: do it", Some(Vocative::Leading)),
            ("qwen - what do you think?", Some(Vocative::Leading)),
            ("hey qwen, morning", Some(Vocative::Leading)),
            ("ok so qwen, what now", Some(Vocative::Leading)),
            ("thanks qwen", Some(Vocative::Leading)),
            ("thank you qwen!", Some(Vocative::Leading)),
            ("qwen?", Some(Vocative::Leading)),
            ("qwen.", Some(Vocative::Leading)),
            // the next-token filter: about, not to
            ("qwen is depressed", None),
            ("qwen was right about the build", None),
            ("qwen and alex should decide", None),
            ("qwen said the same thing", None),
            ("qwen isn't answering", None),
            // at
            ("what does @qwen think?", Some(Vocative::At)),
            ("ping @qwen:example.com about it", Some(Vocative::At)),
            // trailing
            ("what do you think, qwen?", Some(Vocative::Trailing)),
            ("any idea - qwen", Some(Vocative::Trailing)),
            ("I like qwen.", None),
            // parenthetical
            ("well, qwen, what now?", Some(Vocative::Parenthetical)),
            // ... and a filler in front of it is still the leading form
            ("so, qwen, what now?", Some(Vocative::Leading)),
            // bare: not an address on its own
            ("ask qwen about it", None),
            ("that is qwen's problem", None),
            // a longer name wins, and a name is not a syllable
            ("bot-a, are you there?", Some(Vocative::Leading)),
            ("bot-abacus, are you there?", None),
            ("qwenite, are you there?", None),
            ("the qwen team shipped it", None),
        ];
        for (body, expected) in cases {
            let found = vocative(body, &me).map(|address| address.form);
            assert_eq!(found, expected, "{body:?}");
        }
    }

    #[test]
    fn a_bare_name_needs_second_person_or_the_knob() {
        let all = names();
        for body in ["what do you think qwen", "any idea what qwen makes of u"] {
            let address = all.addresses_me(body, false).expect("addressed");
            assert_eq!(address.form, Vocative::BareSecondPerson, "{body:?}");
            assert_eq!(address.matched, "qwen");
        }
        assert!(
            all.addresses_me("ask qwen about it", false).is_none(),
            "a bare name with no second person is tier 2's business, not tier 1's"
        );
        let with_knob = all
            .addresses_me("ask qwen about it", true)
            .expect("the knob turns a bare name into an address");
        assert_eq!(with_knob.form, Vocative::Bare);
    }

    #[test]
    fn second_person_is_the_forms_people_type_and_not_a_syllable() {
        for body in [
            "what do you think",
            "is that YOUR build?",
            "you're right",
            "and yours?",
            "u around?",
        ] {
            assert!(second_person(body), "{body:?}");
        }
        for body in ["the queue is stuck", "nothing here", "a universal problem"] {
            assert!(!second_person(body), "{body:?}");
        }
    }

    #[test]
    fn a_name_of_mine_is_never_somebody_elses() {
        // Two members answer to "alex", and so do I. The line is mine to
        // answer, and 3d must not be able to say it was addressed elsewhere.
        let names = Names::new(
            &["alex".to_owned()],
            vec![("@alex2:example.com".to_owned(), vec!["alex".to_owned()])],
        );
        assert!(names.addresses_me("alex, ping", false).is_some());
        assert!(names.addresses_other("alex, ping", false).is_none());
    }

    #[test]
    fn a_vocative_of_somebody_else_names_them_and_their_user_id() {
        let all = names();
        let (user_id, address) = all
            .addresses_other("alex, what do you think?", false)
            .expect("alex was addressed");
        assert_eq!(user_id, "@bot-b:example.com");
        assert_eq!(address.form, Vocative::Leading);
        assert_eq!(address.matched, "alex");

        // ... and the second-person rule is mine alone: this line asks me.
        assert!(
            all.addresses_other("you should ask alex about it", false)
                .is_none(),
            "a bare name plus \"you\" must never silence me"
        );
    }

    // -- the pre-score ---------------------------------------------------

    /// One human line, as the homeserver sends it.
    fn line(body: &str) -> RoomEvent {
        crate::events::from_source(
            &serde_json::json!({
                "type": "m.room.message",
                "event_id": "$evt",
                "sender": "@human:example.com",
                "origin_server_ts": 1_700_000_000_000u64,
                "room_id": "!room:example.com",
                "content": { "msgtype": "m.text", "body": body },
            }),
            "!room:example.com",
            None,
            crate::events::BotRules {
                bot_user_ids: &[],
                bot_localpart_patterns: &[],
            },
        )
    }

    fn scored(body: &str, topics: &[&str]) -> PreScore {
        let all = names();
        let cues = Cues {
            names: &all,
            ..Cues::default()
        };
        let cfg = PolicyConfig {
            topics: topics.iter().map(|topic| (*topic).to_owned()).collect(),
            ..PolicyConfig::default()
        };
        pre_score(&line(body), &cues, &cfg)
    }

    #[test]
    fn the_pre_score_reads_the_cues_that_are_free_to_read() {
        // Every row is a line nobody addressed. The score is not a verdict and
        // never becomes one: it is how long tier 2 waits before asking.
        let cases: [(&str, u8, &[&str]); 12] = [
            (
                "does anyone know why the build is red?",
                8,
                &["question", "asked of the room", "an invitation to the room"],
            ),
            (
                "who is looking at this?",
                5,
                &["question", "asked of the room"],
            ),
            ("what do you think?", 5, &["question", "second person"]),
            ("what do you think", 2, &["second person"]),
            ("the build finished", 0, &[]),
            (
                "somebody should ask qwen about it",
                5,
                &["an invitation to the room", "my name"],
            ),
            // "you" wins the two points a room question would also have scored:
            // one cue, one score, and the log says which.
            (
                "anyone know what you make of it?",
                8,
                &["question", "second person", "an invitation to the room"],
            ),
            ("I like this queue", 0, &[]),
            // The line from the room log this whole slice came out of. Nobody
            // is addressed, nothing is a question, and it is the one line in
            // the room that must not be sat out.
            (
                "you should just talk amongst yourselves",
                5,
                &["second person", "an invitation to the room"],
            ),
            (
                "you two, talk amongst yourselves about the weather",
                5,
                &["second person", "an invitation to the room"],
            ),
            ("tell me what happened", 3, &["an invitation to the room"]),
            // Everything at once, which is also the cap in practice.
            (
                "qwen, anyone know if the deploy is done?",
                12,
                &[
                    "question",
                    "asked of the room",
                    "an invitation to the room",
                    "my name",
                    "my subject",
                ],
            ),
        ];
        for (body, score, cues) in cases {
            let topics: &[&str] = if body.contains("deploy") {
                &["deploy"]
            } else {
                &[]
            };
            let found = scored(body, topics);
            assert_eq!(found.score, score, "{body:?} scored {}", found.listed());
            assert_eq!(found.cues, cues, "{body:?}");
        }
    }

    #[test]
    fn the_lines_that_hand_the_turn_to_the_room_and_the_ones_that_do_not() {
        // The first column is what an agent must conclude about a line nobody
        // addressed: was the ROOM asked for an answer? It selects nobody, so it
        // is never an address - it is the cue the judge was missing when it
        // read "you should just talk amongst yourselves" and answered "no, the
        // conversation has naturally settled".
        let invitations = [
            "you should just talk amongst yourselves",
            "you two, talk amongst yourselves about the weather",
            "so, you all - what now?",
            "y'all have been quiet",
            "does anyone have a view on this?",
            "everyone is welcome to weigh in",
            "any of you know why the build is red?",
            "talk to each other for a bit",
            "tell me what you think",
            "just discuss it between yourselves",
            "ok so talk about the weather",
            "introduce yourselves please",
            "keep going, this is interesting",
            "I have a question for the room",
        ];
        for body in invitations {
            assert!(
                addresses_room(body),
                "{body:?} is an invitation to the room"
            );
        }
        let not_invitations = [
            "what do you think?",
            "the build finished",
            "I talked to alex about it yesterday",
            "she told me the deploy was done",
            "everyones-bot is down",
            "I like this queue",
            "qwen, why so quiet?",
            "and why is that?",
        ];
        for body in not_invitations {
            assert!(
                !addresses_room(body),
                "{body:?} addresses nobody in particular, but it is not an invitation either"
            );
        }
    }

    #[test]
    fn an_invitation_that_asks_for_an_answer_and_one_that_only_mentions_the_room() {
        // The line the room log was written around, and the shape of every
        // other line an agent must answer without asking a model first: the
        // turn was thrown open AND somebody is waiting at the end of it.
        let asked = [
            "so, anyone here got an opinion on whether weekends should be three days long?",
            "does anyone have a view on this?",
            "any of you know why the build is red?",
            "what do you all think?",
            // The imperative forms carry no question mark and are still a
            // request put to whoever is listening.
            "tell me what you think",
            "you two, talk amongst yourselves about the weather",
            "ok so talk about the weather",
            "introduce yourselves please",
        ];
        for body in asked {
            assert!(addresses_room(body), "{body:?} is thrown at the room");
            assert!(
                invites_an_answer(body),
                "{body:?} asks the room for an answer"
            );
        }
        // Thrown at the room, but nobody is waiting: a statement about
        // everybody is not a turn handed to anybody.
        let unasked = [
            "everyone is welcome to weigh in",
            "y'all have been quiet",
            "someone has been busy in here",
            "I have a question for the room",
        ];
        for body in unasked {
            assert!(
                addresses_room(body),
                "{body:?} still mentions the room as a body"
            );
            assert!(
                !invites_an_answer(body),
                "{body:?} asks nobody for anything, so it is not an invitation to answer"
            );
        }
        // And a question that selects nobody and hands nothing to the room is
        // an ordinary tier-2 line: the judge still decides those.
        for body in [
            "what do you think?",
            "and why is that?",
            "is the build red?",
        ] {
            assert!(
                !invites_an_answer(body),
                "{body:?} is a question, but it was not handed to the room"
            );
        }
    }

    #[test]
    fn a_topic_is_a_word_and_not_a_substring() {
        assert_eq!(scored("the deploy is stuck", &["deploy"]).score, 2);
        assert_eq!(scored("the DEPLOY is stuck", &["Deploy"]).score, 2);
        assert_eq!(
            scored("the deployment is stuck", &["deploy"]).score,
            0,
            "a topic must not match inside a longer word"
        );
        assert_eq!(
            scored("ask deploy-bot about it", &["deploy"]).score,
            0,
            "the hyphen is a word character here too"
        );
        assert_eq!(
            scored("nothing at all", &["  "]).score,
            0,
            "a blank topic matches nothing (config refuses it as well)"
        );
    }

    #[test]
    fn a_room_with_no_names_still_scores_the_rest_of_the_line() {
        // Before the first sync nobody has a name. The question mark and the
        // second person are still free to read, and still worth reading.
        let nobody = Names::empty();
        let cues = Cues {
            names: &nobody,
            ..Cues::default()
        };
        let found = pre_score(&line("what do you think?"), &cues, &PolicyConfig::default());
        assert_eq!(found.score, 5);
        assert_eq!(found.listed(), "question, second person");
        // `prescore_fast: 0` puts a line with no cues at all on the fast path,
        // and the reason string it prints has to say something.
        assert_eq!(PreScore::default().listed(), "nothing in particular");
    }

    #[test]
    fn names_are_cleaned_deduplicated_and_ordered_longest_first() {
        let vocab = vocab(&["qwen", "  qwen  ", "QWEN", "q", "the long one", "bot-a"]);
        assert_eq!(vocab.names(), ["the long one", "bot-a", "qwen"]);
        assert!(
            Vocab::new(&["q".to_owned(), "ab".to_owned()]).is_none(),
            "nothing over the floor is nothing to compile"
        );
    }

    #[test]
    fn a_display_name_yields_itself_and_its_first_word() {
        assert_eq!(names_from_display("Alex Smith"), ["Alex Smith", "Alex"]);
        assert_eq!(names_from_display("Qwen"), ["Qwen"]);
        assert!(names_from_display("   ").is_empty());
        assert_eq!(
            names_for("@bot-a:example.com", Some("Agent A")),
            ["bot-a", "Agent A", "Agent"]
        );
        assert_eq!(names_for("@bot-a:example.com", None), ["bot-a"]);
    }

    #[test]
    fn a_hyphen_is_part_of_a_word_and_not_a_boundary() {
        // The one that would break the live gates: a member called "gate"
        // would otherwise be addressed by every line naming "gate-bot-a".
        let short = vocab(&["gate"]);
        assert!(vocative("gate-bot-a, why so quiet?", &short).is_none());
        assert!(named_bare("gate-bot-a, why so quiet?", &short).is_none());
        assert!(vocative("gate, why so quiet?", &short).is_some());
    }

    #[test]
    fn a_name_on_a_later_line_is_still_a_vocative() {
        let me = vocab(&["qwen"]);
        let body = "one more thing\nqwen, can you look at it?";
        assert_eq!(
            vocative(body, &me).map(|address| address.form),
            Some(Vocative::Leading)
        );
    }

    #[test]
    fn an_empty_name_set_addresses_nobody() {
        let nobody = Names::empty();
        assert!(nobody.addresses_me("qwen, hello", false).is_none());
        assert!(nobody.addresses_other("qwen, hello", false).is_none());
        assert!(nobody.mine().is_empty());
        assert!(nobody.theirs().is_empty());
    }
}
