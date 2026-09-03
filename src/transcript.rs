//! Append-only per-room transcript (JSONL).
//!
//! Every event we see and every reply we make is appended here. This file is
//! the session memory a brain gets handed: it survives restarts and needs no
//! server round-trip to read back. The format is the Python's, line
//! for line, so either implementation can read what the other wrote.
//!
//! It is bounded. When an append leaves the live file holding more than
//! `transcript_keep` lines it rolls: `<room>.jsonl` becomes `<room>.jsonl.1`
//! (the archives already there shift down one and the oldest falls off the
//! end), and a fresh live file takes its place, seeded with the newest half of
//! what was rolled away so the agent does not lose its memory of the last few
//! hundred messages at the moment the file turns over. The line format does not
//! change: an archive is a transcript, and anything that reads one reads it.
//!
//! `recent()` and `thread()` read the LIVE FILE ONLY, which is the point of the
//! cap - what an append or a turn costs is bounded by `transcript_keep` rather
//! than by how long the room has existed. The archives are history for a human
//! with `jq`, not memory for the brain.

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::events::RoomEvent;

const BLOCK: usize = 64 * 1024;
/// How many records to scan back when collecting one thread.
pub const DEFAULT_THREAD_SCAN: usize = 2000;
/// Lines the live file keeps before it rolls: `transcript_keep`'s default.
///
/// 5000 events is about 3 MB at the 630 bytes a message measured on
/// 1.0.0-rc.1, and comfortably more than the 2000 lines `thread()` scans or the
/// 40 `recent()` hands the brain.
pub const DEFAULT_KEEP: usize = 5000;
/// How many `<room>.jsonl.N` archives are kept: `transcript_archives`'s
/// default. Four of them plus the live file is about a year of a busy room.
pub const DEFAULT_ARCHIVES: usize = 4;

/// The two points inside a roll a test can stop it at. Only the crash gate ever
/// arms one; in the shipped binary `crash_point` has no state behind it.
const AFTER_SEED: &str = "after the seed file was written";
const AFTER_ARCHIVE: &str = "after the live file was rolled into .1";

/// What a record is: something the room said, or something I posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Seen,
    Reply,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Seen => "seen",
            Self::Reply => "reply",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    kind: String,
    event: RoomEvent,
}

/// Append-only JSONL log of one room, rolled when it gets long.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub path: PathBuf,
    /// Lines the live file may hold before it rolls. 0 = never roll.
    keep: usize,
    /// How many `<room>.jsonl.N` archives to keep. 0 = keep none.
    archives: usize,
    /// Lines in the live file: counted from disk once, then kept in step, so an
    /// append never re-reads the whole file to find out whether it is time to
    /// roll. `None` = not counted yet, or a roll that failed left it uncertain.
    lines: Arc<Mutex<Option<usize>>>,
}

impl Transcript {
    /// One room's transcript, rolled at the shipped defaults.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self::with_rotation(path, DEFAULT_KEEP, DEFAULT_ARCHIVES)
    }

    /// One room's transcript, rolled the way the config asks.
    ///
    /// `keep` of 0 turns rolling off altogether - the file then grows for ever,
    /// which is what every version before this one did.
    #[must_use]
    pub fn with_rotation(path: PathBuf, keep: usize, archives: usize) -> Self {
        Self {
            path,
            keep,
            archives,
            lines: Arc::new(Mutex::new(None)),
        }
    }

    /// Append one record. Creates the file (0600) and its parents (0700).
    ///
    /// A failure is logged rather than raised: the transcript is memory, and
    /// losing a line of it must not lose the turn that produced it.
    pub fn append(&self, kind: Kind, ev: &RoomEvent) {
        if let Err(exc) = self.try_append(kind, ev) {
            warn!(
                "cannot append to the transcript {}: {exc}",
                self.path.display()
            );
            return;
        }
        // Reported on its own line, and after the fact: the record is on disk by
        // now, so a roll that failed costs disk space, never the turn.
        if let Err(exc) = self.roll_if_full() {
            warn!("cannot roll the transcript {}: {exc}", self.path.display());
        }
    }

    fn try_append(&self, kind: Kind, ev: &RoomEvent) -> std::io::Result<()> {
        let record = Record {
            kind: kind.as_str().to_owned(),
            event: ev.clone(),
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        if let Some(parent) = self.path.parent()
            && !parent.is_dir()
        {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut handle = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        handle.write_all(line.as_bytes())
    }

    // -- rolling ---------------------------------------------------------

    /// Count the line just appended, and roll the file when it is over the cap.
    ///
    /// The count is cached rather than recomputed: at the default cap the file
    /// is a few megabytes, and reading all of it to answer "is it full yet?"
    /// on every message in the room would be the most expensive thing an append
    /// does. It is counted once per process per room, then kept in step.
    fn roll_if_full(&self) -> std::io::Result<()> {
        if self.keep == 0 {
            return Ok(());
        }
        let mut held = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let count = match *held {
            Some(known) => known + 1,
            None => count_lines(&self.path)?,
        };
        *held = Some(count);
        if count <= self.keep {
            return Ok(());
        }
        // Between here and the end of the roll the file's length is anybody's
        // guess, so the cache says so: a roll that fails is recounted, not
        // guessed at.
        *held = None;
        *held = Some(self.rotate(count)?);
        Ok(())
    }

    /// Roll the live file away and start a fresh one, seeded with the newest
    /// `keep / 2` events. Returns how many lines the new live file holds.
    ///
    /// The ORDER is what makes an interrupted roll survivable. The seed is
    /// written to a temporary name first, so the only moment `<room>.jsonl`
    /// does not exist is between two renames: a crash before that leaves the
    /// old file intact, a crash after it leaves the new one complete, and a
    /// crash inside it leaves every line in `<room>.jsonl.1` with the next
    /// append making a new live file. Nothing is ever written into the live
    /// file's own name, so no reader ever meets a half-written transcript.
    fn rotate(&self, held: usize) -> std::io::Result<usize> {
        let seed = self.tail_lines(self.keep / 2);
        let staged = self.sibling(".rolling");
        write_seed(&staged, &seed)?;
        crash_point(AFTER_SEED)?;
        self.shift_archives()?;
        if self.archives == 0 {
            fs::remove_file(&self.path)?;
        } else {
            fs::rename(&self.path, self.archive(1))?;
        }
        crash_point(AFTER_ARCHIVE)?;
        fs::rename(&staged, &self.path)?;
        let archived = if self.archives == 0 {
            "dropped (transcript_archives is 0)".to_owned()
        } else {
            self.archive(1).display().to_string()
        };
        info!(
            "rolled the transcript {}: {held} events over the {} cap, archived to {archived}, \
             {} kept live",
            self.path.display(),
            self.keep,
            seed.len(),
        );
        Ok(seed.len())
    }

    /// `.1` -> `.2` ... up to `transcript_archives`; the oldest is deleted.
    fn shift_archives(&self) -> std::io::Result<()> {
        if self.archives == 0 {
            return Ok(());
        }
        if let Err(exc) = fs::remove_file(self.archive(self.archives))
            && exc.kind() != std::io::ErrorKind::NotFound
        {
            return Err(exc);
        }
        for index in (1..self.archives).rev() {
            let older = self.archive(index);
            if older.exists() {
                fs::rename(&older, self.archive(index + 1))?;
            }
        }
        Ok(())
    }

    /// `<room>.jsonl.<index>`.
    fn archive(&self, index: usize) -> PathBuf {
        self.sibling(&format!(".{index}"))
    }

    /// The live file's path with `suffix` stuck on the end.
    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    pub fn append_seen(&self, ev: &RoomEvent) {
        self.append(Kind::Seen, ev);
    }

    pub fn append_reply(&self, ev: &RoomEvent) {
        self.append(Kind::Reply, ev);
    }

    // -- reading back ----------------------------------------------------
    //
    // All of it reads the LIVE file and nothing else. What has rolled into
    // `<room>.jsonl.1` and beyond is there for a person reading back over the
    // room's history, and is deliberately invisible to the agent: bounded
    // memory is the whole point of the cap.

    /// Last `count` non-empty lines, oldest first, read from the end.
    fn tail_lines(&self, count: usize) -> Vec<String> {
        if count == 0 || !self.path.exists() {
            return Vec::new();
        }
        let Ok(mut handle) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(mut position) = handle.seek(SeekFrom::End(0)) else {
            return Vec::new();
        };
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut newlines = 0usize;
        while position > 0 && newlines <= count {
            let read = std::cmp::min(BLOCK as u64, position);
            position -= read;
            if handle.seek(SeekFrom::Start(position)).is_err() {
                break;
            }
            let mut block = vec![0u8; usize::try_from(read).unwrap_or(BLOCK)];
            if handle.read_exact(&mut block).is_err() {
                break;
            }
            #[allow(clippy::naive_bytecount)] // one small crate is not worth one call site
            {
                newlines += block.iter().filter(|byte| **byte == b'\n').count();
            }
            chunks.push(block);
        }
        chunks.reverse();
        let data: Vec<u8> = chunks.concat();
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<String> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(ToOwned::to_owned)
            .collect();
        let start = lines.len().saturating_sub(count);
        lines[start..].to_vec()
    }

    fn parse(&self, lines: &[String]) -> Vec<(Kind, RoomEvent)> {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            match serde_json::from_str::<Record>(line) {
                Ok(record) => {
                    let kind = if record.kind == "reply" {
                        Kind::Reply
                    } else {
                        Kind::Seen
                    };
                    out.push((kind, record.event));
                }
                Err(exc) => warn!(
                    "skipping malformed transcript line in {}: {exc}",
                    self.path.display()
                ),
            }
        }
        out
    }

    /// The last `n` events (seen and replied), oldest first.
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<RoomEvent> {
        self.parse(&self.tail_lines(n))
            .into_iter()
            .map(|(_kind, ev)| ev)
            .collect()
    }

    /// The last `n` records as `(kind, event)`, oldest first.
    #[must_use]
    pub fn tail_records(&self, n: usize) -> Vec<(Kind, RoomEvent)> {
        self.parse(&self.tail_lines(n))
    }

    /// Events belonging to thread `root`, oldest first, deduplicated.
    #[must_use]
    pub fn thread(&self, root: &str) -> Vec<RoomEvent> {
        self.thread_scan(root, DEFAULT_THREAD_SCAN)
    }

    #[must_use]
    pub fn thread_scan(&self, root: &str, scan: usize) -> Vec<RoomEvent> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for ev in self.tail(scan) {
            if ev.thread_root_or_self() != root || seen.contains(&ev.event_id) {
                continue;
            }
            seen.insert(ev.event_id.clone());
            out.push(ev);
        }
        out
    }

    /// The last `n` distinct events, oldest first (a reply record and its later
    /// `seen` echo collapse into one).
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<RoomEvent> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for ev in self.tail(n * 2).into_iter().rev() {
            if seen.contains(&ev.event_id) {
                continue;
            }
            seen.insert(ev.event_id.clone());
            out.push(ev);
            if out.len() >= n {
                break;
            }
        }
        out.reverse();
        out
    }
}

/// Non-empty lines in `path`; a file that is not there has none.
///
/// Bytes rather than `str`: a transcript with one corrupt byte in it still has
/// to be countable, and `tail_lines` reads it lossily for the same reason.
fn count_lines(path: &Path) -> std::io::Result<usize> {
    let handle = match fs::File::open(path) {
        Ok(handle) => handle,
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(exc) => return Err(exc),
    };
    let mut reader = std::io::BufReader::new(handle);
    let mut line = Vec::new();
    let mut count = 0usize;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(count);
        }
        if line.iter().any(|byte| !byte.is_ascii_whitespace()) {
            count += 1;
        }
    }
}

/// Write the seed for a new live file, 0600, and get it onto the disk.
///
/// `sync_all` before the rename that publishes it: the file this becomes is the
/// agent's memory, and a rename that lands before the bytes do would turn a
/// power cut into an empty transcript.
fn write_seed(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut handle = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    for line in lines {
        handle.write_all(line.as_bytes())?;
        handle.write_all(b"\n")?;
    }
    handle.sync_all()
}

/// Stop a roll at `point` when this thread's test asked for it.
///
/// The hook is the only way to prove what an interrupted roll leaves behind,
/// and it exists in test builds ONLY: a switch that could be flipped in the
/// shipped binary would be a way to lose somebody's transcript.
#[cfg(test)]
fn crash_point(point: &'static str) -> std::io::Result<()> {
    if FAULT.with(std::cell::Cell::get) == Some(point) {
        return Err(std::io::Error::other(format!("simulated crash {point}")));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The step of a roll this thread wants to die at.
    static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)] // the test build's version really can fail
fn crash_point(_point: &'static str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM_ID: &str = "!room:example.com";
    const HUMAN: &str = "@human:example.com";
    const ME: &str = "@bot-a:example.com";

    fn event(event_id: &str, body: &str, thread_root: Option<&str>) -> RoomEvent {
        RoomEvent {
            event_id: event_id.to_owned(),
            room_id: ROOM_ID.to_owned(),
            sender: HUMAN.to_owned(),
            sender_display: None,
            body: body.to_owned(),
            formatted_body: None,
            msgtype: "m.text".to_owned(),
            ts: 1.0,
            thread_root: thread_root.map(ToOwned::to_owned),
            reply_to: None,
            reply_is_fallback: false,
            mentions: std::collections::BTreeSet::new(),
            is_bot: false,
        }
    }

    #[test]
    fn append_and_tail_keep_order() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("room.jsonl"));
        for index in 0..5 {
            transcript.append_seen(&event(
                &format!("$e{index}"),
                &format!("line {index}"),
                None,
            ));
        }
        let ids: Vec<String> = transcript.tail(3).into_iter().map(|e| e.event_id).collect();
        assert_eq!(ids, ["$e2", "$e3", "$e4"]);
        assert_eq!(transcript.tail(100).len(), 5);
    }

    #[test]
    fn a_tail_on_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        assert!(
            Transcript::new(dir.path().join("nope.jsonl"))
                .tail(10)
                .is_empty()
        );
    }

    #[test]
    fn the_tail_reads_across_block_boundaries() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("room.jsonl"));
        for index in 0..2000 {
            transcript.append_seen(&event(&format!("$e{index}"), &"x".repeat(100), None));
        }
        assert!(fs::metadata(&transcript.path).expect("stat").len() > 64 * 1024);
        let ids: Vec<String> = transcript.tail(2).into_iter().map(|e| e.event_id).collect();
        assert_eq!(ids, ["$e1998", "$e1999"]);
    }

    #[test]
    fn replies_are_recorded_with_their_kind() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("room.jsonl"));
        transcript.append_seen(&event("$trigger", "hi", None));
        let mut reply = event("$mine", "echo: hi", Some("$trigger"));
        reply.sender = ME.to_owned();
        reply.msgtype = "m.notice".to_owned();
        reply.is_bot = true;
        transcript.append_reply(&reply);
        let kinds: Vec<Kind> = transcript
            .tail_records(10)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(kinds, [Kind::Seen, Kind::Reply]);
    }

    #[test]
    fn a_thread_is_sliced_by_its_root() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("room.jsonl"));
        transcript.append_seen(&event("$root", "root line", None));
        transcript.append_seen(&event("$in", "in thread", Some("$root")));
        transcript.append_seen(&event("$elsewhere", "other thread", Some("$other")));
        transcript.append_seen(&event("$loose", "unthreaded", None));
        let ids: Vec<String> = transcript
            .thread("$root")
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(ids, ["$root", "$in"]);
        let others: Vec<String> = transcript
            .thread("$other")
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(others, ["$elsewhere"]);
    }

    #[test]
    fn recent_collapses_a_reply_and_its_echo() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("room.jsonl"));
        let mut mine = event("$mine", "echo: hi", None);
        mine.sender = ME.to_owned();
        mine.msgtype = "m.notice".to_owned();
        mine.is_bot = true;
        transcript.append_seen(&event("$trigger", "hi", None));
        transcript.append_reply(&mine);
        transcript.append_seen(&mine); // the same event coming back through /sync
        let ids: Vec<String> = transcript
            .recent(10)
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(ids, ["$trigger", "$mine"]);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("room.jsonl"));
        transcript.append_seen(&event("$good", "x", None));
        let mut handle = fs::OpenOptions::new()
            .append(true)
            .open(&transcript.path)
            .expect("append");
        handle.write_all(b"{ this is not json\n").expect("write");
        drop(handle);
        transcript.append_seen(&event("$also-good", "x", None));
        let ids: Vec<String> = transcript
            .tail(10)
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(ids, ["$good", "$also-good"]);
    }

    // -- rolling ---------------------------------------------------------
    //
    // The gates below are about ONE thing: after a roll the agent still has a
    // transcript it can read, and the file it reads is bounded. The archives
    // are checked as files, because that is what a person looking for last
    // month's conversation finds.

    /// A transcript with a small cap, in its own directory.
    fn rolling(dir: &tempfile::TempDir, keep: usize, archives: usize) -> Transcript {
        Transcript::with_rotation(dir.path().join("room.jsonl"), keep, archives)
    }

    /// Append `count` events named `$e<start>` upwards.
    fn fill(transcript: &Transcript, start: usize, count: usize) {
        for index in start..start + count {
            transcript.append_seen(&event(
                &format!("$e{index}"),
                &format!("line {index}"),
                None,
            ));
        }
    }

    fn line_count(path: &Path) -> usize {
        count_lines(path).expect("the file is readable")
    }

    #[test]
    fn a_transcript_over_the_cap_rolls_exactly_once() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 10, 2);

        // Ten is the cap, not over it.
        fill(&transcript, 0, 10);
        assert!(!transcript.archive(1).exists(), "it rolled AT the cap");
        assert_eq!(line_count(&transcript.path), 10);

        // The eleventh crosses it: one archive, and half the cap kept live.
        fill(&transcript, 10, 1);
        assert_eq!(line_count(&transcript.archive(1)), 11);
        assert_eq!(line_count(&transcript.path), 5);
        assert!(
            !transcript.archive(2).exists(),
            "one roll made two archives"
        );

        // And it does not roll again until the new file fills up: five more
        // lines is ten, still the cap.
        fill(&transcript, 11, 5);
        assert_eq!(line_count(&transcript.path), 10);
        assert_eq!(line_count(&transcript.archive(1)), 11, "it rolled twice");
        assert!(!transcript.archive(2).exists(), "it rolled twice");
    }

    #[test]
    fn the_archives_shift_down_and_the_oldest_is_dropped() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 4, 2);

        // Cap 4, seed 2: every roll archives 5 lines and leaves 2, so three
        // more lines roll it again. Three rolls, two archives kept.
        fill(&transcript, 0, 5); // roll 1: $e0..$e4 archived
        fill(&transcript, 5, 3); // roll 2
        fill(&transcript, 8, 3); // roll 3
        assert!(transcript.archive(1).exists());
        assert!(transcript.archive(2).exists());
        assert!(
            !transcript.archive(3).exists(),
            "archives are kept past transcript_archives"
        );

        // The newest archive is .1 and the older one has shifted to .2; the
        // oldest roll (the one holding $e0) has been deleted.
        let newest = Transcript::new(transcript.archive(1)).tail(10);
        let older = Transcript::new(transcript.archive(2)).tail(10);
        assert_eq!(newest.last().expect("lines").event_id, "$e10");
        assert_eq!(older.last().expect("lines").event_id, "$e7");
        let ids: Vec<&str> = older
            .iter()
            .chain(newest.iter())
            .map(|e| e.event_id.as_str())
            .collect();
        assert!(!ids.contains(&"$e0"), "the oldest archive was not dropped");
    }

    #[test]
    fn the_new_live_file_holds_the_newest_half_and_recent_reads_it_back() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 10, 2);
        fill(&transcript, 0, 11);

        // keep / 2 events, the newest ones, in order.
        let ids: Vec<String> = transcript
            .recent(10)
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(ids, ["$e6", "$e7", "$e8", "$e9", "$e10"]);
        assert_eq!(
            transcript.tail(100).len(),
            5,
            "the live file is not bounded"
        );

        // Byte for byte what was archived: a roll copies lines, it does not
        // re-serialise events, so the format promise survives it.
        let archived = fs::read_to_string(transcript.archive(1)).expect("readable");
        let live = fs::read_to_string(&transcript.path).expect("readable");
        assert!(
            archived.ends_with(&live),
            "the seed is not a copy of the tail"
        );

        // Both files hold the room's conversation, so both are still 0600: a
        // roll that widened the mode would publish it to every user on the box.
        for path in [&transcript.path, &transcript.archive(1)] {
            let mode = fs::metadata(path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is not private", path.display());
        }
    }

    #[test]
    fn a_thread_rooted_in_the_seeded_half_still_reads_back_after_a_roll() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 10, 2);
        fill(&transcript, 0, 8);
        transcript.append_seen(&event("$root", "the question", None));
        transcript.append_seen(&event("$in", "the answer", Some("$root")));
        fill(&transcript, 8, 1); // the eleventh line: it rolls here

        let ids: Vec<String> = transcript
            .thread("$root")
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(ids, ["$root", "$in"], "the thread went blind at the roll");
    }

    #[test]
    fn a_file_under_the_cap_is_never_touched() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 5000, 4);
        fill(&transcript, 0, 50);
        assert_eq!(line_count(&transcript.path), 50);
        assert_eq!(names_in(dir.path()), ["room.jsonl"]);
    }

    #[test]
    fn a_keep_of_zero_never_rolls_at_all() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 0, 4);
        fill(&transcript, 0, 40);
        assert_eq!(line_count(&transcript.path), 40);
        assert_eq!(names_in(dir.path()), ["room.jsonl"]);
    }

    #[test]
    fn a_roll_interrupted_after_the_archive_leaves_a_state_that_reads() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 10, 2);
        fill(&transcript, 0, 10);

        // Die in the one window where the live file does not exist.
        FAULT.with(|cell| cell.set(Some(AFTER_ARCHIVE)));
        fill(&transcript, 10, 1);
        FAULT.with(|cell| cell.set(None));

        // Nothing was lost: every line is in the archive.
        assert_eq!(line_count(&transcript.archive(1)), 11);
        assert!(
            !transcript.path.exists(),
            "the live file survived the crash"
        );

        // And nothing panics or hangs: the reads answer "nothing here", which
        // is true, and the next append starts the file again.
        assert!(transcript.recent(10).is_empty());
        assert!(transcript.thread("$e0").is_empty());
        assert!(transcript.tail_records(10).is_empty());
        fill(&transcript, 11, 1);
        let ids: Vec<String> = transcript
            .recent(10)
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(ids, ["$e11"], "the transcript did not recover");
        assert_eq!(line_count(&transcript.archive(1)), 11, "the archive moved");
    }

    #[test]
    fn a_roll_interrupted_before_the_archive_leaves_the_live_file_alone() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 10, 2);
        fill(&transcript, 0, 10);

        FAULT.with(|cell| cell.set(Some(AFTER_SEED)));
        fill(&transcript, 10, 1);
        FAULT.with(|cell| cell.set(None));

        assert!(!transcript.archive(1).exists(), "it archived anyway");
        assert_eq!(line_count(&transcript.path), 11, "the append was lost");
        let ids: Vec<String> = transcript.tail(2).into_iter().map(|e| e.event_id).collect();
        assert_eq!(ids, ["$e9", "$e10"]);

        // The next append recounts rather than trusting a cache the failed roll
        // left behind, and rolls the file it should have rolled.
        fill(&transcript, 11, 1);
        assert_eq!(line_count(&transcript.archive(1)), 12);
        assert_eq!(line_count(&transcript.path), 5);
    }

    #[test]
    fn a_roll_touches_the_transcript_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = rolling(&dir, 10, 2);
        // The ledger and another room's transcript live in the same directory.
        let ledger = dir.path().join("room.ledger.json");
        fs::write(&ledger, "{\"version\": 1}").expect("write");
        let other = dir.path().join("other.jsonl");
        fs::write(&other, "{\"kind\": \"seen\"}\n").expect("write");

        fill(&transcript, 0, 11);

        assert_eq!(
            fs::read_to_string(&ledger).expect("readable"),
            "{\"version\": 1}"
        );
        assert_eq!(
            fs::read_to_string(&other).expect("readable"),
            "{\"kind\": \"seen\"}\n"
        );
        assert_eq!(
            names_in(dir.path()),
            [
                "other.jsonl",
                "room.jsonl",
                "room.jsonl.1",
                "room.ledger.json"
            ],
            "a roll left something behind"
        );
    }

    /// Every file in `dir`, sorted: a roll is judged by what is on the disk.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("the directory is readable")
            .map(|entry| {
                entry
                    .expect("readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_transcript_file_is_0600_in_a_0700_directory() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let transcript = Transcript::new(dir.path().join("state").join("room.jsonl"));
        transcript.append_seen(&event("$e", "x", None));
        let mode = fs::metadata(&transcript.path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let parent = fs::metadata(transcript.path.parent().expect("has a parent"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent, 0o700);
    }
}
