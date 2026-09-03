//! R4: the MCP server, `init` and `doctor`, in one test binary.
//!
//! One binary rather than three because they share the fake homeserver and
//! because three would link the crate three times for no gain. `#[path]`
//! because a test target's root file owns `tests/`, not `tests/r4_commands/`.

#![forbid(unsafe_code)]

#[path = "r4_commands/doctor.rs"]
mod doctor;
#[path = "r4_commands/fake_homeserver.rs"]
mod fake_homeserver;
#[path = "r4_commands/init.rs"]
mod init;
#[path = "r4_commands/mcp.rs"]
mod mcp;
