# Roadmap: from 1.0.0-rc.5 to 1.0.0

What each remaining release candidate has to prove before 1.0.0 is tagged, and
what deliberately waits until after it. Decided 2026-09-05 with the owner; the
detail of each version is re-planned from what the previous one showed in the
live room, so the later sections are the spine, not a promise of every line.

## What 1.0.0 means

- **Organic chat is complete and proven with real models.** Every mechanism
  that makes an agent speak or stay silent - addressing, follow-ups, room
  invitations, the scored judge, bot-to-bot conversation, impulses, open loops,
  presence, inner thoughts - has a live gate against a real model or a recorded
  soak in a room with three different brains, not only the echo brain.
- **Anyone on GitHub can run it** against their own homeserver: onboarding
  written for a stranger, a license, a security policy, a readiness walk done on
  a clean machine by someone who was not there when it was built.
- **A new brain is not a fork.** A generic `command` brain (JSON on stdin, reply
  on stdout) is the extension point; the in-tree brains are reference
  implementations of it.
- **Compatibility is a promise from here on:** state files and the config schema
  follow SemVer; a deprecation is announced one minor release before it bites.
- **License: AGPL-3.0**, committed before the tag.

## The cadence, every version

1. Slice PRs to `main`, each with unit gates, the relevant live gates, and the
   teeth proof (`docs/GATES.md`); `make gate` green.
2. `CHANGELOG.md` section written for the person running an agent; version bump;
   tag; CI releases; running hosts pull.
3. **Soak: at least three days in a live room** with three agents on three
   different brains (a local model, Claude Code, and a connector somebody else
   runs) on the same release. Anything the room shows becomes a fix PR with a
   gate in the SAME version before the next one starts.
4. The soak is recorded in `docs/GATES.md` under the version: what was asked,
   what happened, what was looked at and left alone.

Versions stay `1.0.0-rc.N`; the first tag without a hyphen is 1.0.0.

## 1.0.0-rc.6 - correct about what already exists

The code review that produced this roadmap found defects in what ships. They are
fixed before new behaviour goes on top.

Connector:
- Only conversational message types reach the brain. Today an image, a file or a
  location arrives with its filename or caption posing as a chat line; the filter
  exists and the connector does not use it.
- Edits (`m.replace`) replace the transcript line; `* corrected text` is never a
  new message.
- Redactions remove the text from the transcript and ledger. Today redacted text
  is re-fed to the brain as history until the transcript rolls.
- Room aliases (`#room:server`) work in `run` and `mcp`; today `init` and
  `doctor` accept them and `run` refuses the resulting config.
- An invitation received while running is joined, from a member of a room the
  agent is already in or from a listed user (`policy.accept_invites_from`).
- `agent-room mcp` refuses to serve an encrypted room instead of posting
  plaintext into it; `doctor` says why.
- Sync failures back off exponentially with jitter, one warning per state
  change; today it is a fixed five seconds and a warning per attempt.
- The ledger is written once per turn or interval, not fsynced on every event
  seen; a crash loses at most the debounce window, proven by a gate.

Doctor and supervisor:
- New rows: state directory exists, is private and writable; persona readable;
  judge endpoint reachable when one is configured; a warning when TLS
  verification is off; a warning when the loose-permissions escape hatch is in
  force (it also logs one at startup).
- The systemd unit stops restarting on exit 3 (device wedged) instead of looping
  every ten seconds; the compose example documents the equivalent.

Documentation debt closed: the design's open questions that were answered
months ago, the tool count, the "no CI" line, the MCP guide's claim that typed
names reach nobody, the gates note about the judge timeout.

## 1.0.0-rc.7 - organic chat, complete and proven with real models

- A `make live-real` gate set drives tier 2 and every unprompted path (impulses,
  open loops, presence gate, room invitations, bot-to-bot) against a real judge
  model. Opt-in, needs a model; never in CI. Today those paths are gated with
  the echo brain only.
- Inner thoughts on the OpenAI-compatible brain proven in the room; the default
  decided from the soak. Still refused for Claude Code (a paid call per line).
- Reactions as cheap participation: the brain may answer a middling judge score
  with a reaction instead of a line, and a reaction to the agent's own post
  counts as a signal. Off by default until the soak says otherwise.
- Collision reducer: two agents whose back-offs end within the sync round trip
  both answer today. Another agent's typing notice during the back-off is a
  stand-down.
- Formatted replies: the model's markdown becomes `formatted_body` with a safe
  HTML subset; the plain body stays for clients without HTML.
- A Claude Code hook example that drops an impulse when a session finishes a
  task, so the owner's agent has something of its own to say.

Exit: three heterogeneous agents converse in the soak room for three days
without being told to, and every mechanism has a real-model gate or a recorded
observation.

## 1.0.0-rc.8 - brains without a fork

- `brain.kind: command`: the connector runs the configured program per turn,
  writes one JSON document to its stdin (occasion, persona, room, rendered
  history, the event, a deadline) and reads one JSON document from its stdout
  (a reply, or silence; a score and reason when judging; optional follow-up and
  reaction). Non-zero exit or timeout is silence; stderr goes to the log. The
  prompt never touches argv.
- `doctor` runs the command with a `doctor` occasion and expects an `ok`.
- `BRAIN_CONTRACT.md` rewritten around the command protocol; two reference
  scripts under `examples/brains/`, both exercised by a gate.
- Per-room persona, chattiness and topics (`rooms:` grows an object form);
  budgets stay global.

Exit: somebody adds an agent in a language of their choice without touching the
Rust, and two command brains soak in the room.

## 1.0.0-rc.9 - runs for months, for strangers

- `agent-room status`: posts, refusals and judge scores per room from the
  ledger's counters; optional `--health-addr` serving `/healthz` (sync age,
  device state) for a supervisor.
- JSON log format; `SIGHUP` reloads persona and policy without a backlog sweep.
- `agent-room owner ...` for a Synapse homeserver (labelled as such; other
  homeservers keep the runbook): reset a bot account's password, invite it, exempt
  it from rate limits, and list who holds which account from a private registry.
- Interactive `init` when run in a terminal (flags stay for scripts); an
  `install.sh` that picks the architecture, verifies `SHA256SUMS`, installs and
  checks `PATH`.
- Release hygiene: the test traffic generator leaves the release binary (a cargo
  feature); the state directory's parents are created private, not with the
  umask.
- Repository for strangers: `LICENSE`, `CONTRIBUTING.md`, `SECURITY.md`, issue
  templates; onboarding reworded for "a homeserver".
- Owner tasks: fresh devices for the live-gate accounts so the wedged-device
  escape hatch leaves the suite; an arm64 run on real hardware if any is at hand.

Exit: a second readiness walk, on a clean machine, as a stranger following the
docs, including the password login path that the first walk could not exercise.
Recorded in `docs/READINESS.md`.

## 1.0.0

- Readiness walk verdict "ready" with nothing left to fix; no open issues.
- `LICENSE` committed; the 1.0.0 changelog section written for a stranger; the
  compatibility promise stated in `README.md`.
- The tag without a hyphen: the container image's `latest` moves; the soak is
  repeated once on the final tag.

## After 1.0.0

Listed so nobody argues them back into 1.0:

- Per-room brain and budgets (persona per room ships in rc.8).
- Images and files to the brain (multimodal context).
- Streaming replies.
- A macOS release leg (nothing in the code blocks it; it needs a runner and a
  decision about what "a static binary you copy over" means there).
- Windows (blocked: the whole 0600/0700 security model is Unix permissions).
- A router or capability tier above tier 2.
- MSC4295 bot bounce limits.
- Packaging (deb, nix, brew).
- Named adapters (Anthropic Messages API, a webhook) as thin layers over the
  command protocol.
- `agent-room mcp` in encrypted rooms (a crypto store for the session client).
