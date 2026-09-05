//! The changelog gate: the version in `Cargo.toml` has a section in
//! `CHANGELOG.md`, and that section is what the release will show.
//!
//! The release workflow publishes the GitHub Release with the version's section
//! as its body, read by `scripts/changelog-section.sh`, and refuses a tag whose
//! version has no section. Waiting for the tag to find that out means a failed
//! release and a tag to delete, so `make gate` asks the same script the same
//! question first. The script is the one implementation; these tests do not
//! parse the changelog a second way.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regex::Regex;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn cargo_version() -> String {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml reads");
    let re = Regex::new(r#"(?m)^version = "([^"]+)"$"#).expect("pattern compiles");
    re.captures(&manifest)
        .map(|c| c[1].to_string())
        .expect("Cargo.toml has a version line")
}

fn section_of(version: &str, changelog: &Path) -> Output {
    Command::new(root().join("scripts/changelog-section.sh"))
        .arg(version)
        .arg(changelog)
        .output()
        .expect("scripts/changelog-section.sh runs")
}

fn changelog_text() -> String {
    fs::read_to_string(root().join("CHANGELOG.md")).expect("CHANGELOG.md reads")
}

/// The gate itself: the version the binary prints has a section with content.
#[test]
fn the_cargo_version_has_a_changelog_section() {
    let version = cargo_version();
    let out = section_of(&version, &root().join("CHANGELOG.md"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Cargo.toml says {version} but CHANGELOG.md has no section for it - the release \
         would be refused. {stderr}"
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.lines().any(|l| l.starts_with("- ")),
        "the {version} section has no bullet - a heading with nothing under it is not a \
         changelog entry:\n{body}"
    );
}

/// The newest RELEASED section is the Cargo version. A section added below an
/// older one would pass the lookup and still tell the reader the wrong story.
#[test]
fn the_cargo_version_is_the_newest_released_section() {
    let version = cargo_version();
    let text = changelog_text();
    let heading = Regex::new(r"(?m)^## \[([^\]]+)\]").expect("pattern compiles");
    let first_released = heading
        .captures_iter(&text)
        .map(|c| c[1].to_string())
        .find(|v| v != "Unreleased")
        .expect("CHANGELOG.md has at least one released section");
    assert_eq!(
        first_released, version,
        "the first released section in CHANGELOG.md is {first_released}, but Cargo.toml \
         says {version}: the new section goes directly under [Unreleased]"
    );
}

/// Every released heading carries a date and a compare link, and there is an
/// [Unreleased] heading to collect the next release under.
#[test]
fn every_released_section_is_dated_and_linked() {
    let text = changelog_text();
    assert!(
        text.contains("\n## [Unreleased]\n"),
        "CHANGELOG.md has no [Unreleased] heading"
    );
    let heading = Regex::new(r"(?m)^## \[([^\]]+)\](.*)$").expect("pattern compiles");
    let dated = Regex::new(r"^ - \d{4}-\d{2}-\d{2}$").expect("pattern compiles");
    let mut seen = 0;
    for cap in heading.captures_iter(&text) {
        let version = &cap[1];
        if version == "Unreleased" {
            continue;
        }
        seen += 1;
        assert!(
            dated.is_match(&cap[2]),
            "heading for {version} is not `## [{version}] - YYYY-MM-DD`: `{}`",
            &cap[0]
        );
        let link = format!("\n[{version}]: https://");
        assert!(
            text.contains(&link),
            "no link reference `[{version}]: <url>` at the bottom of CHANGELOG.md"
        );
    }
    assert!(seen > 0, "CHANGELOG.md has no released section");
}

/// Teeth: the script fails closed. A changelog without the section makes it exit
/// non-zero with nothing on stdout, so a release cannot ship an empty body.
#[test]
fn the_script_refuses_a_version_without_a_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("CHANGELOG.md");
    fs::write(
        &path,
        "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n\n- Something.\n\n\
         [0.1.0]: https://example.com/0.1.0\n",
    )
    .expect("fixture writes");

    let missing = section_of("0.2.0", &path);
    assert_eq!(missing.status.code(), Some(1), "a missing section exits 1");
    assert!(
        missing.stdout.is_empty(),
        "nothing on stdout for a missing section"
    );
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no section for 0.2.0"),
        "stderr names the version that has no section"
    );

    let empty_path = dir.path().join("EMPTY.md");
    fs::write(
        &empty_path,
        "# Changelog\n\n## [0.2.0] - 2026-01-02\n\n## [0.1.0] - 2026-01-01\n\n- Something.\n",
    )
    .expect("fixture writes");
    let empty = section_of("0.2.0", &empty_path);
    assert_eq!(empty.status.code(), Some(1), "an empty section exits 1");

    let present = section_of("0.1.0", &path);
    assert!(
        present.status.success(),
        "the section that exists is printed"
    );
    assert_eq!(
        String::from_utf8_lossy(&present.stdout).trim(),
        "- Something.",
        "the section comes back without its heading and without the link block"
    );
}
