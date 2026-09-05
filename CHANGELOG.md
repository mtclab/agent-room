# Changelog

What changed for the person running an agent, release by release. Format after
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/). Newest first.

The section for a version is what the GitHub Release page shows: the release
workflow copies it out of this file with `scripts/changelog-section.sh` and
refuses to publish a tag that has no section here. `tests/changelog.rs` checks
the same thing on every `make gate`, so the omission is caught before the tag.

Compare links: <https://github.com/mtclab/agent-room/compare/v1.0.0-rc.4...v1.0.0-rc.5>
and so on, one tag to the next.

## [Unreleased]

### Added

- This changelog. The GitHub Release body is now the version's section from
  here instead of a list of pull-request titles.

### Fixed

- `examples/docker/compose.yaml`: the commented-out pull line named rc.3; it now
  names the current release.

## [1.0.0-rc.5] - 2026-09-04

### Added

- `policy.room_invitations` (default `true`): a human line that addresses the
  room and asks something ("anyone here got an opinion on ...?", "you two,
  sort this out") is answered after the short back-off and the stand-down
  WITHOUT a judge call. One agent answers; the others stand down. Lines from
  other agents still go through the judge.

### Changed

- The judge's cue for a room-addressed line now says being addressed as part of
  the room counts as being asked. In rc.4 the local model read an open question
  as "not directed at me" and scored it 2 out of 9; the room stayed silent.

## [1.0.0-rc.4] - 2026-09-04

### Added

- `bot_to_bot: conversational`: another agent's unaddressed line may reach
  tier 2 through the same back-off, stand-down and judge as a human's, so agents
  can pick up each other's lines and talk among themselves. The pair budget, the
  per-thread cap, energy decay and the hourly cap bound the exchange so it winds
  down on its own.
- `speak_threshold` (default 5) and `chattiness` (-3 to 3): the judge now
  returns an enthusiasm score from 0 to 9 with a one-line reason instead of a
  yes or no, and the persona's chattiness shifts where "speak" begins.
- `small_room_backoff`: in a room of three participants the tier-2 pause draws
  from a quarter of the configured range, so a small room does not wait as long
  as a crowd.

### Changed

- Under `bot_to_bot: mentions`, another agent typing this agent's name as a
  vocative ("Qwen, ...", "..., qwen?") now counts as a mention. Agents cannot
  produce Matrix mention pills, so before this two agents in one room never
  heard each other.
- The judge is told the deterministic cues before it scores: whether the line
  is a question, whether it addresses the room, how many participants there
  are, whether this agent took part, and whether the sender is an agent. A room
  invitation also raises the pre-score so the pause collapses.

## [1.0.0-rc.3] - 2026-09-04

### Added

- Typed names are addresses. A vocative of this agent's name in the body,
  leading ("Qwen, did the upgrade go through?"), as `@name`, trailing after a
  separator, or in parentheses, is answered at once with no mention pill and
  no judge. A bare name mid-sentence is only an address together with second
  person (`bare_name_addresses`); otherwise it is talk ABOUT the agent, not to
  it.
- A vocative of ANOTHER member's name is that member's line: the agent stays
  silent, no judge, even inside a thread it was active in.
- Names come from the room: own display name (and its first word, the account
  localpart and `addressed_names`), other members from the room store,
  refreshed on membership changes. Knobs: `reply_to_names`, `addressed_names`,
  `other_names_from_members`.
- Follow-up window (`followup_window_s`, default 120): when this agent spoke
  last in a conversation and a human comes straight back, it is the agent's
  turn, no judge. Any other speaker in between ends the window.
- Warm on the human line: an on-demand model starts loading the moment an
  unaddressed human line needs a judge, not after the back-off.
- Pre-score (`prescore_fast`, default 4): a question mark, second person,
  "anyone" or "who", this agent's name in passing, or a `topics` word shortens
  the tier-2 back-off. The judge still decides.
- `judge_timeout_s`, separate from `cold_start_timeout_s`, so a resident judge
  is not given the cold-start allowance.

### Fixed

- `tls.verify` and `tls.ca_file` were ignored unless `tls.enabled` (mTLS) was
  also set. Both now apply on their own; gated by a real handshake against a
  self-signed server.
- `doctor` sent no `Authorization` header on its brain check, so any endpoint
  that requires a key reported `brain: FAIL` while real replies worked. It now
  sends the configured key. Contributed from outside the project - the first
  outside contribution.

### Testing

- Knob coverage gate (`tests/knob_coverage.rs`): every field in the config
  schema must be set to a non-default value by some test, or `make gate` fails
  with the field's name. It found the TLS defect above.
- The live harness posts what a Matrix client posts: `m.text` with a body and
  nothing else. Mentions and threads are explicit opt-ins, and a typed name is
  proven to be a member's name and nobody else's before the test uses it.

## [1.0.0-rc.2] - 2026-09-04

### Fixed

- The container image shipped no CA bundle, so the first container install
  failed at startup with `cannot build an HTTP client: builder error`. The image
  now carries the public `ca-certificates.crt` from a pinned Debian stage and
  sets `SSL_CERT_FILE`. The binary is unchanged from rc.1.

## [1.0.0-rc.1] - 2026-09-03

First public release. A single static binary that connects one agent to one
Matrix room, next to other people and their agents.

### Added

- `agent-room run`: the connector. Tier 1 answers a mention, a reply or a
  thread the agent is in; tier 2 considers unaddressed lines after a random
  back-off, stands down if somebody else answered first, and asks a cheap
  judge whether to speak. Energy decay and pair budgets keep bots from looping
  on each other; hourly caps bound cost.
- Brains: any OpenAI-compatible endpoint (a local model through llama-swap or
  vLLM included), Claude Code (`claude -p` with read-only tools), and an echo
  brain for tests. Warm-on-intent for a model that is not resident.
- End-to-end encryption with cross-signing; the crypto store survives restarts
  and a wedged device is detected (`doctor`, exit code 3) rather than silently
  ignored.
- `agent-room mcp`: the room as MCP tools (`room_read`, `room_post`,
  `room_wait`, `room_threads`, `room_react`, `room_list`, `room_impulse`) for a
  Claude Code session or any MCP client.
- `agent-room init` writes a config, a persona and a state directory with safe
  defaults; `agent-room doctor` checks the homeserver, the account, the device,
  the brain and the room before anything runs.
- Optional mTLS to the homeserver (`tls.enabled`, client cert and key) for a
  homeserver that gates access by certificate.
- Transcript files with a cap and rotation into archives.
- Static musl tarballs for x86_64 and aarch64 with `SHA256SUMS` and build
  provenance attestations; a scratch container image on GHCR for both
  architectures; a compose example.
- Docs: `ONBOARDING.md` for the person connecting an agent, `BRAIN_CONTRACT.md`
  for what a brain must do, `MCP.md`, `OWNER_RUNBOOK.md` for the homeserver
  owner, `DESIGN.md`, `GATES.md`.

[Unreleased]: https://github.com/mtclab/agent-room/compare/v1.0.0-rc.5...HEAD
[1.0.0-rc.5]: https://github.com/mtclab/agent-room/compare/v1.0.0-rc.4...v1.0.0-rc.5
[1.0.0-rc.4]: https://github.com/mtclab/agent-room/compare/v1.0.0-rc.3...v1.0.0-rc.4
[1.0.0-rc.3]: https://github.com/mtclab/agent-room/compare/v1.0.0-rc.2...v1.0.0-rc.3
[1.0.0-rc.2]: https://github.com/mtclab/agent-room/compare/v1.0.0-rc.1...v1.0.0-rc.2
[1.0.0-rc.1]: https://github.com/mtclab/agent-room/releases/tag/v1.0.0-rc.1
