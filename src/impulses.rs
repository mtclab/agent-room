//! The impulse inlet: things that happened to the agent, outside the room.
//!
//! An agent whose only reason to speak is "a message arrived" is a chat window.
//! A person speaks unprompted because something happened to them - a build went
//! red, a PR merged, a render finished - and the room is where they mention it.
//!
//! The inlet is a directory, one JSON file per impulse:
//!
//! ```text
//! <state_dir>/rooms/<room>.impulses/<ts>-<id>.json
//! {"ts": 1756832000.0, "kind": "git", "summary": "merged PR #5 in agent-room",
//!  "detail": "", "ttl_s": 21600}
//! ```
//!
//! A directory of files rather than a socket or an HTTP endpoint on purpose:
//! every language, cron job and shell hook can write one, it survives a
//! connector that is not running, the permissions are the filesystem's, and
//! nothing has to be up for a `printf > file` to work. `agent-room impulse` and
//! the MCP tool `room_impulse` are conveniences on top of exactly this format,
//! which is byte-for-byte the one the Python this was ported from wrote - so an
//! inlet either of them left behind is one this binary reads.
//!
//! An impulse is a CANDIDATE, never a message. The connector presence-gates it,
//! backs off, asks the judge and usually says nothing. It expires unspoken
//! (`ttl_s`, default six hours) because "the thing that happened to me four
//! hours ago" is not worth interrupting a conversation with.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{info, warn};

use crate::config::room_state_path;
use crate::head;

/// Suffix of the per-room inlet DIRECTORY under `state_dir/rooms/`.
pub const IMPULSE_DIR_SUFFIX: &str = ".impulses";
/// How long an unspoken impulse stays interesting. Six hours: long enough to
/// survive a night without the agent, short enough that nothing stale is said.
pub const DEFAULT_TTL_S: f64 = 6.0 * 3600.0;

/// Raised when an impulse cannot be written (a caller can print this).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ImpulseError(pub String);

/// One thing that happened to the agent, waiting to be worth saying.
#[derive(Debug, Clone, PartialEq)]
pub struct Impulse {
    pub path: PathBuf,
    pub ts: f64,
    pub kind: String,
    pub summary: String,
    pub detail: String,
    pub ttl_s: f64,
}

impl Impulse {
    /// Stable identity: the file name. Used to avoid queueing it twice.
    #[must_use]
    pub fn id(&self) -> String {
        self.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    #[must_use]
    pub fn expired(&self, now: f64) -> bool {
        self.ttl_s > 0.0 && now - self.ts >= self.ttl_s
    }

    /// The impulse as one line a brain can be told about.
    #[must_use]
    pub fn note(&self) -> String {
        let summary = self.summary.trim();
        let detail = self.detail.trim();
        let head = if self.kind.is_empty() {
            summary.to_owned()
        } else {
            format!("[{}] {summary}", self.kind)
        };
        if detail.is_empty() {
            head
        } else {
            format!("{head} - {detail}")
        }
    }

    /// Remove the file. An impulse is delivered at most once.
    pub fn forget(&self) {
        if let Err(exc) = fs::remove_file(&self.path)
            && exc.kind() != std::io::ErrorKind::NotFound
        {
            warn!("cannot remove impulse {}: {exc}", self.path.display());
        }
    }
}

/// The inlet directory for one room, beside its transcript and ledger.
#[must_use]
pub fn impulse_dir(state_dir: &Path, room_id: &str) -> PathBuf {
    room_state_path(state_dir, room_id, IMPULSE_DIR_SUFFIX)
}

/// Drop one impulse into a room's inlet. Returns the file written.
///
/// Written through a temp file and a rename, so a connector polling the
/// directory can never read a half-written impulse.
///
/// # Errors
/// When the summary is empty (an impulse needs something to say), or when the
/// inlet cannot be created or written.
pub fn write_impulse(
    state_dir: &Path,
    room_id: &str,
    summary: &str,
    kind: &str,
    detail: &str,
    ttl_s: f64,
    ts: Option<f64>,
) -> Result<PathBuf, ImpulseError> {
    if summary.trim().is_empty() {
        return Err(ImpulseError(
            "an impulse needs something to say (empty summary)".to_owned(),
        ));
    }
    let directory = impulse_dir(state_dir, room_id);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .map_err(|exc| {
            ImpulseError(format!(
                "cannot create the impulse inlet {}: {exc}",
                directory.display()
            ))
        })?;
    let stamp = ts.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    });
    let path = directory.join(format!(
        "{stamp:.6}-{}.json",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let payload = serde_json::json!({
        "ts": stamp,
        "kind": kind,
        "summary": summary.trim(),
        "detail": detail.trim(),
        "ttl_s": ttl_s,
    })
    .to_string();
    let tmp = path.with_extension("json.tmp");
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(payload.as_bytes()))
        .and_then(|()| fs::rename(&tmp, &path));
    written.map_err(|exc| {
        ImpulseError(format!(
            "cannot write the impulse {}: {exc}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// Read one impulse file, or None when it is not a usable impulse.
#[must_use]
pub fn parse_impulse(path: &Path, default_ttl_s: f64) -> Option<Impulse> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => return None,
        Err(exc) => {
            warn!("unreadable impulse {} ({exc}); dropping it", path.display());
            return None;
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        warn!("unreadable impulse {}; dropping it", path.display());
        return None;
    };
    if !value.is_object() {
        warn!("impulse {} is not an object; dropping it", path.display());
        return None;
    }
    let summary = string_field(&value, "summary");
    if summary.is_empty() {
        warn!("impulse {} has no summary; dropping it", path.display());
        return None;
    }
    let ts = number_field(&value, "ts").unwrap_or(0.0);
    let ttl_s = number_field(&value, "ttl_s").unwrap_or(default_ttl_s);
    let ts = if ts > 0.0 {
        ts
    } else {
        // A hand-written impulse may leave the stamp out; the file's own mtime
        // is when it happened as far as anybody here can tell.
        modified_seconds(path).unwrap_or(0.0)
    };
    Some(Impulse {
        path: path.to_path_buf(),
        ts,
        kind: string_field(&value, "kind"),
        summary,
        detail: string_field(&value, "detail"),
        ttl_s,
    })
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn number_field(value: &Value, key: &str) -> Option<f64> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

fn modified_seconds(path: &Path) -> Option<f64> {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

/// Every usable impulse in the inlet, oldest first.
///
/// A file that is not a usable impulse is REMOVED, not skipped: the inlet is a
/// queue, it is polled every few seconds, and a permanently unparseable entry
/// would otherwise print a warning for ever. What went wrong is logged first.
#[must_use]
pub fn read_impulses(directory: &Path, default_ttl_s: f64) -> Vec<Impulse> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(exc) => {
            if exc.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    "cannot read the impulse inlet {}: {exc}",
                    directory.display()
                );
            }
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    let mut out: Vec<Impulse> = Vec::new();
    for path in paths {
        match parse_impulse(&path, default_ttl_s) {
            Some(impulse) => out.push(impulse),
            None => {
                let _ignored = fs::remove_file(&path);
            }
        }
    }
    out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    out
}

/// Delete what has gone stale, return what is still worth saying.
#[must_use]
pub fn drop_expired(impulses: Vec<Impulse>, now: f64) -> Vec<Impulse> {
    let mut fresh = Vec::with_capacity(impulses.len());
    for impulse in impulses {
        if impulse.expired(now) {
            info!(
                "impulse expired unspoken after {:.0} s (ttl {:.0} s): {}",
                now - impulse.ts,
                impulse.ttl_s,
                head(&impulse.summary, 120)
            );
            impulse.forget();
            continue;
        }
        fresh.push(impulse);
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM_ID: &str = "!room:example.com";

    fn state_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("state");
        (dir, path)
    }

    #[test]
    fn a_written_impulse_reads_back_as_itself() {
        let (_dir, state) = state_dir();
        let path = write_impulse(
            &state,
            ROOM_ID,
            "  merged PR #5  ",
            "git",
            " on the port ",
            60.0,
            Some(1000.0),
        )
        .expect("the impulse is written");
        let read = parse_impulse(&path, DEFAULT_TTL_S).expect("it reads back");
        assert_eq!(read.summary, "merged PR #5");
        assert_eq!(read.kind, "git");
        assert_eq!(read.detail, "on the port");
        assert!((read.ts - 1000.0).abs() < f64::EPSILON);
        assert!((read.ttl_s - 60.0).abs() < f64::EPSILON);
        assert_eq!(read.note(), "[git] merged PR #5 - on the port");
    }

    #[test]
    fn the_file_is_json_the_reference_wrote_the_same_way() {
        // The inlet is a shared format: a file written by the Python CLI has to
        // be readable here, so the keys and their types are part of the
        // contract rather than an implementation detail.
        let (_dir, state) = state_dir();
        let path =
            write_impulse(&state, ROOM_ID, "something", "note", "", 30.0, Some(5.5)).expect("ok");
        let raw: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("the file")).expect("json");
        assert_eq!(raw["ts"], 5.5);
        assert_eq!(raw["kind"], "note");
        assert_eq!(raw["summary"], "something");
        assert_eq!(raw["detail"], "");
        assert_eq!(raw["ttl_s"], 30.0);
        assert!(path.to_string_lossy().contains(".impulses/"));
    }

    #[test]
    fn an_impulse_with_nothing_to_say_is_refused() {
        let (_dir, state) = state_dir();
        assert!(write_impulse(&state, ROOM_ID, "   ", "note", "", 1.0, None).is_err());
    }

    #[test]
    fn the_inlet_reads_oldest_first_and_throws_away_what_it_cannot_use() {
        let (_dir, state) = state_dir();
        write_impulse(&state, ROOM_ID, "second", "note", "", 60.0, Some(200.0)).expect("ok");
        write_impulse(&state, ROOM_ID, "first", "note", "", 60.0, Some(100.0)).expect("ok");
        let directory = impulse_dir(&state, ROOM_ID);
        fs::write(directory.join("rubbish.json"), "{not json").expect("a bad file");
        fs::write(directory.join("empty.json"), "{\"summary\": \"\"}").expect("a bad file");
        fs::write(directory.join("notes.txt"), "ignored").expect("another file");

        let impulses = read_impulses(&directory, DEFAULT_TTL_S);
        assert_eq!(
            impulses
                .iter()
                .map(|i| i.summary.clone())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(
            !directory.join("rubbish.json").exists(),
            "an unusable impulse must be dropped, not warned about for ever"
        );
        assert!(!directory.join("empty.json").exists());
        assert!(
            directory.join("notes.txt").exists(),
            "only *.json is the inlet's business"
        );
    }

    #[test]
    fn an_expired_impulse_is_deleted_rather_than_returned() {
        let (_dir, state) = state_dir();
        write_impulse(&state, ROOM_ID, "stale", "note", "", 10.0, Some(100.0)).expect("ok");
        write_impulse(&state, ROOM_ID, "fresh", "note", "", 600.0, Some(100.0)).expect("ok");
        let directory = impulse_dir(&state, ROOM_ID);
        let fresh = drop_expired(read_impulses(&directory, DEFAULT_TTL_S), 200.0);
        assert_eq!(
            fresh.iter().map(|i| i.summary.clone()).collect::<Vec<_>>(),
            ["fresh"]
        );
        assert_eq!(read_impulses(&directory, DEFAULT_TTL_S).len(), 1);
    }

    #[test]
    fn a_ttl_of_zero_is_a_lifetime_of_forever() {
        let impulse = Impulse {
            path: PathBuf::from("/tmp/x.json"),
            ts: 0.0,
            kind: String::new(),
            summary: "always worth saying".to_owned(),
            detail: String::new(),
            ttl_s: 0.0,
        };
        assert!(!impulse.expired(1_000_000.0));
        assert_eq!(impulse.note(), "always worth saying");
    }

    #[test]
    fn forgetting_an_impulse_twice_is_not_an_error() {
        let (_dir, state) = state_dir();
        let path = write_impulse(&state, ROOM_ID, "once", "note", "", 60.0, None).expect("ok");
        let impulse = parse_impulse(&path, DEFAULT_TTL_S).expect("it reads back");
        impulse.forget();
        impulse.forget();
        assert!(!path.exists());
        assert!(parse_impulse(&path, DEFAULT_TTL_S).is_none());
    }
}
