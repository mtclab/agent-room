//! Per-room ledger: what I posted, what I consumed, and what the budgets allow.
//!
//! The ledger is the only place budgets are enforced. It is persisted as JSON
//! per room under `state_dir`, in the format the Python this was ported from
//! read and wrote, so a state directory either one left behind keeps every
//! promise it made (no double replies, no budget reset by kill -9). The clock
//! is injected so tests can drive it.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::error;

use crate::config::BudgetsConfig;
use crate::loops::{STATE_CLOSED, STATE_OPEN, STATE_RAISED};

pub const STATE_VERSION: u32 = 1;
pub const PAIR_WINDOW_S: f64 = 60.0;
pub const HOUR_WINDOW_S: f64 = 3600.0;
/// Posts kept for the sliding windows. Thread totals live in `thread_counts`,
/// which is never trimmed, so trimming here cannot loosen the per-thread cap.
pub const MAX_POSTS: usize = 2000;
/// Consumed event ids kept. Restart backlog is marked consumed wholesale, so
/// this only has to cover recent traffic.
pub const MAX_CONSUMED: usize = 10_000;
/// Loops kept on disk. Closed ones are only history.
pub const MAX_LOOPS: usize = 200;

/// A clock the caller owns, so a test can move time by hand.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// The wall clock, in epoch seconds.
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    })
}

/// Outcome of one budget question, with the reason for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetCheck {
    pub allowed: bool,
    pub reason: String,
}

impl BudgetCheck {
    fn yes(reason: String) -> Self {
        Self {
            allowed: true,
            reason,
        }
    }
    fn no(reason: String) -> Self {
        Self {
            allowed: false,
            reason,
        }
    }
}

/// One message I posted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub event_id: String,
    #[serde(default)]
    pub thread_root: String,
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub replied_to_sender: String,
    /// 1 = I was addressed, 2 = I spoke into an unaddressed line, 3 = unprompted.
    #[serde(default = "default_tier")]
    pub tier: u8,
}

fn default_tier() -> u8 {
    1
}

/// One thing I left open, and when it is worth coming back to it.
///
/// Persisted for the same reason the energy count is: a restart must not lose a
/// promise, and it must not hand a wound-down thread a fresh licence to nag
/// either. The state is a plain string because that is what is on disk, and an
/// unrecognised one reads as closed (see [`Loop::is_due`]): an unreadable loop
/// must never become a follow-up nobody can explain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Loop {
    #[serde(default)]
    pub event_id: String,
    #[serde(default)]
    pub thread_root: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub opened_ts: f64,
    #[serde(default)]
    pub due_ts: f64,
    #[serde(default = "default_loop_state")]
    pub state: String,
    #[serde(default)]
    pub reason: String,
}

fn default_loop_state() -> String {
    STATE_OPEN.to_owned()
}

impl Loop {
    /// Whether this loop's delay has passed and nobody has answered it.
    #[must_use]
    pub fn is_due(&self, now: f64) -> bool {
        self.state == STATE_OPEN && now >= self.due_ts
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state == STATE_CLOSED
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    posts: Vec<Post>,
    #[serde(default)]
    thread_counts: BTreeMap<String, u32>,
    #[serde(default)]
    thread_energy: BTreeMap<String, u32>,
    #[serde(default)]
    pair_cooldowns: BTreeMap<String, f64>,
    #[serde(default)]
    loops: Vec<Loop>,
    #[serde(default)]
    consumed: Vec<String>,
    #[serde(default)]
    counters: BTreeMap<String, i64>,
}

/// Budgets and consumed-event bookkeeping for one room.
pub struct Ledger {
    pub path: PathBuf,
    pub budgets: BudgetsConfig,
    clock: Clock,
    pub posts: Vec<Post>,
    pub thread_counts: BTreeMap<String, u32>,
    /// Consecutive bot-authored messages seen in a thread. Never trimmed, for
    /// the same reason `thread_counts` is not: forgetting it would hand a
    /// wound-down thread a fresh budget.
    pub thread_energy: BTreeMap<String, u32>,
    pub pair_cooldowns: BTreeMap<String, f64>,
    /// What I left open and have not come back to.
    pub loops: Vec<Loop>,
    pub counters: BTreeMap<String, i64>,
    consumed: Vec<String>,
    consumed_set: HashSet<String>,
}

impl Ledger {
    #[must_use]
    pub fn new(path: PathBuf, budgets: BudgetsConfig, clock: Clock) -> Self {
        Self {
            path,
            budgets,
            clock,
            posts: Vec::new(),
            thread_counts: BTreeMap::new(),
            thread_energy: BTreeMap::new(),
            pair_cooldowns: BTreeMap::new(),
            loops: Vec::new(),
            counters: BTreeMap::from([("posts".to_owned(), 0), ("consumed".to_owned(), 0)]),
            consumed: Vec::new(),
            consumed_set: HashSet::new(),
        }
    }

    /// Load the ledger at `path`, starting empty when it does not exist.
    ///
    /// An unreadable ledger is logged and replaced by an empty one rather than
    /// being fatal: a corrupt budget file must not keep an agent out of its
    /// room for ever.
    #[must_use]
    pub fn load(path: &Path, budgets: BudgetsConfig, clock: Clock) -> Self {
        let mut ledger = Self::new(path.to_path_buf(), budgets, clock);
        if !path.exists() {
            return ledger;
        }
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(exc) => {
                error!(
                    "ledger {} unreadable ({exc}); starting a fresh one",
                    path.display()
                );
                return ledger;
            }
        };
        let file: LedgerFile = match serde_json::from_str(&raw) {
            Ok(file) => file,
            Err(exc) => {
                error!(
                    "ledger {} unreadable ({exc}); starting a fresh one",
                    path.display()
                );
                return ledger;
            }
        };
        ledger.posts = file.posts;
        ledger.thread_counts = file.thread_counts;
        ledger.thread_energy = file.thread_energy;
        ledger.pair_cooldowns = file.pair_cooldowns;
        ledger.loops = file.loops;
        ledger.counters = file.counters;
        ledger.counters.entry("posts".to_owned()).or_insert(0);
        ledger.counters.entry("consumed".to_owned()).or_insert(0);
        ledger.consumed_set = file.consumed.iter().cloned().collect();
        ledger.consumed = file.consumed;
        ledger
    }

    #[must_use]
    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    /// Write the ledger atomically with mode 0600.
    ///
    /// A failure is logged, never raised: losing a turn because the budget file
    /// could not be written is worse than carrying on with the in-memory copy,
    /// and the next save will try again.
    pub fn save(&self) {
        if let Err(exc) = self.try_save() {
            error!("cannot write the ledger {}: {exc}", self.path.display());
        }
    }

    fn try_save(&self) -> std::io::Result<()> {
        let payload = LedgerFile {
            version: STATE_VERSION,
            posts: self.posts.clone(),
            thread_counts: self.thread_counts.clone(),
            thread_energy: self.thread_energy.clone(),
            pair_cooldowns: self.pair_cooldowns.clone(),
            loops: self.loops.clone(),
            consumed: self.consumed.clone(),
            counters: self.counters.clone(),
        };
        if let Some(parent) = self.path.parent()
            && !parent.is_dir()
        {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut handle = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let body = serde_json::to_vec(&payload)?;
        handle.write_all(&body)?;
        handle.flush()?;
        handle.sync_all()?;
        drop(handle);
        fs::rename(&tmp, &self.path)
    }

    // -- budgets ---------------------------------------------------------

    /// Whether I may answer `other` again: sliding minute, then a cooldown.
    #[must_use]
    pub fn pair_allows(&self, other: &str, now: f64) -> BudgetCheck {
        let cap = self.budgets.per_pair_per_minute;
        let recent = self
            .posts
            .iter()
            .filter(|p| p.replied_to_sender == other && now - p.ts < PAIR_WINDOW_S)
            .count();
        if recent >= cap as usize {
            return BudgetCheck::no(format!(
                "pair budget: {recent}/{cap} replies to {other} in the last 60 s"
            ));
        }
        if let Some(until) = self.pair_cooldowns.get(other)
            && *until > now
        {
            return BudgetCheck::no(format!(
                "pair cooldown: {:.0} s left before answering {other} again",
                until - now
            ));
        }
        BudgetCheck::yes(format!("pair budget ok ({recent}/{cap} to {other})"))
    }

    /// Whether I may post into `thread_root` again.
    #[must_use]
    pub fn thread_allows(&self, thread_root: &str) -> BudgetCheck {
        let cap = self.budgets.per_thread_max;
        let count = self.thread_counts.get(thread_root).copied().unwrap_or(0);
        if count >= cap {
            return BudgetCheck::no(format!(
                "thread budget: {count}/{cap} posts in {thread_root}"
            ));
        }
        BudgetCheck::yes(format!("thread budget ok ({count}/{cap})"))
    }

    /// Whether I am under my room-wide messages-per-hour cap.
    #[must_use]
    pub fn hour_allows(&self, now: f64) -> BudgetCheck {
        let cap = self.budgets.per_hour_max;
        let count = self
            .posts
            .iter()
            .filter(|p| now - p.ts < HOUR_WINDOW_S)
            .count();
        if count >= cap as usize {
            return BudgetCheck::no(format!("hour budget: {count}/{cap} posts in the last hour"));
        }
        BudgetCheck::yes(format!("hour budget ok ({count}/{cap})"))
    }

    /// Whether I may speak UNINVITED again this hour.
    ///
    /// Separate from [`Self::hour_allows`] on purpose: answering when addressed
    /// is the job and gets the wide cap; speaking unprompted is the luxury and
    /// gets the narrow one.
    #[must_use]
    pub fn tier2_hour_allows(&self, now: f64) -> BudgetCheck {
        let cap = self.budgets.tier2_per_hour_max;
        let count = self
            .posts
            .iter()
            .filter(|p| p.tier >= 2 && now - p.ts < HOUR_WINDOW_S)
            .count();
        if count >= cap as usize {
            return BudgetCheck::no(format!(
                "tier-2 budget: {count}/{cap} unprompted posts in the last hour"
            ));
        }
        BudgetCheck::yes(format!("tier-2 budget ok ({count}/{cap})"))
    }

    // -- conversation energy ---------------------------------------------

    /// Record who is keeping a thread alive.
    ///
    /// Every bot-authored message in a thread (mine included - I am a bot in
    /// this room too) adds one to its energy count; ANY human message resets it
    /// to nothing. That is the whole mechanism behind "the thread winds down
    /// like people running out of things to say".
    ///
    /// This does NOT write the file: every path that follows it ends in
    /// [`Self::mark_consumed`] or [`Self::record_post`], both of which save.
    pub fn note_event(&mut self, thread_root: &str, from_bot: bool) {
        if thread_root.is_empty() {
            return;
        }
        if from_bot {
            *self
                .thread_energy
                .entry(thread_root.to_owned())
                .or_insert(0) += 1;
        } else {
            self.thread_energy.remove(thread_root);
        }
    }

    #[must_use]
    pub fn bot_only_turns(&self, thread_root: &str) -> u32 {
        self.thread_energy.get(thread_root).copied().unwrap_or(0)
    }

    /// Whether this thread still has the energy for an unprompted reply.
    ///
    /// A refusal does not silence tier 1 outright: a human mention always gets
    /// an answer. It stops tier 2 in this thread, and it makes a bot's mention
    /// conditional on the judge - which is how two agents stop talking to each
    /// other without anyone telling them to.
    #[must_use]
    pub fn energy_allows(&self, thread_root: &str) -> BudgetCheck {
        let cap = self.budgets.bot_only_turns_before_decay;
        let count = self.bot_only_turns(thread_root);
        if count >= cap {
            return BudgetCheck::no(format!(
                "energy decay: {count}/{cap} bot-only turns in {thread_root}"
            ));
        }
        BudgetCheck::yes(format!("energy ok ({count}/{cap} bot-only turns)"))
    }

    // -- open loops ------------------------------------------------------

    /// Remember something I left open, and when to come back to it.
    pub fn open_loop(&mut self, event_id: &str, thread_root: &str, text: &str, due_ts: f64) {
        let opened_ts = self.now();
        self.loops.push(Loop {
            event_id: event_id.to_owned(),
            thread_root: thread_root.to_owned(),
            text: text.to_owned(),
            opened_ts,
            due_ts,
            state: STATE_OPEN.to_owned(),
            reason: String::new(),
        });
        self.trim_loops();
        self.save();
    }

    /// Loops whose delay has passed and which nobody has answered.
    #[must_use]
    pub fn due_loops(&self, now: f64) -> Vec<Loop> {
        self.loops
            .iter()
            .filter(|loop_| loop_.is_due(now))
            .cloned()
            .collect()
    }

    /// The loop opened by one of my posts, as it stands now.
    #[must_use]
    pub fn loop_by_event(&self, event_id: &str) -> Option<&Loop> {
        self.loops.iter().find(|loop_| loop_.event_id == event_id)
    }

    /// Mark a loop as taken up. It gets exactly one follow-up, ever.
    pub fn raise_loop(&mut self, event_id: &str) {
        if let Some(loop_) = self.loops.iter_mut().find(|l| l.event_id == event_id) {
            STATE_RAISED.clone_into(&mut loop_.state);
            self.save();
        }
    }

    /// Close one loop, with the reason anybody reading the ledger will see.
    pub fn close_loop(&mut self, event_id: &str, reason: &str) {
        if let Some(loop_) = self
            .loops
            .iter_mut()
            .find(|l| l.event_id == event_id && l.state != STATE_CLOSED)
        {
            STATE_CLOSED.clone_into(&mut loop_.state);
            reason.clone_into(&mut loop_.reason);
            self.save();
        }
    }

    /// Somebody came back to this thread, so I do not have to.
    ///
    /// Both open and already-raised loops close: if a follow-up is in flight
    /// while the answer arrives, the answer wins. Returns what was closed, so
    /// the caller can log it and drop any candidate waiting on it.
    pub fn close_loops_in_thread(&mut self, thread_root: &str, reason: &str) -> Vec<Loop> {
        let mut closed = Vec::new();
        for loop_ in &mut self.loops {
            if loop_.thread_root == thread_root && loop_.state != STATE_CLOSED {
                STATE_CLOSED.clone_into(&mut loop_.state);
                reason.clone_into(&mut loop_.reason);
                closed.push(loop_.clone());
            }
        }
        if !closed.is_empty() {
            self.save();
        }
        closed
    }

    /// Drop the oldest CLOSED loops until the list fits.
    ///
    /// Order is chronological and stays that way, and a loop that is still open
    /// is never dropped: forgetting a promise to save 200 bytes would be the one
    /// bug this whole feature exists to avoid.
    fn trim_loops(&mut self) {
        let mut excess = self.loops.len().saturating_sub(MAX_LOOPS);
        if excess == 0 {
            return;
        }
        self.loops.retain(|loop_| {
            if excess > 0 && loop_.is_closed() {
                excess -= 1;
                return false;
            }
            true
        });
    }

    // -- bookkeeping -----------------------------------------------------

    /// Record a message I posted and arm the pair cooldown if it hit the cap.
    pub fn record_post(
        &mut self,
        event_id: &str,
        thread_root: &str,
        replied_to_sender: &str,
        ts: Option<f64>,
        tier: u8,
    ) {
        let stamp = ts.unwrap_or_else(|| self.now());
        self.posts.push(Post {
            event_id: event_id.to_owned(),
            thread_root: thread_root.to_owned(),
            ts: stamp,
            replied_to_sender: replied_to_sender.to_owned(),
            tier,
        });
        if self.posts.len() > MAX_POSTS {
            let excess = self.posts.len() - MAX_POSTS;
            self.posts.drain(..excess);
        }
        *self
            .thread_counts
            .entry(thread_root.to_owned())
            .or_insert(0) += 1;
        *self.counters.entry("posts".to_owned()).or_insert(0) += 1;
        if !replied_to_sender.is_empty() {
            let recent = self
                .posts
                .iter()
                .filter(|p| {
                    p.replied_to_sender == replied_to_sender && stamp - p.ts < PAIR_WINDOW_S
                })
                .count();
            if recent >= self.budgets.per_pair_per_minute as usize {
                self.pair_cooldowns.insert(
                    replied_to_sender.to_owned(),
                    stamp + self.budgets.pair_cooldown_s,
                );
            }
            self.pair_cooldowns.retain(|_, until| *until > stamp);
        }
        self.save();
    }

    /// Remember that this event has been handled; never act on it again.
    pub fn mark_consumed(&mut self, event_id: &str) {
        if self.consumed_set.contains(event_id) {
            return;
        }
        self.consumed.push(event_id.to_owned());
        self.consumed_set.insert(event_id.to_owned());
        *self.counters.entry("consumed".to_owned()).or_insert(0) += 1;
        self.trim_consumed();
        self.save();
    }

    /// Mark a batch consumed with a single save. Returns how many were new.
    pub fn mark_many_consumed(&mut self, event_ids: &[String]) -> usize {
        let mut new = 0;
        for event_id in event_ids {
            if self.consumed_set.contains(event_id) {
                continue;
            }
            self.consumed.push(event_id.clone());
            self.consumed_set.insert(event_id.clone());
            *self.counters.entry("consumed".to_owned()).or_insert(0) += 1;
            new += 1;
        }
        self.trim_consumed();
        if new > 0 {
            self.save();
        }
        new
    }

    fn trim_consumed(&mut self) {
        if self.consumed.len() <= MAX_CONSUMED {
            return;
        }
        let excess = self.consumed.len() - MAX_CONSUMED;
        for dropped in self.consumed.drain(..excess) {
            self.consumed_set.remove(&dropped);
        }
    }

    #[must_use]
    pub fn is_consumed(&self, event_id: &str) -> bool {
        self.consumed_set.contains(event_id)
    }

    /// Whether `event_id` is one of my recorded posts.
    #[must_use]
    pub fn is_my_event(&self, event_id: Option<&str>) -> bool {
        let Some(event_id) = event_id else {
            return false;
        };
        self.posts.iter().any(|p| p.event_id == event_id)
    }

    /// Threads I have posted in (thread stickiness).
    #[must_use]
    pub fn thread_roots(&self) -> HashSet<String> {
        self.posts
            .iter()
            .filter(|p| !p.thread_root.is_empty())
            .map(|p| p.thread_root.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Mutex;

    const HUMAN: &str = "@human:example.com";
    const OTHER_BOT: &str = "@bot-b:example.com";

    /// A clock the tests move by hand.
    #[derive(Clone)]
    pub struct FakeClock(Arc<Mutex<f64>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(1_000_000.0)))
        }
        fn now(&self) -> f64 {
            *self.0.lock().expect("the clock is never poisoned")
        }
        fn advance(&self, seconds: f64) {
            *self.0.lock().expect("the clock is never poisoned") += seconds;
        }
        fn as_clock(&self) -> Clock {
            let inner = Arc::clone(&self.0);
            Arc::new(move || *inner.lock().expect("the clock is never poisoned"))
        }
    }

    fn ledger(dir: &Path, clock: &FakeClock, budgets: BudgetsConfig) -> Ledger {
        Ledger::load(&dir.join("ledger.json"), budgets, clock.as_clock())
    }

    #[test]
    fn the_pair_budget_allows_three_a_minute_then_cools_down() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock, BudgetsConfig::default());
        for index in 0..3 {
            assert!(led.pair_allows(OTHER_BOT, clock.now()).allowed);
            led.record_post(&format!("$mine{index}"), "$root", OTHER_BOT, None, 1);
            clock.advance(1.0);
        }
        let fourth = led.pair_allows(OTHER_BOT, clock.now());
        assert!(!fourth.allowed);
        assert!(fourth.reason.contains("pair budget"), "{}", fourth.reason);

        clock.advance(58.0);
        assert!(!led.pair_allows(OTHER_BOT, clock.now()).allowed);
        clock.advance(1.0);
        assert!(led.pair_allows(OTHER_BOT, clock.now()).allowed);
    }

    #[test]
    fn the_pair_cooldown_is_per_pair() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock, BudgetsConfig::default());
        for index in 0..3 {
            led.record_post(&format!("$mine{index}"), "$root", OTHER_BOT, None, 1);
        }
        assert!(!led.pair_allows(OTHER_BOT, clock.now()).allowed);
        assert!(led.pair_allows(HUMAN, clock.now()).allowed);
    }

    #[test]
    fn the_pair_cooldown_lasts_as_long_as_the_config_says() {
        // The cooldown and the three-a-minute rule are two different things:
        // the rule lets go after 60 s on its own, so a cooldown is only worth
        // configuring when it outlasts the window. This one does, and the
        // shipped 60 s would have let the pair start again at 61 s.
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let budgets = BudgetsConfig {
            pair_cooldown_s: 600.0,
            ..BudgetsConfig::default()
        };
        let mut led = ledger(dir.path(), &clock, budgets);
        for index in 0..3 {
            led.record_post(&format!("$mine{index}"), "$root", OTHER_BOT, None, 1);
        }
        clock.advance(61.0);
        let refused = led.pair_allows(OTHER_BOT, clock.now());
        assert!(
            !refused.allowed,
            "a 600 s cooldown was over after 61 s: it is not the config's"
        );
        assert!(
            refused.reason.contains("pair cooldown"),
            "the three-a-minute rule is what refused, not the cooldown: {}",
            refused.reason
        );
        clock.advance(600.0);
        assert!(led.pair_allows(OTHER_BOT, clock.now()).allowed);
    }

    #[test]
    fn the_thread_budget_counts_every_post_in_the_thread() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let budgets = BudgetsConfig {
            per_thread_max: 3,
            ..BudgetsConfig::default()
        };
        let mut led = ledger(dir.path(), &clock, budgets);
        for index in 0..3 {
            assert!(led.thread_allows("$root").allowed);
            led.record_post(&format!("$mine{index}"), "$root", HUMAN, None, 1);
            clock.advance(600.0);
        }
        let refused = led.thread_allows("$root");
        assert!(!refused.allowed);
        assert!(
            refused.reason.contains("thread budget: 3/3"),
            "{}",
            refused.reason
        );
        assert!(led.thread_allows("$other").allowed);
        clock.advance(86_400.0);
        assert!(
            !led.thread_allows("$root").allowed,
            "time never heals a thread cap"
        );
    }

    #[test]
    fn the_hour_budget_is_a_sliding_window() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let budgets = BudgetsConfig {
            per_hour_max: 2,
            ..BudgetsConfig::default()
        };
        let mut led = ledger(dir.path(), &clock, budgets);
        led.record_post("$a", "$t1", HUMAN, None, 1);
        clock.advance(10.0);
        led.record_post("$b", "$t2", HUMAN, None, 1);
        let refused = led.hour_allows(clock.now());
        assert!(!refused.allowed);
        assert!(
            refused.reason.contains("hour budget: 2/2"),
            "{}",
            refused.reason
        );
        clock.advance(3600.0);
        assert!(led.hour_allows(clock.now()).allowed);
    }

    #[test]
    fn consumed_and_posts_survive_a_reload() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let path = dir.path().join("state").join("ledger.json");
        let mut led = Ledger::load(&path, BudgetsConfig::default(), clock.as_clock());
        led.mark_consumed("$trigger");
        led.record_post("$mine", "$root", HUMAN, None, 1);
        assert_eq!(led.counters.get("posts"), Some(&1));
        assert_eq!(led.counters.get("consumed"), Some(&1));

        let reloaded = Ledger::load(&path, BudgetsConfig::default(), clock.as_clock());
        assert!(reloaded.is_consumed("$trigger"));
        assert!(!reloaded.is_consumed("$never-seen"));
        assert!(reloaded.is_my_event(Some("$mine")));
        assert!(!reloaded.is_my_event(Some("$someone-elses")));
        assert!(!reloaded.is_my_event(None));
        assert_eq!(reloaded.thread_roots(), HashSet::from(["$root".to_owned()]));
        assert_eq!(reloaded.counters.get("posts"), Some(&1));
    }

    #[test]
    fn the_pair_cooldown_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let path = dir.path().join("ledger.json");
        let mut led = Ledger::load(&path, BudgetsConfig::default(), clock.as_clock());
        for index in 0..3 {
            led.record_post(&format!("$mine{index}"), "$root", OTHER_BOT, None, 1);
        }
        let restarted = Ledger::load(&path, BudgetsConfig::default(), clock.as_clock());
        assert!(!restarted.pair_allows(OTHER_BOT, clock.now()).allowed);
        clock.advance(61.0);
        assert!(restarted.pair_allows(OTHER_BOT, clock.now()).allowed);
    }

    #[test]
    fn marking_a_batch_consumed_is_idempotent() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock, BudgetsConfig::default());
        let batch = ["$a".to_owned(), "$b".to_owned(), "$a".to_owned()];
        assert_eq!(led.mark_many_consumed(&batch), 2);
        assert_eq!(led.mark_many_consumed(&batch), 0);
        assert!(led.is_consumed("$a") && led.is_consumed("$b"));
    }

    #[test]
    fn the_state_file_is_written_0600() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock, BudgetsConfig::default());
        led.mark_consumed("$a");
        let mode = fs::metadata(&led.path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_corrupt_state_file_starts_fresh_instead_of_crashing() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let path = dir.path().join("ledger.json");
        fs::write(&path, "{not json").expect("write");
        let led = Ledger::load(&path, BudgetsConfig::default(), clock.as_clock());
        assert!(led.posts.is_empty());
        assert!(!led.is_consumed("$anything"));
    }

    #[test]
    fn the_tier2_budget_only_counts_unprompted_posts() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let budgets = BudgetsConfig {
            tier2_per_hour_max: 2,
            per_hour_max: 30,
            ..BudgetsConfig::default()
        };
        let mut led = ledger(dir.path(), &clock, budgets);
        for index in 0..10 {
            led.record_post(&format!("$asked{index}"), "$t", HUMAN, None, 1);
            clock.advance(1.0);
        }
        assert!(led.tier2_hour_allows(clock.now()).allowed);

        led.record_post("$unprompted1", "$t1", HUMAN, None, 2);
        led.record_post("$heartbeat", "$t2", "", None, 3);
        let refused = led.tier2_hour_allows(clock.now());
        assert!(!refused.allowed);
        assert!(
            refused.reason.contains("tier-2 budget: 2/2"),
            "{}",
            refused.reason
        );
        assert!(led.hour_allows(clock.now()).allowed);
        clock.advance(3601.0);
        assert!(led.tier2_hour_allows(clock.now()).allowed);
    }

    #[test]
    fn the_tier_of_a_post_survives_a_reload() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let path = dir.path().join("ledger.json");
        let budgets = BudgetsConfig {
            tier2_per_hour_max: 1,
            ..BudgetsConfig::default()
        };
        let mut led = Ledger::load(&path, budgets.clone(), clock.as_clock());
        led.record_post("$unprompted", "$t", HUMAN, None, 2);
        let restarted = Ledger::load(&path, budgets, clock.as_clock());
        assert!(
            !restarted.tier2_hour_allows(clock.now()).allowed,
            "a restart handed the agent a fresh licence to speak uninvited"
        );
    }

    #[test]
    fn bot_only_turns_accumulate_and_a_human_resets_them() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let budgets = BudgetsConfig {
            bot_only_turns_before_decay: 4,
            ..BudgetsConfig::default()
        };
        let mut led = ledger(dir.path(), &clock, budgets);
        for turn in 1..4 {
            led.note_event("$root", true);
            clock.advance(5.0);
            assert_eq!(led.bot_only_turns("$root"), turn);
            assert!(
                led.energy_allows("$root").allowed,
                "turn {turn} must still pass"
            );
        }
        led.note_event("$root", true);
        let spent = led.energy_allows("$root");
        assert!(!spent.allowed);
        assert!(
            spent.reason.contains("energy decay: 4/4 bot-only turns"),
            "{}",
            spent.reason
        );
        assert!(led.energy_allows("$other").allowed);
        clock.advance(86_400.0);
        assert!(
            !led.energy_allows("$root").allowed,
            "time alone never refills it"
        );
        led.note_event("$root", false);
        assert_eq!(led.bot_only_turns("$root"), 0);
        assert!(led.energy_allows("$root").allowed);
    }

    #[test]
    fn the_energy_count_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let path = dir.path().join("ledger.json");
        let budgets = BudgetsConfig {
            bot_only_turns_before_decay: 2,
            ..BudgetsConfig::default()
        };
        let mut led = Ledger::load(&path, budgets.clone(), clock.as_clock());
        led.note_event("$root", true);
        led.note_event("$root", true);
        // `note_event` deliberately does not fsync per message; the save that
        // follows it in every connector path is what puts it on disk.
        led.mark_consumed("$whatever-came-with-it");

        let restarted = Ledger::load(&path, budgets, clock.as_clock());
        assert_eq!(restarted.bot_only_turns("$root"), 2);
        assert!(!restarted.energy_allows("$root").allowed);
    }

    #[test]
    fn an_event_outside_any_thread_is_not_counted() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock, BudgetsConfig::default());
        led.note_event("", true);
        assert!(led.thread_energy.is_empty());
    }

    #[test]
    fn a_loop_is_due_once_its_delay_has_passed_and_never_after_it_closes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut ledger = ledger(dir.path(), &clock, BudgetsConfig::default());
        ledger.open_loop("$mine", "$root", "did anyone try it?", clock.now() + 100.0);

        assert!(ledger.due_loops(clock.now()).is_empty());
        clock.advance(101.0);
        let due = ledger.due_loops(clock.now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].text, "did anyone try it?");

        // Raised is not open: one follow-up ever, whatever the judge says.
        ledger.raise_loop("$mine");
        assert!(ledger.due_loops(clock.now()).is_empty());
        ledger.close_loop("$mine", "followed up");
        assert_eq!(
            ledger.loop_by_event("$mine").map(|l| l.reason.clone()),
            Some("followed up".to_owned())
        );
    }

    #[test]
    fn anybody_posting_in_the_thread_closes_what_i_left_open_there() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut ledger = ledger(dir.path(), &clock, BudgetsConfig::default());
        ledger.open_loop("$mine", "$root", "did anyone try it?", clock.now());
        ledger.open_loop("$other", "$elsewhere", "and the deploy?", clock.now());

        let closed =
            ledger.close_loops_in_thread("$root", "@human:example.com posted in the thread");
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].event_id, "$mine");
        assert!(ledger.loop_by_event("$mine").expect("the loop").is_closed());
        assert!(
            !ledger
                .loop_by_event("$other")
                .expect("the loop")
                .is_closed()
        );
        // Nothing to close twice.
        assert!(ledger.close_loops_in_thread("$root", "again").is_empty());
    }

    #[test]
    fn loops_survive_a_restart_because_a_promise_has_to() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        {
            let mut ledger = ledger(dir.path(), &clock, BudgetsConfig::default());
            ledger.open_loop("$mine", "$root", "did anyone try it?", 42.0);
            ledger.raise_loop("$mine");
        }
        let reloaded = ledger(dir.path(), &clock, BudgetsConfig::default());
        let loop_ = reloaded.loop_by_event("$mine").expect("the loop came back");
        assert_eq!(loop_.state, "raised");
        assert!((loop_.due_ts - 42.0).abs() < f64::EPSILON);
        assert_eq!(loop_.thread_root, "$root");
    }

    #[test]
    fn trimming_drops_closed_loops_and_never_an_open_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let clock = FakeClock::new();
        let mut ledger = ledger(dir.path(), &clock, BudgetsConfig::default());
        for index in 0..MAX_LOOPS {
            ledger.open_loop(&format!("$closed{index}"), "$root", "old", 0.0);
        }
        ledger.close_loops_in_thread("$root", "answered");
        ledger.open_loop("$open", "$other", "still waiting", 0.0);
        assert_eq!(ledger.loops.len(), MAX_LOOPS);
        assert!(
            ledger.loop_by_event("$open").is_some(),
            "an open loop was thrown away to make room"
        );
        assert!(ledger.loop_by_event("$closed0").is_none());
    }

    #[test]
    fn the_injected_clock_is_the_one_that_stamps_a_post() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let clock = FakeClock::new();
        let mut led = ledger(dir.path(), &clock, BudgetsConfig::default());
        clock.advance(500.0);
        led.record_post("$mine", "$root", HUMAN, None, 1);
        let stamped = led.posts.first().expect("one post").ts;
        assert!((stamped - clock.now()).abs() < f64::EPSILON);
        let _unused: Cell<u8> = Cell::new(0);
    }
}
