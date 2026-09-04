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

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::events::localpart;

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
