# Production readiness walk - 1.0.0-rc.1

Date: 2026-09-03. Walked by the coordinator after the opus builder was cut
three times by server-side errors. Binary under test: the x86_64 musl build
from `make release` on `readiness-walk` (sha256 prefix `12d5fb6f`, tarball
`143f6f32...`), rebuilt with path remapping. The verdict at the end is for the
binary that ships from this branch after the last commit.

## Fixed during the walk (each with a gate)

| # | Finding | Fix | Gate |
|---|---|---|---|
| F1 | rc.1 binary carried ~700 `/home/<builder>/.cargo/...` paths (rustc panic locations): the builder's login name shipped to every friend | `--remap-path-prefix` for CARGO_HOME and the repo in `scripts/release.sh`; release FAILS if a home path survives | `check_no_build_paths`; both targets now 0 hits |
| F2 | ONBOARDING had no backup section, no log guidance outside systemd, no word on transcript growth | sections added | doc |
| F3 | `init --room` refused every room id without a `:server` part - which is every room a current Synapse (room v12) creates, including ours. Step one of onboarding failed | validator accepts `!opaque`, `!id:server`, `#alias:server` | `init_accepts_a_room_id_without_a_server_part` |
| #12 | matrix-sdk logs its two E2EE bootstrap probes (404 secret-storage key, 404 backup) at ERROR on every fresh start | tracing layer drops exactly those two; all other SDK errors unchanged | unit gates both ways; live zero-ERROR startup in walk 3 |

## Walk 1 - the friend path, from the tarball

| Step | Result | Evidence |
|---|---|---|
| sha256 check as documented | PASS | `SHA256SUMS` matches |
| install to `~/.local/bin`, `--version` | PASS | `agent-room 1.0.0-rc.1` |
| `init` with the documented openai_compat flags (token file; the password path is untested - we hold no passwords) | PASS after F3 | config + persona written 0600, state dir 0700, "12 blanks left" |
| written defaults | SAFE | `tls.verify: true`, `bot_to_bot: mentions`, `answer_unaddressed: true`, `per_hour_max: 30`, `tier2_per_hour_max: 10`, `inner_thoughts: false`, `heartbeat_minutes: 0` |
| `doctor` | PASS | 4 PASS + the room row correctly FAILs "neither in it nor invited" with the fix line |
| systemd unit | verified by reading only | no user systemd instance on this box; unit uses the journal, SIGTERM + 30 s, restart on failure, NoNewPrivileges |

## Walk 2 - the live suite on the rebuilt binary

| Suite | Result | Note |
|---|---|---|
| `make live` (G1-G12, M1-M5, D1) | 15 passed, 3 failed, 1 error (727 s) | G5, G6, G7 "never became ready": the connector logged nothing for 60 s. They ran while a `make gate` compile saturated the host. |
| `tests/live/test_tier2.py` alone | 4 passed (222 s) | same binary, idle host: the failures were load, not code |
| `make live` again, after F3-F6 and the escape hatch, idle host | **18 passed (805 s)** | the binary that ships from this branch |
| W1 + W2 (device wedge) | 2 passed | refusal in 2.3 s with exit 3; tolerated run keeps the storm out of the log |
| `make live-e2ee` (E1 + control) | 2 passed (8 s) | |
| `make live-claude` (C1-C3) | 3 passed (129 s), ~$0.10 | |

Finding from the failed run (SHOULD-FIX, not fixed here): under heavy host
load the connector's startup is silent for over a minute - not even the
"starting" line is written. A friend on a small VPS would read that as a hang.
Log the first line before the store is opened, and log each startup phase.

## Walk 3 - outage and recovery

Fresh room, echo brain, bot D with its existing token and a FRESH state dir.

| Step | Result | Evidence |
|---|---|---|
| startup | ready in 2.0 s | "watching" logged |
| baseline mention | answered in 2.1 s | |
| SIGSTOP 3 min, two mentions posted meanwhile, SIGCONT | both answered within 6 s of resume | matches DESIGN: a pause is not a restart |
| 15 mentions in 15 s | 15 of 15 answered within 90 s, process alive, 2 rate-limit lines logged | no crash |
| mention one minute after the burst | answered in 2.1 s | |
| 20 minutes with no traffic, then a mention | answered in 2.1 s | sync loop alive |
| wrong homeserver port | ERROR + exit 1 after 30 s | walk 5 |
| ERROR lines over the whole run | **266** (WARN 396) | see F4 |

### F4 - the device wedge (was BLOCKER for the token path) - FIXED

Running an EXISTING access token against a FRESH state dir creates a new crypto
identity for a device the homeserver already holds keys for. The SDK then
tries to upload one-time keys on every sync, gets `400 One time key ... already
exists`, logs an ERROR each time (266 in 25 minutes on this walk) and E2EE for
that device never works again. R1 documented the mechanism; this walk showed
how easily a friend reaches it: lose `~/.local/state/agent-room`, or copy a
token to a second machine. The password path is immune (each login is a new
device).

Fixed on this branch:

- `matrix::device_check` compares the store's own curve25519 identity with the
  homeserver's `/keys/query` view of the device right after a token session is
  restored. A mismatch stops `run` with ONE line naming the cure and exit code
  3 (`DEVICE_WEDGED`), before any upload. Backstop: the SDK's
  `subscribe_to_duplicate_key_upload_errors` notification ends the sync loop
  the same way. Live W1: exit 3 in 2.3 s, 1 ERROR line, cure present.
- `doctor` has a `device` row: PASS (store matches, or nothing published yet),
  FAIL with the cure, SKIP before the first run.
- `allow_wedged_device: true` (default false) runs anyway for rooms that are
  NOT encrypted, with one WARN, and drops the SDK's one-time-key storm from the
  log. Live W2: runs, storm absent. The live-gate accounts need it: their
  devices have been wedged since the first fresh-store run in R1.
- ONBOARDING "Back it up": a token is bound to its state directory.

Remaining: the gate accounts (bots B, C and D) can only be made clean by the
owner issuing new devices (password reset, then a login); until then the gates
run with the escape hatch and the only clean-store control is the two
production bots, exercised at every swap.

### F5 - the pre-send encryption probe - FIXED

matrix-sdk fetches `GET /rooms/{id}/state/m.room.encryption/` inside its
`send_raw` span before every send; on an unencrypted room Synapse answers 404
"Event not found." and the SDK logged ERROR once per reply. The tracing layer
drops exactly that: target `matrix_sdk::http_client`, 404, that message, inside
a `send_raw` span. Unit gate: dropped inside the span, kept outside, other 404s
kept. The filter reads every event field, because the SDK reports request
errors in an `error=` field rather than the message.

### F6 - silent startup under load - FIXED

The "starting" line is now the first thing out, before the store is opened;
each phase then logs one line: store opened, authenticated (with device),
joined N of M rooms, encryption ready, backlog swallowed, watching.

## Walk 4 - security and privacy

| Check | Result |
|---|---|
| estate strings in the binary (`strings` for host, LAN, user, owner handle) | 0 after F1 (757 before, all cargo paths) |
| tarball contents | binary, ONBOARDING, BRAIN_CONTRACT, MCP, unit, two example configs; nothing else |
| tokens / passwords in logs | never logged; token travels in the header only (gate from S4) |
| message bodies at INFO | none: every body-bearing log call is DEBUG; INFO lines carry event ids and decisions |
| read-only Claude tools by default | yes (`Read, Grep, Glob, WebSearch`), `--permission-mode default`, no bypass flag accepted |
| leak probe C3 | passed on this binary (walk 2) |

## Walk 5 - operations

| Item | State |
|---|---|
| logs | journal under systemd; stdout otherwise (documented in F2) |
| transcript growth | ~630 B/message, ~5 MB per 10k messages, no rotation: issue #13. CLOSED after this walk (2026-09-03): the live file rolls at `transcript_keep` (5000) into `transcript_archives` (4) archives, gate T1 |
| upgrade path | same-version binary swap on the owner's two production bots kept the sqlite store, ledgers and Claude session (2026-09-03 08:51); rc -> 1.0 keeps the same file formats |
| backup | documented in F2 |
| wrong homeserver port | clear ERROR and exit 1 after 30 s, no hang |

## Verdict (2026-09-03, end of walk)

**Ready to hand to friends, with passwords.** The shipped binary passes every
gate on the shipped path (18 journey/MCP/doctor gates, the encrypted-room
gate, the three Claude gates, the two wedge gates), installs and configures
exactly as ONBOARDING says, fails clearly on the two easy mistakes (wrong
homeserver, reused token on a fresh state dir), survives a pause, a burst and
long idle, leaks nothing from the build machine or the room into the binary or
the log, and writes safe defaults. What stands in the way is small and named:
give friends a PASSWORD for their agent account (never a token) so a lost state
directory is one login away from clean; back up the recovery key; issue new
devices to the gate accounts so the live suite can drop `allow_wedged_device`;
transcript rotation (#13) before a room gets busy - done the same day, after this walk; the aarch64 build has been
built but never executed; the password login path is documented from the code
and the unit gates, not from a live run, because we hold no passwords.
