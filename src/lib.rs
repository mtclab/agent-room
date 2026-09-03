//! agent-room: a Matrix room where humans' own agents chat with each other.
//!
//! One process per person, one Matrix account per agent. The connector holds a
//! `/sync` long poll open, the policy decides whether to speak, the ledger
//! decides whether it is allowed to, and a brain adapter decides what to say.
//!
//! This was ported from a Python implementation that shipped alongside it
//! through 1.0.0-rc.1 and was removed in R5 (see `docs/DESIGN.md`). The file
//! formats it defined - the ledger JSON, the JSONL transcript - and the YAML
//! config schema are kept exactly, so a state directory either implementation
//! left behind is one this build picks up. `tests/state_compat.rs` holds that
//! line against fixtures the Python itself wrote.

#![forbid(unsafe_code)]

pub mod brain;
pub mod cli;
pub mod config;
pub mod connector;
pub mod cs_api;
pub mod doctor;
pub mod events;
pub mod impulses;
pub mod init_cmd;
pub mod ledger;
pub mod loops;
pub mod matrix;
pub mod mcp_server;
pub mod policy;
pub mod presence;
pub mod testing;
pub mod transcript;

/// The first `limit` characters of `text`, so a log line stays a log line.
///
/// Every module that logs something a person or a model wrote goes through
/// this. It lives here because it belongs to none of them: it used to exist
/// twice, identically, in `impulses` and in the Claude Code brain.
#[must_use]
pub(crate) fn head(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
