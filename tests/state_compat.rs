//! A binary started on the Python implementation's `state_dir` must pick up
//! where it left off.
//!
//! The fixtures in `tests/fixtures/` were WRITTEN by the Python implementation
//! that this build was ported from (removed in R5, last shipped as part of
//! 1.0.0-rc.1); they are not hand-typed JSON that happens to look right. Only
//! the account localparts in them were rewritten for publication - every key,
//! nesting and value shape is still exactly what the Python wrote. Every
//! promise the state file makes is asserted here, and the round trip goes back
//! the other way too: what this build writes has to be the same shape, key for
//! key, or a swap in either direction loses a budget, a consumed event or a
//! promise.
//!
//! Keep the fixtures. They are the only remaining record of the file format
//! somebody else's state directory may still be in.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_room::config::BudgetsConfig;
use agent_room::ledger::Ledger;
use agent_room::transcript::{Kind, Transcript};
use serde_json::Value;

/// The instant the fixtures were written at, so the sliding windows land where
/// the Python's did.
const FIXTURE_NOW: f64 = 1_700_000_500.0;
const HUMAN: &str = "@human:example.com";
const OTHER_BOT: &str = "@bot-b:example.com";
const ME: &str = "@bot-a:example.com";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copy a fixture into a scratch directory: the tests write to it.
fn scratch(dir: &Path, name: &str, to: &str) -> PathBuf {
    let path = dir.join(to);
    fs::copy(fixture(name), &path).expect("the fixture is readable");
    path
}

fn clock() -> Arc<dyn Fn() -> f64 + Send + Sync> {
    Arc::new(|| FIXTURE_NOW)
}

#[test]
fn a_python_ledger_is_read_and_every_budget_it_paid_for_still_counts() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = scratch(dir.path(), "python.ledger.json", "ledger.json");
    let ledger = Ledger::load(&path, BudgetsConfig::default(), clock());

    // What it consumed stays consumed: this is what stops a restart answering
    // the same message twice.
    assert!(ledger.is_consumed("$backlog1"));
    assert!(ledger.is_consumed("$backlog2"));
    assert!(ledger.is_consumed("$trigger"));
    assert!(!ledger.is_consumed("$never-seen"));

    // What it posted is still mine, so a reply to it is still tier 1.
    assert!(ledger.is_my_event(Some("$mine1")));
    assert!(!ledger.is_my_event(Some("$somebody-elses")));
    assert_eq!(
        ledger.thread_roots(),
        std::collections::HashSet::from(["$root".to_owned()])
    );

    // The budgets: two posts in the hour, one of them uninvited, two in the
    // thread, and the thread's energy at 2 bot-only turns.
    let hour = ledger.hour_allows(FIXTURE_NOW);
    assert!(hour.reason.contains("2/30"), "{}", hour.reason);
    let tier2 = ledger.tier2_hour_allows(FIXTURE_NOW);
    assert!(tier2.reason.contains("1/10"), "{}", tier2.reason);
    let thread = ledger.thread_allows("$root");
    assert!(thread.reason.contains("2/12"), "{}", thread.reason);
    assert_eq!(ledger.bot_only_turns("$root"), 2);
    assert_eq!(ledger.counters.get("posts"), Some(&2));
    assert_eq!(ledger.counters.get("consumed"), Some(&3));

    // And the pair window is measured against the timestamps the Python wrote,
    // not against "now": 30 s after that reply it still counts, 300 s later it
    // has slid out of the minute.
    let inside = ledger.pair_allows(OTHER_BOT, 1_700_000_230.0);
    assert!(inside.reason.contains("1/3"), "{}", inside.reason);
    let outside = ledger.pair_allows(OTHER_BOT, FIXTURE_NOW);
    assert!(outside.reason.contains("0/3"), "{}", outside.reason);
}

#[test]
fn a_promise_the_python_made_is_not_dropped_when_this_build_writes_the_file() {
    // R1 does not raise follow-ups, but it must not forget one either: an agent
    // that comes back after an upgrade having quietly lost what it said it
    // would do is worse than one that never promised.
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = scratch(dir.path(), "python.ledger.json", "ledger.json");
    let mut ledger = Ledger::load(&path, BudgetsConfig::default(), clock());
    assert_eq!(ledger.loops.len(), 1);
    assert_eq!(ledger.loops[0].text, "check the deploy log");
    assert_eq!(ledger.loops[0].state, "open");

    ledger.mark_consumed("$something-new");
    let written: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("written")).expect("valid JSON");
    assert_eq!(written["loops"][0]["text"], "check the deploy log");
    assert_eq!(written["loops"][0]["state"], "open");
    assert_eq!(written["loops"][0]["due_ts"], 1_700_003_000.0);
}

#[test]
fn what_this_build_writes_has_the_shape_the_python_reads() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = scratch(dir.path(), "python.ledger.json", "ledger.json");
    let before: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("readable")).expect("valid JSON");

    let mut ledger = Ledger::load(&path, BudgetsConfig::default(), clock());
    ledger.record_post("$mine3", "$root", HUMAN, None, 1);
    let after: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("written")).expect("valid JSON");

    let keys = |value: &Value| {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect::<Vec<String>>()
    };
    assert_eq!(
        keys(&before),
        keys(&after),
        "this build wrote a ledger with different top-level keys"
    );
    assert_eq!(after["version"], 1);
    assert_eq!(keys(&before["posts"][0]), keys(&after["posts"][0]));
    assert_eq!(keys(&before["loops"][0]), keys(&after["loops"][0]));
    assert_eq!(after["posts"].as_array().expect("posts").len(), 3);
    assert_eq!(after["counters"]["posts"], 3);
    // Types matter as much as names: a ts that came back as a string, or a tier
    // as a float, would parse here and blow up in the reference.
    assert!(after["posts"][2]["ts"].is_f64());
    assert!(after["posts"][2]["tier"].is_i64());
    assert!(after["consumed"].is_array());
}

#[test]
fn a_python_transcript_reads_back_as_the_same_conversation() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = scratch(dir.path(), "python.transcript.jsonl", "room.jsonl");
    let transcript = Transcript::new(path);

    let records = transcript.tail_records(10);
    assert_eq!(
        records.iter().map(|(kind, _)| *kind).collect::<Vec<Kind>>(),
        [Kind::Seen, Kind::Seen, Kind::Reply]
    );

    let events = transcript.recent(10);
    assert_eq!(
        events
            .iter()
            .map(|e| e.event_id.as_str())
            .collect::<Vec<_>>(),
        ["$backlog1", "$trigger", "$mine1"]
    );

    let human = &events[0];
    assert_eq!(human.sender, HUMAN);
    assert_eq!(
        human.display(),
        "Alex",
        "the display name survives the round trip"
    );
    assert_eq!(human.body, "hello there");
    assert!(!human.is_bot);

    let trigger = &events[1];
    assert!(
        trigger.mentions.contains(ME),
        "the mention that made this a tier-1 trigger was lost"
    );

    let mine = &events[2];
    assert!(mine.is_bot);
    assert_eq!(mine.msgtype, "m.notice");
    assert_eq!(mine.thread_root.as_deref(), Some("$root"));
    assert_eq!(mine.reply_to.as_deref(), Some("$trigger"));
    assert!(
        mine.reply_is_fallback,
        "a threaded reply must not read back as a rich reply, or every other \
         connector reads it as \"you replied to me\""
    );
    assert_eq!(transcript.thread("$root").len(), 1);
}

#[test]
fn a_line_this_build_appends_is_a_line_the_python_would_read() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = scratch(dir.path(), "python.transcript.jsonl", "room.jsonl");
    let transcript = Transcript::new(path.clone());
    let before = fs::read_to_string(&path).expect("readable");
    let python_keys: Vec<String> = {
        let first: Value =
            serde_json::from_str(before.lines().next().expect("the fixture has a line"))
                .expect("valid JSON");
        first["event"]
            .as_object()
            .expect("an event object")
            .keys()
            .cloned()
            .collect()
    };

    let mut appended = transcript.recent(10).pop().expect("something to copy");
    appended.event_id = "$mine2".to_owned();
    transcript.append_reply(&appended);

    let after = fs::read_to_string(&path).expect("written");
    let last: Value =
        serde_json::from_str(after.lines().last().expect("a line")).expect("valid JSON");
    assert_eq!(last["kind"], "reply");
    let mine_keys: Vec<String> = last["event"]
        .as_object()
        .expect("an event object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        python_keys, mine_keys,
        "this build wrote a transcript event with different keys"
    );
    // `mentions` is a sorted list on disk, never a set literal or a map.
    assert!(last["event"]["mentions"].is_array());
    assert!(after.starts_with(&before), "the transcript is append-only");
}

#[test]
fn the_state_layout_is_the_one_the_python_uses() {
    // Not a formality: a Rust binary pointed at a Python state_dir has to find
    // the same files, or it starts with an empty ledger and answers the backlog.
    let state = Path::new("/tmp/agent-room-state");
    assert_eq!(
        agent_room::config::room_state_path(state, "!abc:example.com", ".ledger.json"),
        state.join("rooms/_abc_example.com.ledger.json")
    );
    assert_eq!(
        agent_room::config::room_state_path(state, "!abc:example.com", ".jsonl"),
        state.join("rooms/_abc_example.com.jsonl")
    );
}

/// Every fixture these gates read has to be IN the repository.
///
/// `python.transcript.jsonl` was not, for four slices: the `.gitignore`'s
/// blanket `*.jsonl` (there to keep transcripts out) swallowed it, so a fresh
/// clone failed two of the tests above with "the fixture is readable". Nothing
/// noticed, because nobody cloned. `git ls-files` is the only thing that can
/// tell "on my disk" from "in the repo".
#[test]
fn every_fixture_the_gates_read_is_committed() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let listed = match std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--", "tests/fixtures"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        // No git, or not a checkout: nothing to say, rather than a false alarm.
        _ => return,
    };
    let tracked: BTreeSet<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for entry in fs::read_dir(repo.join("tests/fixtures")).expect("the fixture directory exists") {
        let name = entry.expect("readable").file_name();
        let path = format!("tests/fixtures/{}", name.to_string_lossy());
        assert!(
            tracked.contains(path.as_str()),
            "{path} is on disk but not in the repository - check .gitignore"
        );
    }
}

#[test]
fn the_fixtures_really_came_from_the_reference() {
    // A guard on the guard: if somebody regenerates these by hand and drops a
    // field, the tests above would still pass while proving nothing.
    let ledger: Value =
        serde_json::from_str(&fs::read_to_string(fixture("python.ledger.json")).expect("readable"))
            .expect("valid JSON");
    let expected = BTreeSet::from([
        "version",
        "posts",
        "thread_counts",
        "thread_energy",
        "pair_cooldowns",
        "loops",
        "consumed",
        "counters",
    ]);
    let found: BTreeSet<&str> = ledger
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        expected, found,
        "the fixture is not the file the reference writes"
    );
}
