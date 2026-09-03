//! Who is actually in the room: Matrix presence, kept for the humans.
//!
//! The third reason people speak unprompted, and the one that stops the other
//! two being annoying: a person says the thing that happened to them *to
//! somebody*. Nobody walks into an empty office and announces that the build
//! went green.
//!
//! Matrix gives us `m.presence` for everyone we share a room with (`online`,
//! `unavailable`, `offline`), pushed in `/sync` like everything else. It is a
//! hint, not a fact - a phone that went to sleep says `offline` while its owner
//! is reading over someone's shoulder - so the connector treats "a human posted
//! here in the last `presence_window_min`" as presence too, and this book only
//! holds the half that comes from the homeserver.
//!
//! Deliberately NOT here: typing. A typing notice means somebody is about to
//! speak, which is a reason to warm a model up, not a reason to interrupt them.

use std::collections::BTreeMap;

use tracing::debug;

/// The `m.presence` state that counts as "in the room". `unavailable` is the
/// client saying its user went idle, which is exactly when not to interrupt.
pub const ONLINE: &str = "online";
/// What we report about somebody the homeserver has never mentioned.
pub const UNKNOWN: &str = "unknown";

/// The last `m.presence` state seen for each user, room-independent.
///
/// Presence in Matrix is a property of a USER, not of a room: one event covers
/// every room we share with them. Which of those users matter is the caller's
/// question, so this holds the states and answers about a given list.
#[derive(Debug, Default)]
pub struct PresenceBook {
    states: BTreeMap<String, String>,
}

impl PresenceBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn note(&mut self, user_id: &str, state: &str) {
        let previous = self.states.insert(user_id.to_owned(), state.to_owned());
        if previous.as_deref() != Some(state) {
            debug!(
                "presence: {user_id} is {state} (was {})",
                previous.unwrap_or_else(|| UNKNOWN.to_owned())
            );
        }
    }

    /// The last state seen, or `unknown` if the homeserver never said.
    #[must_use]
    pub fn state_of(&self, user_id: &str) -> &str {
        self.states.get(user_id).map_or(UNKNOWN, String::as_str)
    }

    /// The first of these users the homeserver calls online, if any.
    #[must_use]
    pub fn online_among<'a, I>(&self, user_ids: I) -> Option<&'a str>
    where
        I: IntoIterator<Item = &'a str>,
    {
        user_ids
            .into_iter()
            .find(|user_id| self.state_of(user_id) == ONLINE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUMAN: &str = "@human:example.com";
    const ANNA: &str = "@anna:example.com";

    #[test]
    fn the_book_remembers_the_last_state_it_was_told() {
        let mut book = PresenceBook::new();
        assert_eq!(book.state_of(HUMAN), UNKNOWN);
        book.note(HUMAN, "online");
        assert_eq!(book.state_of(HUMAN), ONLINE);
        book.note(HUMAN, "offline");
        assert_eq!(book.state_of(HUMAN), "offline");
    }

    #[test]
    fn only_online_counts_as_being_here() {
        // `unavailable` is a client saying its user went idle, which is exactly
        // when not to interrupt.
        let mut book = PresenceBook::new();
        book.note(HUMAN, "unavailable");
        book.note(ANNA, "offline");
        assert_eq!(book.online_among([HUMAN, ANNA]), None);
        book.note(ANNA, "online");
        assert_eq!(book.online_among([HUMAN, ANNA]), Some(ANNA));
    }

    #[test]
    fn a_user_nobody_asked_about_never_makes_the_room_look_busy() {
        let mut book = PresenceBook::new();
        book.note("@someone-else:example.com", "online");
        assert_eq!(book.online_among([HUMAN, ANNA]), None);
    }
}
