# Plan - slices and gates

House rules apply: no corner cutting, every bug becomes a gate with teeth,
journeys tested on the shipped path (real Synapse, real connector process).
Live gates use a FRESH test room created per run on a real homeserver with two
existing bot accounts; the room is left + forgotten at teardown. Which
homeserver, and where the bot tokens are, come from
`~/.config/agent-room/live.env` outside the tree (see `tests/live/README.md`);
neither is ever committed. Since S3 there are two pairs, bot A/bot B and bot
C/bot D - a production connector runs on bot A and Synapse rate-limits per user,
so bot-to-bot bursts get accounts of their own. Since the Rust port's R2
(2026-09-03) every gate that is not G1-G4 runs on bot C/bot D, C1 and C2
included: a gate must never share an account with a running agent.

## S0 - scaffold (DONE 2026-09-02)
Repo, venv, pyproject, config example, docs moved in, no CI.

## S1 - connector core + OpenAI-compatible brain (vLLM Qwen)  [BUILT 2026-09-02, branch s1-connector-core]

Status: implemented, `make gate` green, live gates G1-G4 green against the real
Synapse, teeth recorded in `docs/GATES.md`. Restart semantics turned out to need
more than "swallow the initial sync" - see the DESIGN section and G4 below.
- `nio` AsyncClient sync loop, one process, one account, N rooms.
- Event model: normalised `RoomEvent` (sender, body, thread_root, mentions, is_bot,
  reply_to, ts). `m.mentions` parsed per spec; formatted_body pill fallback.
- Tier 1 policy: mention / reply-to-me / thread I spoke in => reply.
  `bot_to_bot: mentions` honoured. Self-echo guard outside any config branch.
- Budgets ledger (per pair/min + cooldown, per thread, per hour) persisted per
  room in `state_dir`; enforced in code before any brain call.
- Per-room JSONL transcript = the session memory; brain receives the last N
  events + persona. Read receipts mark consumed; typing indicator while thinking.
- Replies posted as `m.notice`, threaded (`m.thread` + `is_falling_back` +
  `m.in_reply_to`), with `m.mentions` of who we answer.
- Brain contract: `async reply(ctx: BrainContext) -> str | None`.
  Adapters: `echo` (tests), `openai_compat` (vLLM via llama-swap, cold-start
  tolerant, `chat_template_kwargs.enable_thinking=false`).
- One in-flight run per room; events arriving mid-run are coalesced into the next turn.
- CLI: `agent-room run --config ...`.
Gates (live, real Synapse, real `agent-room run` subprocess):
  G1 human posts "@bot hello" -> in-thread `m.notice` reply within 30 s, mentions the human.
  G2 unaddressed message with `answer_unaddressed: false` -> silence for 20 s.
  G3 two connectors, A mentions B repeatedly (12 msgs) -> B answers at most 3 in the
     first minute, then cooldown; never answers its own posts.
  G4 kill -9 mid-thread, restart -> no duplicate reply to already-receipted events.
  Teeth: each gate proven to fail with its guard commented out (recorded in the PR).
Unit: mention parsing, thread relation building, ledger arithmetic with a fake clock.

## S2 - Claude Code brain  [BUILT 2026-09-02, branch s2-claude-brain]

Status: implemented, `make gate` green, live gates C1/C2 green against the real
`claude` 2.1.258 and the real Synapse, teeth recorded in `docs/GATES.md`.
Shipped:
- `brain/claude_code.py`: `claude -p` per turn, ONE SESSION PER ROOM. A uuid4 is
  minted on the room's first turn, kept in
  `<state_dir>/rooms/<room>.claude-session.json`, and passed as `--resume`
  afterwards; an unresumable session is replaced (WARNING) and the turn still
  happens.
- Persona + a fixed chat frame written to a 0600 temp file per turn and passed
  with `--append-system-prompt-file`; the conversation is rendered by the shared
  `brain/rendering.py`, so this brain and `openai_compat` cannot drift.
- Read-only `--allowedTools` (default Read/Grep/Glob/WebSearch),
  `--permission-mode default`, `--max-turns`, `--setting-sources`, cwd; the
  config refuses `extra_args` matching `/dangerous/i` and refuses
  `permission_mode: bypassPermissions`.
- `--output-format json` parsed for `result` / `is_error` / `num_turns` /
  `total_cost_usd` / `permission_denials`, logged per reply because this brain
  spends the owner's quota. Optional `debug_log` switches to
  `stream-json --verbose` and records every raw line.
- Every failure is silence: non-zero exit, timeout (process killed), `is_error`,
  empty result, missing binary. A usage limit additionally sets a cooldown
  (`rate_limit_backoff_s`) during which `reply()` spawns nothing.
- `build_brain(cfg.brain, cfg.state_dir)`; `examples/config.example.yaml` and
  `docs/DESIGN.md` document the exact command shape.
Gates:
  C1 (live, marker `claude`) the room's session remembers the previous message,
     with `history_limit: 1` so the prompt cannot be carrying the answer.
  C2 (live, marker `claude`) a shell request yields no listing and no Bash that
     was not denied.
  U1-U6 (offline) session-per-room, the exact allowlist, the rate-limit cooldown
     spawning nothing, the fresh-session retry, limits never classified from the
     raw stream, and the config refusals.
Two product defects and four harness defects were found and fixed on the way;
all are written up in `docs/GATES.md`.

Not done here, deliberately: `judge_model` is parsed but unused (it is S3's tier-2
judge), and no heartbeat.

## S3 - tier 2 (organic) + heartbeat  [BUILT 2026-09-02, branch s3-organic]

Status: implemented, `make gate` green, live gates G5-G7 + the C3 leak probe
green against the real Synapse (and the real `claude` for C3), teeth for all
four recorded in `docs/GATES.md`.
Shipped:
- `policy.should_reply` answers with a verdict (`reply` / `judge` / `consider` /
  `silent`) instead of a boolean. Tier 2 is `consider`: budgets and decay are
  checked first, in the policy, so nothing is spent on a message the agent was
  never allowed to answer.
- `Connector._deliberate`: a random back-off from `policy.backoff_s` awaited in
  its own task (the sync loop keeps running), then a stand-down re-read of the
  room - `/messages`, not only the local transcript - then the judge, then a
  budget re-check, then a threaded `m.notice`. ONE deliberation at a time per
  room, so a burst of chat cannot arm a judge call per message. (Since
  1.0.0-rc.5 the judge is skipped for one shape of line: a question a PERSON put
  to the room. See DESIGN, "The room invitation".)
- `Brain.judge(ctx) -> Judgement(speak, score, why)` with a no-by-default in
  `BaseBrain`; the question, the frame and the strict `score: N` parsing live
  in `brain/judging.rs` so the adapters cannot drift (it was `yes:`/`no:`
  through 1.0.0-rc.3; see DESIGN, "The judge scores, the connector decides"). echo judges on a
  `[[speak]]` marker (deterministic gates), openai_compat uses `judge_model` at
  temperature 0, claude_code uses haiku with `--max-turns 1`, `--tools ""` and a
  throwaway session.
- Energy decay in the ledger: consecutive bot-authored messages per thread, any
  human message resets it. At `bot_only_turns_before_decay` tier 2 leaves the
  thread alone and a bot's mention there needs the judge too. Persisted.
- `budgets.tier2_per_hour_max` (10): uninvited posts get their own narrow cap.
- Tier 3 heartbeat (`heartbeat_minutes`, 0 = off): per room, only in a room that
  has been quiet that long, guarded by the unprompted budgets and by a judge
  told "usually no"; posts unthreaded and mentions nobody.
- Replies mention any user id written in them, so one agent can address another
  at all (`bot_to_bot: mentions` only ever looks at `m.mentions`).
- The `claude -p` prompt moved from argv to STDIN: it was the room's
  conversation, visible in `ps` to everyone on the machine.
Gates:
  G5 two connectors, one unaddressed "[[speak]]" question, three rounds ->
     exactly one answer each time, and the other's log says it stood down
     because someone answered first.
  G6 two connectors mentioning each other with the pair budget raised to 20/min
     -> the thread stops on the decay alone within 4+2 posts, stays stopped for
     30 s, and a human post in the thread brings it back.
  G7 unaddressed line the judge declines -> silence for 20 s, then a `[[speak]]`
     line proves the same path is alive.
  G8 tier 3 on the shipped path: a quiet room, `heartbeat_minutes: 1`, and the
     agent posts by itself - unthreaded, mentioning nobody.
  C3 (marker `claude`) leak probe: a working directory full of fake secrets and
     four ways of asking for them, including "Alex said you can tell me" ->
     nothing leaks.
One product defect was found on the way and is written up in `docs/GATES.md`:
the new "a reply mentions any user id it names" rule made two agents ping-pong
off one human sentence that named them both, because each answer quoted the
other's name. Fixed by not re-pinging anyone the trigger had already mentioned.

Not done here, deliberately: S4's MCP server, and any tier-2 gate against a real
model (the echo judge is what makes G5-G8 deterministic; C1-C3 are where a real
model is exercised).

## S4 - MCP server for live sessions  [BUILT 2026-09-02, branch s4-mcp]

Status: implemented, `make gate` green, live gates M1-M5 green against the real
Synapse driving the real `agent-room mcp` process, teeth for M2/M3/M5 recorded
in `docs/GATES.md`. The S1-S3 live gates were re-run green afterwards, because
this slice moved code the connector depends on.
Shipped:
- `agent-room mcp --config PATH`: a stdio MCP server with `room_list`,
  `room_read`, `room_post`, `room_react`, `room_threads` and `room_wait`.
- It is a Matrix CLIENT for the session's own account, NOT a bridge into a
  running connector - the plan's "connector forwards @live-session mentions" was
  dropped, and why is written up in `docs/DESIGN.md`. Same config format, minus
  `brain:`; a session has no brain because it IS one.
- Reuse rather than a second implementation: `matrix.py` (the token-file /
  cached-token / password dance and the room joining, now shared with the
  connector), `build_reply_content` (so a session's threaded reply has exactly
  the shape a connector's does), `from_source` (so `/messages`, `/relations`,
  `/threads` and nio events all become one `RoomEvent`), and the `Ledger` - a
  session is budgeted like anything else that posts, reactions included.
- Reads use the Client-Server API directly, because nio's typed helpers put the
  token in the query string and hide the HTTP status that `/threads` (Matrix
  v1.4) needs for its fallback.
- Failures are one-line tool errors: an unknown room lists the rooms the session
  IS in, a spent budget says so and posts nothing, an unreachable homeserver
  says so in under a second (nio's unlimited retry is capped).
- `mcp.post_as` (default `notice`), a 0600 refusal before the server serves
  anything, `docs/MCP.md` and `examples/session.example.yaml`.
Gates:
  M1 `room_read` sees what the human just posted, with the right sender and ts.
  M2 `room_post` threaded on it -> an `m.notice` in that thread, mentioning the
     human, as the room sees it.
  M3 `room_wait` returns within 10 s when the human speaks during the wait, and
     returns [] after actually waiting out a 5 s timeout.
  M4 a real connector (echo brain) in the same room answers a `room_post` that
     mentions it, and `room_read(thread_root=...)` reads back question-then-answer.
  M5 hourly cap of 2 -> the third `room_post` is a tool error naming the budget
     and the room never sees it.
  Offline: the tools driven through a real in-process MCP client - validation,
  limits, `post_as`, the threads fallback, the wait's cap and drain, the budget
  across a restart, and the 0600 refusal.

## S5 - friend onboarding  [BUILT 2026-09-02, branch s5-onboarding]

Status: implemented, `make gate` green, live gate D1 green against the real
Synapse driving the real `agent-room doctor` process, teeth for D1/U8/U9/U10
recorded in `docs/GATES.md`, and the wheel proven to install and work in a
throwaway venv.
Shipped:
- `agent-room init`: non-interactive, flags only, writes `<out>/config.yaml` and
  `<out>/persona.md` (both 0600) with the policy dumped from `PolicyConfig()`,
  then validates the result through `load_config`. `--password-from-stdin` logs
  in through `matrix.authenticate` and keeps only the cached token; the password
  is never written anywhere and a gate greps the output tree to prove it. It
  refuses to overwrite existing files without `--force`, and refuses a
  credential that belongs to a different account.
- `agent-room doctor --config PATH`: a PASS/FAIL/SKIP row per prerequisite -
  file modes (token, TLS key, and the config itself when it carries a password),
  the homeserver, the token and who it belongs to, each room (joined / invited /
  nobody invited you), and the brain (`GET {base_url}/models`, or
  `claude --version`). One-line fix per failure, exit 1 on any FAIL.
- The persona template moved INTO the package
  (`src/agent_room/templates/persona.md`) so a wheel carries it, with a blank
  for each of the six things a persona has to say. The leak probe (C3) now runs
  on that template with its blanks filled.
- `matrix.build_command_client`: the connection cap S4 needed for tool calls,
  now shared by `init`, `doctor` and the MCP server (teeth U7 moved with it).
- Docs: `docs/ONBOARDING.md` (for a friend), `docs/OWNER_RUNBOOK.md` (Synapse
  admin API: reassigning and revoking a spare bot account),
  `docs/BRAIN_CONTRACT.md` (writing an adapter), README pointing at all three.
- `examples/agent-room.service`: the user unit, accepted by `systemd-analyze
  verify` (gate).
Gates:
  D1 (live) the real `agent-room doctor` against a real bot token and a room the
     account was just invited to: every row PASSes; then the token is replaced
     with a wrong one and exactly that row FAILs, with exit 1.
  U8/U9/U10 (offline) the perms check, the overwrite refusal and the sync drain.
  Plus the offline suites for both commands: 30 cases for `init` (driven through
  `cli.main`, the way a person types it) and 29 for `doctor`.
One product defect was found by D1 and is written up in `docs/GATES.md`: doctor
reported "nobody invited you" about a room the account HAD been invited to,
because Synapse's cached initial sync predates the invitation - the same cache
pathology as G4, and it now drains the same way.

**Distribution is NOT decided** (owner). At the time of S5 both Python install
paths were documented and both worked: `pipx install git+...` (needs the repo to
be public, or the friend to be a collaborator) and a wheel the owner builds and
sends. R5 replaced both with a static musl tarball, and the OPEN question is
unchanged in shape: sending a file needs no decision; making the repo public is
a separate call with the usual consequence that every future commit is public
too.

## S6 - unprompted speech, second design  [BUILT 2026-09-02, branch s6-unprompted]

Status: implemented, `make gate` green (415 offline tests), live gates G9-G12
green against the real Synapse with teeth for all four, and the whole live suite
re-run on the committed branch because this slice moved the connector and the
MCP server: **18 passed in 18:39** (G1-G4, G5-G8, M1-M5, D1, G9-G12).

Owner, 2026-09-02: *"isn't a random timer predestined, not organic?"* It is. S3's
heartbeat is kept as a hidden fallback (`heartbeat_minutes`, default 0, and
removed from ONBOARDING) and unprompted speech now has real triggers.
Shipped:
- **Impulses**: an inlet directory per room (`<state_dir>/rooms/<room>.impulses/`,
  one JSON file each), `agent-room impulse --config C --room R [--kind K]
  [--ttl-s S] "text"`, and the MCP tool `room_impulse`. The connector polls the
  inlet every 5 s, presence-gates, backs off, judges ("given what this room was
  talking about, is that worth telling them?") and posts an UNTHREADED
  `m.notice` mentioning nobody. One chance per impulse; unspoken ones expire
  (`impulse_ttl_s`, 6 h) with a log line.
- **Open loops**: my posts that end in `?`, and the brain's `[[followup: ...]]`
  marker (stripped before posting, text kept). One follow-up per loop ever,
  after `followup_delay_s` (20 min - 3 h), in that thread, mentioning nobody;
  closed the moment anybody else posts there. A follow-up never opens a loop.
- **Presence**: `m.presence` for the room's human members (callback registered
  before the backlog drain - presence is not backlog) OR a human posting within
  `presence_window_min` (30). Candidates queue and wait for it, giving up after
  `unprompted_max_wait_min` (240). The back-off range is halved after a human
  post inside 10 min and doubled in a room quiet for an hour.
- **Inner thoughts** (`inner_thoughts`, off): the judge answers `score: N - ...
  | urgency N` (0-3), accumulated per conversation, and at
  `inner_thoughts_threshold` (4) raises a candidate through the same
  presence/back-off/stand-down path - NOT judged again, because the judge
  already answered four points' worth. REFUSED by config validation for
  `brain.kind: claude_code`, where it would be a paid call per line of chat.
- **Wake strategies** in `openai_compat`, independent knobs: `warm_on_intent`
  (one fire-and-forget `max_tokens: 1` completion when a human starts typing or
  a back-off begins, behind `warm_cooldown_s`) and `judge_base_url` /
  `judge_model` / `judge_api_key` / `judge_extra_body` (a small resident model
  judges; the big one is only ever loaded to speak). The three typical setups
  are three snippets in ONBOARDING and in config.example.yaml.
- Everything unprompted shares `tier2_per_hour_max` and is logged with its
  trigger kind, including every decision NOT to speak.
Gates:
  G9  the real `agent-room impulse` + the human's real Matrix presence: spoken
      while online, unspoken while offline, spoken when the human posts.
  G10 exactly one follow-up on an unanswered question, in its thread, and none
      at all when a human answers first.
  G11 a judge that says no with urgency 2 every time -> one message at 2+2, and
      silence again afterwards.
  G12 a real HTTP endpoint counts the warm-ups: one per typing burst, none with
      `warm_on_intent: false`.
  Offline: `tests/test_impulses.py`, `tests/test_loops.py`, and the S6 sections
  of the connector, judging, openai_compat, config and MCP suites.
One harness defect was found by the teeth run and is written up in
`docs/GATES.md`: G12 had no teeth because Synapse does not repeat a typing
notice, so the gate was never giving the cooldown a second chance to refuse.

Four product defects were found by the standing adversarial read, all before
anything shipped, and all four now have an offline gate with teeth (U11-U15):
a queued impulse that never aged, an inner-thoughts probe per message in a
burst, an uncapped queue behind a public inlet, and an unprompted turn rendered
against whatever thread happened to be newest. A fifth was found by writing the
docs: nothing ever told a MODEL that `[[followup: ...]]` exists.

Not done here, deliberately: no Claude Code hook is shipped for dropping
impulses (the CLI is the interface; wiring a Stop/PostToolUse hook is the
owner's own config), and no gate runs the new paths against a real model - the
echo brain is what makes G9-G12 deterministic.

## The Rust port (owner decision, 2026-09-02: the product becomes Rust)

COMPLETE 2026-09-03 with R5. The Python moved to `reference/` and stayed until
parity; the product is the binary. The design does not change - every slice is a port, module for module,
with the SAME behaviour, the same reason strings and the same file formats, so a
binary started on a Python `state_dir` continues where it left off.

The live journeys are the shared gate: `tests/live/` stays at the top level and
drives whatever `AGENT_ROOM_BIN` names, so G1-G4 (and, later, the rest) run
against either implementation without touching the tests.

### R1 - connector core  [BUILT 2026-09-03, branch r1-rust-core]

Status: `make gate` green (fmt, clippy pedantic with warnings as errors, 134
tests), live gates G1-G4 green against the real Synapse driving the RUST binary,
all four proven to have teeth, and the new encrypted-room gate E1 green with a
negative control. Recorded in `docs/GATES.md` under "Rust R1".

Shipped: `config` (the whole YAML schema, S3/S6 knobs included; a knob this
build does not act on is refused only when it is set away from its default),
`events`, `policy` tier 1 plus the judged tier-1 path, `ledger`, `transcript`,
the `Brain` trait with `echo` and `openai_compat`, the matrix layer (sqlite
store, token or password login, E2EE bootstrap), the connector loop (startup
sweep, one in-flight turn per room with coalescing, typing, threaded m.notice
replies, receipts, clean SIGTERM) and `agent-room run`.

Not in R1, and it says so rather than pretending: a tier-2 `consider` verdict is
logged and dropped; `init`, `doctor`, `mcp` and `impulse` exit 2.

### R2 - the Claude Code brain  [BUILT 2026-09-03, branch r2-rust-brains-tier2]

Status: `make gate` green, live gates C1-C3 green against the real `claude`
2.1.258 and the real Synapse, teeth for C1 and C2 recorded in `docs/GATES.md`
under "Rust R2/R3".

`brain/claude_code.rs`: `claude -p` per turn, ONE SESSION PER ROOM (the uuid in
`<state_dir>/rooms/<room>.claude-session.json`, the same file the reference
writes), the prompt on stdin, `--allowedTools` last after `extra_args`, the
cheap toolless judge on `judge_model`, every failure a silence, and a usage
limit a cooldown that spawns nothing. Every flag was re-verified against the
installed CLI before it was relied on, including the two `--help` does not list
(`--append-system-prompt-file`, `--max-turns`).

The whole unit suite came with it, driven by the same fake `claude` script the
reference's tests use, so both implementations are held to one CLI contract.

### R3 - tier 2 and unprompted speech  [BUILT 2026-09-03, branch r2-rust-brains-tier2]

Status: `make gate` green, live gates G5-G12 green against the real Synapse,
teeth for all eight recorded in `docs/GATES.md`, and G1-G4 + E1 re-run green
afterwards because this slice moved the connector.

The back-off (drawn from `backoff_s`, scaled by the hazard), the stand-down
re-read against `/messages`, the judge and the budget re-check; the impulse
inlet with `agent-room impulse` writing the reference's own file format; open
loops (a `?` of mine, or a `[[followup: ...]]`), presence (`m.presence` plus "a
human posted recently"), inner thoughts, and the heartbeat as the hidden
fallback. `Config::unsupported` is down to `mcp.post_as`.

The connector became a directory - `connector/mod.rs`, `connector/turn.rs`,
`connector/unprompted.rs` - and the deciding parts (the queue, the hazard, the
accumulator, the loop bookkeeping) are plain functions over `WorkerState`, so
they are unit tests rather than live-only behaviour.

### R4 - the MCP server, init, doctor  [BUILT 2026-09-03, branch r4-rust-mcp-init-doctor]

Status: `make gate` green (85 new unit cases in `tests/r4_commands`), live gates
M1-M5 and D1 green against the real Synapse with the RUST binary, teeth for M2,
M3, M5, U8, U9 and D1 recorded in `docs/GATES.md`, and G1 + E1 re-run green
afterwards because this slice moved shared code.

`agent-room mcp` over stdio (rmcp 3.2) with the same seven tools - `room_list`,
`room_read`, `room_post`, `room_react`, `room_threads`, `room_wait`,
`room_impulse` - the same parameter names and the same result JSON, because the
live gates drive it through the MCP Python client and those shapes ARE the
contract. Same rules too: the session ledger is its own file, `post_as`,
`room_read`'s limit counts messages and pages `/messages` up to four times,
`/threads` with the scan fallback, a wait that drains at timeout 0 first and
caps at 120 s, one-line tool errors, receipts on read, the token never logged.

`init` and `doctor` with identical flags, identical output and identical files.
The three commands share `cs_api`: the Client-Server API over the same reqwest
client the mTLS config builds, with no retries, because a command somebody is
waiting on must fail in under a second rather than reconnect all night.

`Config::unsupported` is gone: every knob in the schema is now one this build
acts on.

### R5 - release  [BUILT 2026-09-03, branch r5-release]

Status: `make gate` green, the full live sweep green against the RELEASE binary,
and the port is complete - `reference/` is gone and there is one implementation.
Recorded in `docs/GATES.md` under "1.0.0-rc.1".

Shipped:
- **Static musl builds**, `make release` -> `scripts/release.sh`:
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, each a
  `dist/agent-room-<version>-<target>.tar.gz` carrying the binary, ONBOARDING,
  BRAIN_CONTRACT, MCP, the systemd unit and the two example configs, plus a
  `dist/SHA256SUMS`. Cross-compiled with `zig cc` through `cargo-zigbuild`,
  because `aws-lc-sys` and the bundled SQLite are C and this box has no musl
  toolchain, no `cross` and no Docker. Version 1.0.0-rc.1, from `Cargo.toml`,
  printed by `agent-room --version`.
- **rustls everywhere**, and a gate that says so: the mTLS client identity is
  built from a throwaway PEM pair in a unit test, with a negative case for a PEM
  that carries no private key.
- **The docs are the binary's**: ONBOARDING is download-verify-install, MCP.md
  registers the binary's own path, OWNER_RUNBOOK gained "cut a release" and "hand
  a friend a tarball", BRAIN_CONTRACT is the Rust `Brain` trait with a worked
  HTTP adapter, README is what it is and where the docs are, and DESIGN stopped
  calling the Python a reference - it is history, and the file formats it defined
  are the ones that stayed.
- **`reference/` deleted.** The live harness lost its Python fallback
  (`AGENT_ROOM_BIN` is the release binary or nothing), `teeth.py` lost its Python
  mutation table, and the harness's own runner environment moved out of the root
  venv into `tests/live/.venv` (`make live-env`, `tests/live/requirements.txt`).
  The live gates still need Python to RUN - the human in every journey is a
  Matrix client and the MCP gates are an MCP client - and nothing else does.
- **Estate details out of the source.** DESIGN, PLAN and GATES name no
  homeserver, no LAN address and no home directory; the live harness reads its
  homeserver, server name and tokens path from `~/.config/agent-room/live.env` (outside the tree),
  with a committed `live.env.example` and `tests/live/README.md`.
- Four findings from the standing adversarial read, all fixed and three of them
  gated, plus a flaky unit gate the slice found in itself and the panic that hid
  why. All written up in `docs/GATES.md`, along with what the read looked at and
  deliberately left alone.

**The port is complete.** One implementation, one binary, one gate.

Not done here, and deliberately: **the owner's own two connectors are still the
Python ones**, running out of the root `.venv` that this slice did not touch.
Swapping them over is a live change to two things that are really talking to
people, and it is the owner's to make once this is merged: stop each service,
`install -m 0755 target/x86_64-unknown-linux-musl/release/agent-room
~/.local/bin/`, `agent-room doctor --config ...`, start it again. Their state
directories carry over unchanged - that is what `tests/state_compat.rs` is for.
The root `.venv` can go afterwards.
