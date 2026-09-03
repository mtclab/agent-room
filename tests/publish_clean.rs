//! The publish gate: no tracked file names a deployment or an account.
//!
//! This repository is public. Which homeserver the connectors run against,
//! which accounts they hold and who owns them are deployment details that live
//! in `~/.config/agent-room/live.env` and in the operator's own config - never
//! in the tree. A scrub is a one-off; this is the standing assertion that keeps
//! it scrubbed, and it fails with `file:line` so the offending text is one
//! `git diff` away.
//!
//! Every pattern below is assembled from fragments, so THIS file does not
//! itself carry any of the strings it forbids. That is deliberate: a gate that
//! had to exempt its own path would be a hole anyone could park a leak in.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;

/// The one tracked file allowed to hold placeholder account localparts: it is
/// the documented shape of the live-gate env, and its names are invented.
const ALLOWED_FILES: &[&str] = &["tests/live/live.env.example"];

/// A file is text if it decodes as UTF-8 and holds no NUL byte. Everything else
/// (there is nothing binary tracked today, but a font or an icon would be) is
/// skipped rather than mis-scanned.
fn text_of(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// What may appear despite matching a pattern below: this repository's own
/// slug. It was the clone URL alone until CI started publishing from the tree,
/// and every way of naming the repo is built from the same two words - the
/// clone and release URLs, the registry path of the container image, and the
/// `--repo` a reader passes to `gh attestation verify`. The slug is the public
/// name of a public repository; nothing else about the estate is spelled by it.
/// Assembled, like the patterns, so this file stays clean of the literal.
fn allowed_substrings() -> Vec<String> {
    let org = format!("{}{}", "mtc", "lab");
    vec![format!("{org}/agent-room")]
}

/// `(name, pattern)` pairs. The name is what a failure reports, so it says what
/// KIND of leak was found rather than just echoing the line.
fn forbidden() -> Vec<(&'static str, Regex)> {
    let hp = "hp";
    let org = format!("{}{}", "mtc", "lab");
    let chat = format!("{}{}", "mtc", "chat");
    let owner = format!("{}{}", "ol", "li");
    let workspace_user = format!("{}{}", "kasm", "-user");
    let live_env = format!("{}{}", "tests/live/", r"\.env");
    // Sibling projects on the owner's machine: their names and paths say a
    // private estate exists, which a public repo has no business revealing.
    let sibling = format!("{}{}", "home", "pilot");
    let workspace_path = format!("{}{}", "rep", "ot/");
    let agent_tool_config = format!("{}{}", r"\.config/open", "code");
    let old_venv = format!("{}{}", "agent-", "venv");
    // The production room's id leaked into a test once (readiness walk).
    let live_room = format!("{}{}", "zSGZ", "ayOm");
    let patterns = vec![
        ("live room id", live_room),
        (
            "sibling project",
            format!(r"(?i)({sibling}|{workspace_path}|{agent_tool_config}|{old_venv})"),
        ),
        (
            "estate account localpart",
            format!(r"(?i)(?:\b|_){hp}-[a-z]"),
        ),
        ("owner name or handle", format!(r"(?i)\bk?{owner}\b")),
        ("org name", format!(r"(?i)({org}|{chat})")),
        ("LAN address", format!(r"10\.{}\.", 96)),
        ("workspace account", format!(r"(?i){workspace_user}")),
        ("in-tree live env path", live_env),
    ];
    patterns
        .into_iter()
        .map(|(name, source)| {
            let regex = Regex::new(&source).expect("a literal pattern compiles");
            (name, regex)
        })
        .collect()
}

/// Every file git tracks, as paths relative to the repository root.
///
/// No fallback and no skip: a gate that quietly passed when it could not read
/// the file list would be worse than no gate, because a green run would then
/// mean nothing.
fn tracked_files(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .expect("`git ls-files` runs - this gate reads the tracked file list");
    assert!(
        output.status.success(),
        "`git ls-files` failed in {}: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8(output.stdout).expect("git prints paths as UTF-8");
    let files: Vec<PathBuf> = listing
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(PathBuf::from)
        .collect();
    assert!(
        !files.is_empty(),
        "git tracks no files in {}",
        root.display()
    );
    files
}

#[test]
fn no_tracked_file_names_a_deployment_an_account_or_the_owner() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let allowed = allowed_substrings();
    let patterns = forbidden();
    let mut findings = Vec::new();

    for relative in tracked_files(root) {
        let name = relative.to_string_lossy().replace('\\', "/");
        if ALLOWED_FILES.contains(&name.as_str()) {
            continue;
        }
        let Some(text) = text_of(&root.join(&relative)) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            // The repository URL is allowed to name the org; nothing else is.
            let mut scanned = line.to_owned();
            for substring in &allowed {
                scanned = scanned.replace(substring.as_str(), "");
            }
            for (kind, pattern) in &patterns {
                if let Some(hit) = pattern.find(&scanned) {
                    findings.push(format!(
                        "{name}:{}: {kind} ({:?})",
                        number + 1,
                        hit.as_str()
                    ));
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "this repository is public - these tracked lines name a deployment, an \
         account or the owner:\n{}",
        findings.join("\n")
    );
}
