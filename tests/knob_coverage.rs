//! The knob-coverage gate: no knob in the schema is left at its default by
//! every test in the repository.
//!
//! An outside contributor found that `agent-room doctor` had never sent the
//! configured `api_key` when it checked the brain, so every key-protected
//! endpoint failed 401 with the right key in the config. The suite was green
//! throughout, and the reason it was green is the reason this file exists: every
//! gate pointed at a keyless endpoint, so `api_key` was never set to anything
//! but `""` ANYWHERE, and no test could have noticed whether it was sent. A knob
//! nobody ever turns is a knob nobody is testing, whatever the test names say.
//!
//! So: for every field the config schema has, some test somewhere must set it to
//! a value that is NOT the shipped default. That is a floor, not a ceiling - it
//! says the knob was turned, and the test that turns it is what says the turn
//! did something.
//!
//! # Where the inventory comes from
//!
//! Not from a list in this file. Two baseline configs are loaded through the
//! product's own `load_config`, serialised, and walked key by key. A field that
//! is the same in both is a field with a schema default (and that value IS the
//! default); a field that differs is one the operator must supply, which has no
//! default to be left at. Add a knob to `config.rs` and it appears here on the
//! next run; add a REQUIRED one and both baselines stop loading, which fails
//! this gate with the parser's own message rather than passing quietly.
//!
//! # What the grep can see, and what it cannot
//!
//! The scan reads the tracked test sources - everything under `tests/` and the
//! `#[cfg(test)]` regions of `src/` - for a knob's name being assigned:
//! `name: value` (a Rust struct literal, a YAML mapping, a JSON or Python dict),
//! `.name = value`, `name=value` and `dict["name"] = value` in Python, and
//! `--name value`. Rust strings holding YAML are split on their `\n` escapes and
//! unescaped first, so a config written inside a test reads like the file it is.
//!
//! It reads VALUES only as far as a reader can without running anything:
//!
//! - a literal (number, string, bool, `None`, a list, a map, `Some(...)`,
//!   `Enum::Variant`) is compared with the default;
//! - a bare word inside a config file - a `.yaml`, or the YAML in a Rust string
//!   after its first `\n` - is the scalar it looks like: `post_as: text` is the
//!   string "text";
//! - a bare identifier anywhere else is looked up as a constant in the same
//!   file, and if it is not one, the hit does NOT count: in Rust, `topics:
//!   wanted` could be anything;
//! - any other expression (`str(path)`, `dir.join("x")`) counts only against a
//!   default of `null`, on the assumption that an expression which is not the
//!   literal `None` is not None. That is the one thing here taken on trust.
//!
//! Everything else fails CLOSED. A knob whose only assignments cannot be read is
//! reported as uncovered with the `file:line` that could not be read, because a
//! gate that guessed in the other direction would hand out passes for free.
//!
//! This file is not scanned. Its baselines mention nearly every knob in the
//! schema, and a gate that counted its own scaffolding as coverage would pass
//! for ever.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// This file, which is never scanned - see the module docs.
const SELF_PATH: &str = "tests/knob_coverage.rs";

/// `BotToBot::All`, and anything else written as a path to a variant.
static ENUM_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*::)+([A-Za-z_][A-Za-z0-9_]*)$").expect("a literal pattern")
});

// -- the inventory -----------------------------------------------------------

/// What the schema says a field's default is.
#[derive(Debug, Clone, PartialEq)]
enum Default_ {
    /// The same in every baseline, so this is the shipped value.
    Is(Value),
    /// Different in every baseline: the operator supplies it, and there is no
    /// default to leave it at.
    Operator,
}

/// One baseline config file: everything optional left out, so what loads is the
/// default for each of them, and everything required different from the other
/// baseline's.
fn baseline(dir: &Path, tag: &str) -> PathBuf {
    let path = dir.join(format!("{tag}.yaml"));
    let body = format!(
        "homeserver: https://{tag}.invalid\n\
         user_id: \"@{tag}:{tag}.invalid\"\n\
         {credential}\n\
         rooms:\n  - \"!{tag}:{tag}.invalid\"\n\
         state_dir: {dir}/{tag}-state\n\
         brain:\n  kind: {kind}\n  \
         openai_compat:\n    base_url: https://{tag}.invalid/v1\n    model: {tag}-model\n  \
         claude_code: {{}}\n  echo: {{}}\n",
        dir = dir.display(),
        credential = if tag == "one" {
            format!("access_token_file: {}/one-token", dir.display())
        } else {
            "password: \"two-password\"".to_owned()
        },
        kind = if tag == "one" {
            "echo"
        } else {
            "openai_compat"
        },
    );
    std::fs::write(&path, body).expect("the baseline config is written");
    path
}

/// Flatten a serialised config into `dotted.path -> value`. An empty map (an
/// `extra_body` nobody set) is a leaf: it is a knob, not a section.
fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (key, inner) in fields {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, inner, out);
            }
        }
        _ => {
            out.insert(prefix.to_owned(), value.clone());
        }
    }
}

fn loaded(path: &Path) -> BTreeMap<String, Value> {
    let cfg = agent_room::config::load_config(path).unwrap_or_else(|exc| {
        panic!(
            "this gate's baseline config no longer loads: {exc}\n\nA REQUIRED field was added to \
             the schema. Add it to `baseline()` in {SELF_PATH} (with a different value in each of \
             the two baselines) and the new knob joins the inventory."
        )
    });
    let value = serde_json::to_value(&cfg).expect("a config serialises");
    let mut flat = BTreeMap::new();
    flatten("", &value, &mut flat);
    flat
}

/// Every knob the schema has, with its default, derived from the types.
fn inventory() -> BTreeMap<String, Default_> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let one = loaded(&baseline(dir.path(), "one"));
    let two = loaded(&baseline(dir.path(), "two"));
    assert_eq!(
        one.keys().collect::<Vec<_>>(),
        two.keys().collect::<Vec<_>>(),
        "the two baselines produced different key sets"
    );
    assert!(
        one.len() > 50,
        "the inventory collapsed to {} knobs: the walk is broken",
        one.len()
    );
    one.into_iter()
        .map(|(path, value)| {
            let default = if two.get(&path) == Some(&value) {
                Default_::Is(value)
            } else {
                Default_::Operator
            };
            (path, default)
        })
        .collect()
}

// -- the sources -------------------------------------------------------------

/// One scannable file: its path, and its lines already unescaped, split on the
/// `\n` escapes of embedded YAML, and stripped of comments.
struct Source {
    label: String,
    /// `(line number in the file, text)`.
    lines: Vec<(usize, String)>,
    /// The same lines, lowercased with the underscores taken out, so
    /// `openai_compat` and `OpenAiCompatBrainConfig` are one name.
    flattened: Vec<String>,
    /// Whether a bare word on this line is a YAML scalar rather than a name:
    /// true for a `.yaml` file, and for the config text inside a Rust string
    /// after its first `\n`.
    yaml: Vec<bool>,
}

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
    assert!(!files.is_empty(), "git tracks no files");
    files
}

/// The `#[cfg(test)]` items of a source file, and nothing else: production code
/// setting a field is not a test setting it. Line numbers are kept by blanking
/// what is dropped rather than removing it.
fn test_regions(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if !lines[index].trim_start().starts_with("#[cfg(test)]") {
            index += 1;
            continue;
        }
        // A top-level item ends at the `}` in column zero, which `cargo fmt` -
        // itself a gate here - guarantees.
        while index < lines.len() {
            keep[index] = true;
            let closed = lines[index] == "}";
            index += 1;
            if closed {
                break;
            }
        }
    }
    lines
        .iter()
        .zip(keep)
        .map(|(line, wanted)| if wanted { *line } else { "" })
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Cut a line at the comment that ends it, so a knob named in prose does not
/// count as a knob somebody set.
fn strip_comment(line: &str, rust: bool) -> String {
    if rust {
        for (index, _) in line.match_indices("//") {
            // `//`, but not the one in `https://`.
            if !line[..index].ends_with(':') {
                return line[..index].to_owned();
            }
        }
        return line.to_owned();
    }
    if line.trim_start().starts_with('#') {
        return String::new();
    }
    match line.find(" #") {
        Some(index) => line[..index].to_owned(),
        None => line.to_owned(),
    }
}

fn source_of(label: &str, text: &str, rust: bool) -> Source {
    let plain = Path::new(label)
        .extension()
        .is_some_and(|kind| kind == "yaml" || kind == "yml");
    let mut lines = Vec::new();
    let mut yaml = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        // A Rust string holding YAML is a config file with its newlines
        // escaped: read it as the file it is.
        let unescaped = raw.replace("\\n", "\n").replace("\\\"", "\"");
        for (piece_index, piece) in unescaped.lines().enumerate() {
            let cut = strip_comment(piece, rust);
            if !cut.trim().is_empty() {
                lines.push((index + 1, cut));
                yaml.push(plain || piece_index > 0);
            }
        }
    }
    let flattened = lines.iter().map(|(_, text)| squash(text)).collect();
    Source {
        label: label.to_owned(),
        lines,
        flattened,
        yaml,
    }
}

/// A name with its case and its underscores taken out: `openai_compat`,
/// `OpenAiCompat` and `openaiCompat` are the same section.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Every tracked test source: all of `tests/`, and the `#[cfg(test)]` regions
/// of `src/`.
fn test_sources(root: &Path) -> Vec<Source> {
    let mut sources = Vec::new();
    for relative in tracked_files(root) {
        let name = relative.to_string_lossy().replace('\\', "/");
        if name == SELF_PATH {
            continue;
        }
        let extension = relative
            .extension()
            .map(|value| value.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let rust = extension == "rs";
        let scannable = matches!(extension.as_str(), "rs" | "py" | "yaml" | "yml" | "json");
        if !scannable {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(&relative)) else {
            continue;
        };
        if name.starts_with("tests/") {
            sources.push(source_of(&name, &text, rust));
        } else if name.starts_with("src/") && rust {
            sources.push(source_of(&name, &test_regions(&text), true));
        }
    }
    assert!(sources.len() > 10, "only {} test sources", sources.len());
    sources
}

// -- the scan ----------------------------------------------------------------

/// One place a knob's name was assigned something.
struct Hit {
    key: String,
    value: String,
    /// A bare word in this value is a scalar, not a name: see `Source::yaml`.
    yaml: bool,
    source: usize,
    /// Index into `Source::lines`, so the enclosing section can be found.
    at: usize,
    line: usize,
}

impl Hit {
    fn place(&self, sources: &[Source]) -> String {
        format!(
            "{}:{}: {} = {}",
            sources[self.source].label,
            self.line,
            self.key,
            self.value.trim()
        )
    }
}

/// The five ways a test writes a knob down.
struct Forms {
    all: Vec<(Regex, bool)>,
}

impl Forms {
    fn new() -> Self {
        // `name:` in Rust and YAML, `"name":` in JSON and Python.
        let field = Regex::new(r#"(?:^|[^A-Za-z0-9_])([a-z_][a-z0-9_]*)["']?[ \t]*:[ \t]*"#)
            .expect("a literal pattern");
        let assign = Regex::new(r"\.([a-z_][a-z0-9_]*)[ \t]*=[ \t]*").expect("a literal pattern");
        let subscript = Regex::new(r#"\[[ \t]*"([a-z_][a-z0-9_]*)"[ \t]*\][ \t]*=[ \t]*"#)
            .expect("a literal pattern");
        let flag = Regex::new(r"--([a-z][a-z0-9-]*)[ =]").expect("a literal pattern");
        // Python only: a keyword argument. In Rust, `name = value` with no `.`
        // in front is a local, not a field.
        let kwarg = Regex::new(r"(?:^|[^A-Za-z0-9_.])([a-z_][a-z0-9_]*)[ \t]*=[ \t]*")
            .expect("a literal pattern");
        Self {
            all: vec![
                (field, false),
                (assign, false),
                (subscript, false),
                (flag, false),
                (kwarg, true),
            ],
        }
    }
}

/// Every assignment of every name, in every test source.
fn scan(sources: &[Source]) -> BTreeMap<String, Vec<Hit>> {
    let forms = Forms::new();
    let mut found: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for (index, source) in sources.iter().enumerate() {
        let python = Path::new(&source.label)
            .extension()
            .is_some_and(|kind| kind == "py");
        for (at, (line, text)) in source.lines.iter().enumerate() {
            for (pattern, python_only) in &forms.all {
                if *python_only && !python {
                    continue;
                }
                for capture in pattern.captures_iter(text) {
                    let whole = capture.get(0).expect("the whole match");
                    let name = capture.get(1).expect("the name").as_str();
                    let value = &text[whole.end()..];
                    // `Type::method` and `a == b` are not assignments.
                    if value.starts_with(':') || value.starts_with('=') {
                        continue;
                    }
                    let key = name.replace('-', "_");
                    found.entry(key.clone()).or_default().push(Hit {
                        key,
                        value: value.to_owned(),
                        yaml: source.yaml[at],
                        source: index,
                        at,
                        line: *line,
                    });
                }
            }
        }
    }
    found
}

// -- reading a value ---------------------------------------------------------

/// As much of a value as a reader can tell without running anything.
#[derive(Debug, Clone, PartialEq)]
enum Lit {
    Null,
    Bool(bool),
    Number(f64),
    /// A string, or a YAML scalar, or `Enum::Variant` as its `snake_case` name.
    Text(String),
    /// `Some(...)`: whatever it holds, it is not `None`.
    Wrapped,
    Seq(Vec<String>),
    Map {
        empty: bool,
    },
    /// A bare identifier: a constant this gate could not resolve.
    Word(String),
    /// A call or an expression: not a literal, but not a name either.
    Expr,
    /// A type in a declaration (`state_dir: PathBuf`), which is nobody setting
    /// anything at all.
    Type,
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// The text of one literal, canonical enough to compare: quotes and Rust's
/// `.to_owned()` off, numbers through `f64`.
fn normalise(text: &str) -> String {
    let trimmed = text
        .trim()
        .trim_end_matches(&[',', ';', ')', ']'][..])
        .trim();
    let unquoted = trimmed
        .trim_start_matches(['"', '\''])
        .split(['"', '\''])
        .next()
        .unwrap_or("")
        .to_owned();
    let bare = if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        unquoted
    } else {
        trimmed.to_owned()
    };
    bare.parse::<f64>()
        .map_or(bare.clone(), |number| format!("{number}"))
}

/// Split `[a, b]` on the commas that are not inside something, as written.
fn elements(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for character in inner.chars() {
        match character {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(current.trim().to_owned());
                current = String::new();
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out.retain(|element| !element.is_empty());
    out
}

/// Everything up to the bracket that closes the one this starts with.
fn balanced(text: &str, open: char, close: char) -> Option<&str> {
    let start = text.find(open)?;
    let mut depth = 0i32;
    for (index, character) in text.char_indices().skip(start) {
        if character == open {
            depth += 1;
        } else if character == close {
            depth -= 1;
            if depth == 0 {
                return Some(&text[start + open.len_utf8()..index]);
            }
        }
    }
    None
}

/// Is this token a TYPE rather than a value?
///
/// `state_dir: PathBuf` in a struct definition and `topics: &[&str]` in a
/// function signature are declarations, not somebody setting a knob, and a gate
/// that read them as values would hand out coverage for writing a `struct`.
fn looks_like_a_type(token: &str) -> bool {
    let word = token
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim()
        .trim_end_matches(&[',', ';', ')', ']', '}'][..]);
    if word.contains('(') || word.is_empty() {
        return false;
    }
    let primitives = [
        "str", "bool", "usize", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32", "f64",
        "char", "int", "float", "dict", "list", "tuple", "set", "bytes",
    ];
    primitives.contains(&word) || word.starts_with(|first: char| first.is_ascii_uppercase())
}

fn read_literal(raw: &str, yaml: bool) -> Lit {
    let text = raw.trim().trim_start_matches('&').trim();
    let head: String = text.chars().take_while(|c| !c.is_whitespace()).collect();
    let word = head.trim_end_matches(&[',', ';', ')', ']', '}'][..]);
    if matches!(word, "None" | "null" | "~" | "Option::None" | "nil") {
        return Lit::Null;
    }
    if matches!(word, "true" | "True" | "yes") {
        return Lit::Bool(true);
    }
    if matches!(word, "false" | "False" | "no") {
        return Lit::Bool(false);
    }
    if text.starts_with("Some(") {
        return Lit::Wrapped;
    }
    if text.starts_with('"') || text.starts_with('\'') {
        return Lit::Text(normalise(text));
    }
    for empty in ["BTreeMap::new()", "HashMap::new()", "Map::new()"] {
        if text.starts_with(empty) {
            return Lit::Map { empty: true };
        }
    }
    for opener in ["BTreeMap::from([", "HashMap::from([", "IndexMap::from([["] {
        if text.starts_with(opener) {
            return match balanced(&text[opener.len() - 1..], '[', ']') {
                Some(inner) => Lit::Map {
                    empty: inner.trim().is_empty(),
                },
                // The value runs onto the next line: unreadable, not empty.
                None => Lit::Expr,
            };
        }
    }
    for empty in ["Vec::new()", "vec![]", "[]"] {
        if text.starts_with(empty) {
            return Lit::Seq(Vec::new());
        }
    }
    for opener in ["vec![", "["] {
        if text.starts_with(opener) {
            return match balanced(&text[opener.len() - 1..], '[', ']') {
                Some(inner) => {
                    let found = elements(inner);
                    if found.iter().any(|element| looks_like_a_type(element)) {
                        // `&[&str]` in a signature is a type, not a list.
                        Lit::Type
                    } else {
                        Lit::Seq(found.iter().map(|element| normalise(element)).collect())
                    }
                }
                None => Lit::Expr,
            };
        }
    }
    if text.starts_with('{') || text.starts_with("json!(") || text.starts_with("dict(") {
        return match balanced(text, '{', '}') {
            Some(inner) => Lit::Map {
                empty: inner.trim().is_empty(),
            },
            None => Lit::Expr,
        };
    }
    if let Ok(number) = word.replace('_', "").parse::<f64>() {
        return Lit::Number(number);
    }
    if let Some(captured) = ENUM_PATH.captures(word) {
        return Lit::Text(snake(&captured[2]));
    }
    if looks_like_a_type(word) {
        return Lit::Type;
    }
    if word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !word.is_empty() {
        // In a config file `post_as: text` is the string "text"; in Rust the
        // same shape is a variable, and the gate has to go and find it.
        return if yaml {
            Lit::Text(word.to_owned())
        } else {
            Lit::Word(word.to_owned())
        };
    }
    // A YAML plain scalar: no quotes, no brackets, no call.
    if word == text && !word.is_empty() && !word.contains(['(', ')', '[', ']', '{', '}', '!', '?'])
    {
        return Lit::Text(normalise(word));
    }
    Lit::Expr
}

/// A `NAME = <literal>` in the same file, for a value that is a constant.
fn resolve(name: &str, source: &Source) -> Option<Lit> {
    let pattern = Regex::new(&format!(
        r"(?:^|[^A-Za-z0-9_])(?:const |let |static )?{}(?:\s*:[^=]*)?\s*=\s*(\S.*)$",
        regex::escape(name)
    ))
    .expect("a name is a literal pattern");
    for (_, text) in &source.lines {
        if let Some(captured) = pattern.captures(text) {
            let literal = read_literal(&captured[1], false);
            if !matches!(literal, Lit::Word(_)) {
                return Some(literal);
            }
        }
    }
    None
}

// -- comparing it with the default -------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Verdict {
    /// This assignment turns the knob off its default.
    Turned,
    /// It writes the default back down: the knob is still untested.
    Default,
    /// The value cannot be read here. Never a pass.
    Unreadable,
}

fn json_elements(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            other => normalise(&other.to_string()),
        })
        .collect()
}

fn compare(lit: &Lit, default: &Value) -> Verdict {
    let same = |yes: bool| {
        if yes {
            Verdict::Default
        } else {
            Verdict::Turned
        }
    };
    match (default, lit) {
        // A default of null is the one case an unreadable expression settles:
        // whatever `str(path)` is, it is not `None`.
        (Value::Null, Lit::Null) => Verdict::Default,
        (Value::Null, Lit::Word(_) | Lit::Type) => Verdict::Unreadable,
        (Value::Null, _) => Verdict::Turned,
        (Value::Bool(shipped), Lit::Bool(found)) => same(shipped == found),
        (Value::Number(shipped), Lit::Number(found)) => {
            let shipped = shipped.as_f64().unwrap_or(f64::NAN);
            same((shipped - found).abs() < 1e-9)
        }
        (Value::String(shipped), Lit::Text(found)) => same(shipped == found),
        (Value::Array(shipped), Lit::Seq(found)) => same(&json_elements(shipped) == found),
        (Value::Object(shipped), Lit::Map { empty }) => {
            if shipped.is_empty() == *empty {
                same(true)
            } else {
                Verdict::Turned
            }
        }
        // Everything else - a value that cannot be read, and a null written
        // for a knob whose default is not null (a declaration, or a Python
        // default argument, being misread) - is never a pass.
        _ => Verdict::Unreadable,
    }
}

// -- which knob a hit belongs to ---------------------------------------------

/// Which of several same-named knobs a hit belongs to: the section named
/// nearest above it wins - `OpenAiCompatBrainConfig {` over a struct literal,
/// `openai_compat:` over a block of YAML. None when nothing above it says,
/// and then the hit counts for none of them.
fn section_of(candidates: &[&String], source: &Source, at: usize) -> Option<String> {
    let sections: Vec<(String, String)> = candidates
        .iter()
        .filter_map(|path| {
            let mut parts: Vec<&str> = path.split('.').collect();
            parts.pop();
            let section = squash(parts.last()?);
            Some(((*path).clone(), section))
        })
        .collect();
    for index in (0..=at).rev() {
        let text = &source.flattened[index];
        for (path, name) in &sections {
            if text.contains(name.as_str()) {
                return Some(path.clone());
            }
        }
    }
    None
}

// -- the gate ----------------------------------------------------------------

/// What the scan made of one knob.
#[derive(Default)]
struct Coverage {
    turned: Vec<String>,
    ignored: Vec<String>,
}

fn coverage_of(
    path: &str,
    default: &Default_,
    candidates: &[&String],
    hits: &[Hit],
    sources: &[Source],
) -> Coverage {
    let mut coverage = Coverage::default();
    for hit in hits {
        let source = &sources[hit.source];
        if candidates.len() > 1 && section_of(candidates, source, hit.at).as_deref() != Some(path) {
            continue;
        }
        let mut lit = read_literal(&hit.value, hit.yaml);
        if let Lit::Word(name) = &lit
            && let Some(resolved) = resolve(name, source)
        {
            lit = resolved;
        }
        let verdict = match default {
            // Nothing to be left at: the operator has to write it down, so any
            // real value counts - but a type in a declaration is not one.
            Default_::Operator if lit == Lit::Type || hit.value.trim().is_empty() => {
                Verdict::Unreadable
            }
            Default_::Operator => Verdict::Turned,
            Default_::Is(value) => compare(&lit, value),
        };
        match verdict {
            Verdict::Turned => coverage.turned.push(hit.place(sources)),
            Verdict::Default | Verdict::Unreadable => coverage.ignored.push(format!(
                "{} [{}]",
                hit.place(sources),
                if verdict == Verdict::Default {
                    "the default"
                } else {
                    "unreadable"
                }
            )),
        }
    }
    coverage
}

fn describe(default: &Default_) -> String {
    match default {
        Default_::Operator => "no default: the operator supplies it".to_owned(),
        Default_::Is(value) => format!("default {value}"),
    }
}

#[test]
fn every_knob_in_the_schema_is_turned_by_some_test() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources = test_sources(root);
    let hits = scan(&sources);
    let inventory = inventory();

    let mut by_leaf: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for path in inventory.keys() {
        let leaf = path.rsplit('.').next().expect("a path has a last segment");
        by_leaf.entry(leaf).or_default().push(path);
    }

    let mut failures: Vec<String> = Vec::new();
    let mut turned = 0usize;
    for (path, default) in &inventory {
        let leaf = path.rsplit('.').next().expect("a path has a last segment");
        let candidates = &by_leaf[leaf];
        let found = hits.get(leaf).map_or(&[][..], Vec::as_slice);
        let coverage = coverage_of(path, default, candidates, found, &sources);
        if !coverage.turned.is_empty() {
            turned += 1;
            continue;
        }
        let mut report = format!("  {path} ({})", describe(default));
        if coverage.ignored.is_empty() {
            report.push_str("\n      never assigned in any test");
        } else {
            for place in coverage.ignored.iter().take(4) {
                report.push_str("\n      ");
                report.push_str(place);
            }
        }
        failures.push(report);
    }

    println!(
        "{} knobs in the schema, {turned} turned off their default by a test",
        inventory.len()
    );
    let names: BTreeSet<&String> = inventory.keys().collect();
    assert!(
        names.contains(&"brain.openai_compat.api_key".to_owned()),
        "the inventory lost the knob this gate was written for"
    );
    assert!(
        failures.is_empty(),
        "{} of {} config knobs are never set to anything but their default, so no test in this \
         repository can tell whether they do anything:\n\n{}\n\nGive each one a test that sets it \
         to a value it does not ship with AND asserts what changes - in the module that owns the \
         behaviour, with a literal the reader of this gate can see (a bare constant from another \
         file cannot be read). What this scan can and cannot see is at the top of {SELF_PATH}.",
        failures.len(),
        inventory.len(),
        failures.join("\n")
    );
}
