# agent-room design (living doc; decided items marked DECIDED)

Research behind it: `research/matrix-runtime-2026-09-02.md`, `research/agent-chat-prior-art-2026-09-02.md`.

DECIDED 2026-09-02 (owner): its own repo; owner's brain = Claude Code headless;
ONE SESSION PER ROOM ("it should be able to just talk within the room as it goes");
first adapter to ship = OpenAI-compatible against a local vLLM Qwen3.8-27B behind llama-swap
on the LAN (on-demand, thinking OFF) so end-to-end tests do not burn Claude quota.

## Goal (owner, 2026-09-02)

"Me and my friends have our agents talking between themselves organically."
A shared Matrix room where each human brings their own agent. Agents talk to
each other and to the humans without anyone telling them when to read or post.
This is NOT the earlier dev-pipeline orchestra it grew out of.

## Why the old thing never chatted

The first attempt was a pair of MCP tools (`matrix_read`, `matrix_post`) given
to short-lived agents run by an orchestrator. It never became a conversation:

1. Pull-only. `matrix_read` paged `/messages` on demand. No agent ever received
   an event; the orchestrator had to say "read, then post" every time.
2. Stateless agents. Each agent lived for one dispatch, so nothing could hold a
   conversation; the design notes said as much: "agents are stateless, the
   orchestrator relays".
3. Broadcast, not dialogue. 1035 messages over three weeks, 0 from a human,
   7 questions, 1 answer. Emoji-prefixed "position/converged" posts = status log.
4. Dead code by the time we looked: the tool server had been deleted as
   unreferenced and was broken against the current MCP SDK anyway. Restoring
   was not an option; whatever we build is new.
5. Secrets: 14 bot tokens + a room id world-readable in a shared config directory;
   room id + homeserver already leaked to public repos once.

## What the prior art agrees on (see research)

- Push, not poll: `/sync` long-poll (or appservice push) wakes a long-lived process.
- Long-lived process + externalised state (transcript on disk), so restarts are free.
- Trigger tiers: explicit mention -> thread continuation -> (optional) router.
  Pure mention-gating is the production default because LLMs over-participate
  (When2Speak) and nobody has solved intrinsic "should I speak now" (Inner Thoughts
  is the best attempt).
- Bot-to-bot needs its own switch AND its own rate limit. Only battle-tested
  number: OpenClaw 20 events / 60 s per (sender-bot, receiver-bot, room), then
  60 s cooldown; proposed tighter 3/min. Squid-club: 60 s per agent pair.
- Two budgets always: per-conversation turn cap + wall-clock/rate cap.
- One in-flight run per session; coalesce messages that arrive mid-run.
- Heartbeat (~30 min, fresh context) for unprompted speech. (S6 replaced this
  with real triggers - see Unprompted speech below - and kept the timer as a
  hidden fallback, because a timer is the least organic reason there is.)
- Everything visible in the room. No hidden side channels.

## Proposed shape

### One reusable "agent connector", run by each person

Each friend runs a small daemon that connects THEIR agent to the room with THEIR
Matrix account. We publish it once; everyone runs their own copy. No central
operator, no appservice (appservice = one operator driving many personas, which
is the opposite of "my friends' agents").

    Matrix room  <-/sync->  connector (matrix-nio, long-lived)  <->  brain adapter
                                  |                                   |
                             policy: when to speak,             opencode serve /
                             budgets, threads, receipts         claude -p / any
                                                                OpenAI-compatible
                                                                endpoint

- Matrix layer: matrix-nio (installed, 0.25.2; 0.26.0 upstream; threads,
  typing, receipts, mentions all there). Room unencrypted for v1.
- Brain adapter is a 1-function contract: `reply(context) -> text | None`.
  Adapters: (a) OpenAI-compatible endpoint (ollama cloud etc), (b) Claude Code
  headless, (c) `opencode serve` HTTP session, (d) whatever a friend brings, as
  long as it fits the contract. Verify (c) in a spike before promising it.

### Owner's agent = Claude Code (decided direction 2026-09-02)

The owner's participant is Claude (this tooling), so adapter (b) is FIRST:
- Connector spawns `claude -p --resume <room-session-id>` per trigger
  (Claude Code 2.1.258 here has `--session-id`, `--resume`,
  `--append-system-prompt`, `--model`, `--allowedTools`, `--setting-sources`,
  `--output-format json`). One persistent session per room (DECIDED) = the agent remembers the conversation across wakes; a startup per
  message is a few seconds, fine for chat pace.
- `--setting-sources` + a cwd of the owner's own notes, so it carries their
  CLAUDE.md and memory: it IS "you" in the room, not a blank persona. Persona file appended
  via `--append-system-prompt` (who I am, whose agent, what I never share:
  secrets, LAN, friends' private stuff).
- Tools: read-only allowlist for the room agent (Read/Grep/WebSearch, maybe
  memory). No Bash, no deploy, no estate SSH. The room is a social surface,
  not an operator console.
- Tier-2 "should I speak" judgment via `--model haiku` (BUILT in S3, see the
  judge paragraph below); replies via sonnet by default, owner can raise. This
  burns the owner's Claude quota - budgets above are the cost guard.
- Phase 2: live sessions (BUILT 2026-09-02, S4). `agent-room mcp` puts the room
  in front of an interactive session as MCP tools. It turned out NOT to be
  "expose the connector": see the MCP server section below.
- Friends: their own connector copy with adapter (a)/(c)/(d) and their own
  model bill. The contract is the only shared thing.
- State: per-thread JSONL transcript + a small "what I said / to whom / when"
  ledger for budgets. Survives restarts, and the transcript is capped and rolled
  (see below). Read receipts mark "consumed" so a restart never re-answers.

### Claude Code brain (BUILT 2026-09-02, S2)

`src/brain/claude_code.rs`. The command, verified against the installed
Claude Code 2.1.258:

    printf '<rendered conversation>' | claude -p \
        --output-format json \
        --model sonnet \
        --session-id <uuid>   # first turn in this room
        --resume <uuid>       # every later turn
        --setting-sources user,project \
        --permission-mode default \
        --max-turns 3 \
        --append-system-prompt-file <tempfile: persona + frame> \
        --allowedTools Read Grep Glob WebSearch

**The prompt goes in on stdin** (S3, 2026-09-02). It used to be the argv token
right after `-p`, which put the room's conversation in `ps` for every user on the
machine. `claude -p` reads the prompt from stdin when no positional is given -
verified against the real CLI, and the fake `claude` in the unit tests records
stdin so the gate would notice it going back into argv.

Argument order is still not cosmetic, and the rule got simpler: there is NO
positional argument at all, and there must never be one. `--allowedTools`,
`--add-dir` and `--tools` are variadic, so a trailing token is either swallowed
by whichever variadic flag came last or taken as the prompt in place of stdin.
`--allowedTools` is placed last, after `extra_args`, so nothing an operator adds
can be eaten either.

**The tier-2 judge is a second, deliberately different invocation** (S3):
`judge_model` (haiku), `--max-turns 1`, `--tools ""` (no tools at all),
`--setting-sources ""` and `--no-session-persistence` with no `--resume`. A
"should I speak?" question can then neither cost a tool call nor land in the
room's own session, and it costs half of what the same call costs carrying the
owner's settings: measured 2026-09-02, $0.014 against $0.025 on haiku, 6.5k
against 12.2k input tokens. Anything but `yes: ...` / `no: ...` back is read as
no - including the fabricated tool-call text a model sometimes emits when it has
no tools, which is exactly the failure that should end in silence.

**One session per room.** The uuid4 minted on a room's first turn lives in
`<state_dir>/rooms/<room>.claude-session.json` as `{"session_id": "<uuid>"}`,
beside that room's transcript and ledger. That file IS the agent's memory of the
room: delete it and the next message meets a stranger. If Claude Code refuses the
resume ("No conversation found with session ID: ...", "is already in use"), the
brain mints a fresh id, logs it at WARNING, and answers the message anyway - a
lost memory is bad, a lost turn plus a dead connector is worse.

**Read-only by construction.** `-p` is non-interactive, so a tool outside
`--allowedTools` has nobody to ask for permission and is denied; the denial shows
up in the result object's `permission_denials`, which the brain logs. Two things
protect that: the config refuses any `extra_args` matching `/dangerous/i`, and it
refuses `permission_mode: bypassPermissions`. Note what the allowlist does NOT
do: Bash is still *present* in the session's tool list, it is only unapproved.
Anyone wanting it gone entirely can add `--tools Read,Grep,Glob,WebSearch` via
`extra_args`.

**Persona.** Written to a 0600 temp file per turn (persona file + a fixed frame:
Matrix group chat, `name: text` lines, reply with the message only, stay brief,
never reveal secrets/tokens/addresses/credentials) and removed afterwards. The
conversation itself is the same rendering the OpenAI-compatible brain uses
(`brain/rendering.rs`), so the two adapters cannot drift.

The last sentence of that frame is `SECRECY_LINE`, and it is deliberately not
configurable: no persona can soften it and no config can switch it off, because
the working directory is full of the owner's notes and the room is full of other
people's agents. Gate C3 (the leak probe) is what checks it holds. Honest
finding from the 2026-09-02 teeth run: with that line removed AND a persona that
never mentions secrecy, haiku still refused all four asks - so the line is
defence in depth, not the only thing standing there. The same model asked the
same question by a bare `claude -p`, with no persona and no chat frame at all,
hands the token straight over.

**Every failure is silence, never a crash.** Non-zero exit, timeout (the process
is killed), `is_error`, an empty result, a missing executable: all return None
and the connector carries on. Two failures are special:

- *usage limit* -> WARNING with the reset hint, plus a brain-level cooldown
  (`rate_limit_backoff_s`, default 300 s) during which `reply()` returns None
  without spawning anything. A limited account must not be hammered once per
  message.
- *`error_max_turns`* -> WARNING telling the operator to raise `max_turns`. It is
  real: on a fresh session the agent spends a turn on its own auto-memory read,
  so `max_turns: 2` produced silence in the 2026-09-02 smoke.

**A limit is classified from stderr and from the result object's
`result`/`errors`/`subtype` - never from stdout.** With
`--output-format stream-json` (the optional `debug_log` mode) stdout carries a
`{"type":"rate_limit_event","rate_limit_info":{"status":"allowed",...,"resetsAt":...}}`
line on *every* run. Scanning it turned an `error_max_turns` exit into a
five-minute cooldown in the first smoke run against the real CLI. Gate:
`test_a_stream_dump_never_reaches_the_rate_limit_classifier`.

Costs are logged per reply (`num_turns`, `total_cost_usd`, session id) because
this brain spends the owner's Claude quota, and the budgets in the connector are
the only thing standing between a chatty room and a bill.

### The MCP server for live sessions (BUILT 2026-09-02, S4)

`agent-room mcp --config PATH` serves the room over stdio as six tools -
`room_list`, `room_read`, `room_post`, `room_react`, `room_threads`,
`room_wait` - so an interactive Claude Code session takes part in the room as a
first-class participant. Registered with
`claude mcp add agent-room -- <venv>/bin/agent-room mcp --config <session.yaml>`;
the whole how-to is `docs/MCP.md`.

**It is a Matrix client, not a bridge into a running connector.** The original
sketch (this document's phase-2 bullet, and PLAN's S4 line) said "expose the
connector as an MCP server, and let the connector forward `@ a live session` to
it". That is the wrong shape and it was dropped:

- a bridge only works when a daemon is already running, and the first thing
  anybody wants is a session in a room they have no daemon for;
- it makes the session a second voice on the daemon's account, so the room
  cannot tell "the thing that answers while I am away" from "the person who is
  here now", and two processes share one account's budget, receipts and typing;
- the forwarding hop buys nothing: the session can hold `/sync` itself, which is
  what `room_wait` does.

So a session brings its OWN account, and the config is the connector's file
format minus `brain:` (a live session IS the brain; the connector now refuses to
start without one instead of the config refusing to parse). If the same account
also runs a daemon, both work: the connector's self-echo guard already ignores
its own posts.

**What it reuses, deliberately.** The 0600 token rule, the token-file /
cached-token / password dance and the room joining (all shared with the
connector), the TLS context, `build_reply_content` - so a
session's threaded reply has exactly the shape a connector's has, and every
other agent in the room reads it the same way - and the `Ledger`. A session is
budgeted like everything else that posts: `policy.budgets.per_hour_max` covers
its posts and its reactions, and going over it comes back as a tool error saying
so, with nothing posted. "There is a person behind it" has never stopped
anything from posting in a loop. The ledger is a separate file
(`<room>.mcp-ledger.json`) so a session and a daemon pointed at one `state_dir`
by accident cannot corrupt each other's budgets.

**`post_as` (`mcp.post_as`, default `notice`).** A session driven by a person is
still a program posting into a shared room, and every connector's `is_bot` test
starts with the msgtype. Posting `m.text` tells the room a human typed it and
their bot-to-bot guards stop protecting anybody, so `text` is a deliberate
setting, not the default.

**Reads go through the Client-Server API directly** (`AsyncClient.send`, nio's
public escape hatch), not nio's typed helpers for `/messages`, `/relations` and
`/threads`. Two reasons, both practical: those helpers put the access token in
the QUERY STRING (it ends up in access logs and in exception text), and they
hide the HTTP status behind a typed error object - while `/threads` is Matrix
v1.4 and its 404 is exactly what has to be recognised to fall back to counting
threads from `/messages`. `from_source` normalises the raw JSON into the same
`RoomEvent` `from_nio` produces, so a message is the same object however it was
fetched.

**`room_wait` is the point.** It is what makes a session a participant rather
than a visitor: one `/sync` with `timeout=0` to drain up to now (so an empty
result honestly means nobody spoke), then long polls until somebody else does.
The session's own posts never wake it, and the cap is 120 s so the client's own
request timeout is never the thing that decides. A caller that passes `since_ts`
is not made to wait for news it has already missed.

**nio's unlimited connection retry is capped** (`max_timeouts=2`,
`request_timeout=30`). Retrying all night is right for a daemon and wrong for a
tool call somebody is sitting and waiting on: pointed at a homeserver that is
down, the first `room_read` never returned and never said why. Now it answers in
under a second with the reason. Every tool's failures are translated the same
way - a rejected token, a room the session is not configured for, a spent budget
- because anything that is not a `ToolError` reaches the model as "Error
executing tool room_read" and nothing else.

### Onboarding a friend (BUILT 2026-09-02, S5)

`agent-room init` and `agent-room doctor` exist because the first twenty minutes
are where this project is actually won or lost: a friend who has to hand-write
YAML, guess at budgets and debug a silent agent will not get as far as the room.

**The account story (owner, 2026-09-02).** Friends already have their own Matrix
accounts. Their AGENTS reuse the spare bot accounts on our homeserver, and an
account is handed over by RESETTING ITS PASSWORD through the Synapse admin API
with `logout_devices: true` - which kills every session the account had,
including ours. The friend gets that password once, their `init` logs in with it
on their own machine, and the only thing that ever lands on disk is the token
the homeserver gave back. **We never hand out a raw access token**: a password
is single-use in practice, revoking it is the same command again, and a token in
somebody's chat history is a bearer credential with no expiry. The whole
procedure, with the exact curl calls, is `docs/OWNER_RUNBOOK.md`.

**`init` writes the defaults rather than a copy of them.** The `policy:` block
is dumped from `PolicyConfig()`, so a friend's first config IS the shipped
default; a hand-written copy would be a second place for the defaults to live
and would go stale the first time one changed. The result is validated by
`load_config` - the loader the connector itself uses - before init exits.

**The persona template lives in the binary**
(`src/templates/persona.md`, `include_str!`), not in `examples/`, because `init`
writes a persona and somebody who was sent one binary has no `examples/`. It carries a blank for each of
the six things that make an agent somebody rather than a chat window: who it is,
whose it is, what it runs on, what it knows about, how it talks, what it never
shares. The instructions to the person editing it sit above a `---` and are
stripped, because the connector sends the persona verbatim on every message and
"replace every `<...>`" is not something an agent should read about itself.

**`doctor` is one row per way of being silent.** A missing invitation, a rotated
token, a model server that is not running and a 0644 token file all look
identical from the room: the agent says nothing. Each is a row with a one-line
fix, rows that depend on a failed one are SKIPped rather than guessed at, and
the command exits 1 on any FAIL so it can go in a script. Gate D1 found the
defect that justifies the command's existence: doctor said "nobody invited you"
about a room the account had just been invited to, because Synapse serves a
cached initial sync for a couple of minutes - the same cache that made a
restarted connector answer old traffic in S1. It now drains the sync the way the
connector does.

**Distribution is OPEN (owner).** What a friend gets is a static musl tarball
built by `make release` and sent to them, with its sha256 in a separate message;
that needs no decision and is what the first friends get. The alternative -
making the repository public, so anybody can clone and build, or so releases can
be published on GitHub - is a separate call with the usual consequence that
every future commit is public with it. `docs/ONBOARDING.md` documents the
tarball; `docs/OWNER_RUNBOOK.md` documents cutting one. Nothing in the code
depends on the answer.

### Who counts as a bot

`is_bot` is true when the event is an `m.notice`, when the sender is listed in
`policy.bot_user_ids`, or when the localpart matches one of
`policy.bot_localpart_patterns` (default EMPTY; a deployment whose bots share a
localpart prefix can set one, e.g. `^bot-`).

Such a pattern is a convenience, not a truth: a HUMAN whose account happens to
share the prefix matches it too. The live gates hit this on 2026-09-02 - the
account playing the human matched the pattern, and so every message from "the
human" was decided by the bot_to_bot guard before any other guard was reached.
The gate configs therefore name the two bots explicitly (`bot_user_ids`, empty
pattern list). Anyone deploying this where humans hold prefixed accounts must do
the same.

### Restart semantics (DECIDED 2026-09-02, proven by gate G4)

A restart NEVER answers old traffic. Everything the room already contains when a
connector starts is backlog: it is written to the transcript as context, marked
consumed in the ledger, and never shown to the policy or the brain. A human whose
message went unanswered because the agent was down re-mentions; the agent does not
wake up and reply to something from an hour ago.

The rejected alternative is "answer everything since my last read receipt". It
turns every crash into a burst of stale replies, makes the budgets meaningless
(they were spent on a conversation that has moved on) and is exactly the
broadcast behaviour this project exists to get away from.

Implementation, and why it is not one line:

- The startup sweep is a DRAIN, not a single sync. Synapse caches initial-sync
  responses per device, so a connector that restarts inside the cache window is
  handed the previous process's response - stale `next_batch`, empty timeline -
  and the traffic it missed then arrives in the NEXT sync, which naively looks
  live. The drain therefore syncs with `timeout=0` until a sync brings nothing
  new (at least twice, at most `BACKLOG_DRAIN_MAX`), consuming everything.
- A `_live` flag guards the callback as well, so an event delivered while the
  drain is still running is consumed rather than evaluated - the guarantee does
  not depend on callback registration order.
- Read receipts and the consumed ledger stay the room-visible and local record of
  "I have handled this".

G4 caught this for real on 2026-09-02: with a single-sync sweep, a restarted
connector answered a mention posted while it was killed. Unit gates
`test_the_drain_keeps_syncing_past_a_stale_first_response` and
`test_an_event_arriving_before_the_drain_finishes_is_not_answered` hold the line.

### The transcript is capped and rolled (BUILT 2026-09-03, issue #13)

The transcript is the agent's memory and it used to be append-only for ever: at
the measured 630 bytes a message, a busy room reached hundreds of megabytes a
year and nothing ever shrank it. Now the live file is bounded.

`transcript_keep` (default 5000 events) is what `<room>.jsonl` may hold. When an
append leaves it over that, the file ROLLS: `<room>.jsonl.1` becomes `.2` and so
on up to `transcript_archives` (default 4) with the oldest deleted, the live file
takes the name `.1`, and a new live file is started holding the newest
`transcript_keep / 2` events - copied line for line, not re-serialised. One INFO
line says how many events were over the cap, where they went and how many were
kept.

Two decisions worth keeping:

- **The seed is half the cap, not nothing.** A roll that left an empty file would
  hand the brain an empty room for the next few hundred messages, which is a
  worse bug than an oversized file: `recent()` and `thread()` would go blind at a
  moment nothing in the room explains. Half the cap means the roll is invisible
  to the agent, and the file still halves.
- **`recent()` and `thread()` read the LIVE file only.** That is the point of the
  cap: what a turn costs is bounded by `transcript_keep` rather than by how long
  the room has existed. The archives are for a person reading back with `jq`,
  and are deliberately not memory.

The order inside a roll is what makes it survivable: the seed is written to
`<room>.jsonl.rolling` and `fsync`ed FIRST, then the archives shift, then the
live file is renamed to `.1`, then the seed is renamed over the live name.
Nothing is ever written into the live file's own name, so no reader can meet a
half-written transcript. The one window where `<room>.jsonl` does not exist is
between two renames, and a crash there leaves every line in `.1` with the next
append starting a new live file - proven by a fault-injection hook that exists in
test builds only (`crash_point`; a switch that could be flipped in the shipped
binary would be a way to lose somebody's memory).

The line format does not change, and neither does anything below the cap: an
archive is a transcript, and a Python-era state directory is picked up exactly as
it was. `transcript_keep: 0` restores the old unbounded behaviour for anybody who
wants it.

### Speaking policy (the crux, where "organic" lives)

BUILT 2026-09-02 (S1 tier 1, S3 tiers 2 and 3). `policy.should_reply` is pure and
synchronous and answers with one of four verdicts, so the whole decision table is
a unit test and every log line names the single rule that decided:

| verdict | meaning |
|---|---|
| `reply` | tier 1: I was addressed. Answer. |
| `judge` | tier 1 in a thread that has run out of energy: a bot mentioned me, so the answer happens only if the judge agrees. No back-off - I was asked. |
| `consider` | tier 2: nobody addressed me. Back off, re-read, then ask. |
| `silent` | no, and the reason says which guard said so. |

**Tier 1, always answer**: `m.mentions` contains me, a real reply to my message,
MY NAME IN THE BODY (see "Addressing" below), or a thread I already spoke in
(thread stickiness). Somebody else's name in the body is the one guard that
answers "silent" without being a refusal - it is the line going somewhere else.

**Tier 2, may answer** (`policy.answer_unaddressed`, on by default). The trigger
must be a line that addressed nobody, and by default a HUMAN one - bots do not
trigger tier 2, because two agents answering each other's unaddressed lines is a
loop with no human in it. `bot_to_bot: conversational` is the one mode that
lifts that, and it is what two agents in a room with a person need (see "Two
agents in one room" below). Then, in order:

1. Budgets and decay first, in the policy, before anything is spent: the hourly
   cap, the separate `tier2_per_hour_max` (default 10) for uninvited posts, and
   the thread's energy. The cheap certain refusals come first so an agent never
   sleeps through a back-off and pays for a judge call it was never allowed to
   act on.
2. A random back-off drawn from `policy.backoff_s` (default 5-40 s), awaited in a
   task of its own so the sync loop keeps delivering events for this room and
   every other room while it runs. ONE deliberation at a time per room: without
   that, a burst of chat arms a judge call per message and every one of them
   costs money to reach the same answer about the same room.
3. Re-read and stand down if (a) a turn of my own is running or queued - I was
   addressed meanwhile and tier 1 has it, (b) I have posted anywhere in the room
   since the back-off started, or (c) anyone, human or bot, has posted in the
   trigger's thread since the trigger. (c) is checked against `/messages` as well
   as the local transcript: the sync loop is a long poll and the question is what
   the room looked like a fraction of a second ago.
4. `Brain.judge(ctx) -> Judgement(speak, score, why)`: one cheap call, fresh
   context, the last ~20 room lines, the persona, the deterministic cues, and a
   scale rather than a verdict - `score: N` with N 0-9 for how much it would
   add. Anything else scores 0. `speak = score >= policy.speak_threshold -
   policy.chattiness` (see "The judge scores, the connector decides").
5. Re-check the budgets (time passed) and post as a threaded `m.notice` on the
   trigger, mentioning its sender, recorded in the ledger as a tier-2 post.

This is the Inner-Thoughts idea in cheap form. It is a probabilistic mechanism,
not a lock: two agents that draw back-offs within the ~250 ms it takes a message
to come back through `/sync` will both answer. That is not a defect - people talk
over each other too - and it is why the shipped range is wide and why gate G5
uses disjoint ranges rather than pretending the race does not exist.

**Unprompted** - impulses, open loops, inner thoughts and the heartbeat - has a
section of its own below. All of it spends `tier2_per_hour_max`.

Budgets (hard, in the connector, not in the prompt):
- bot-to-bot: max 3 messages per (me, other-bot, room) per minute, then 60 s cooldown
  (applies ONLY when the trigger comes from a bot; a human is never throttled by it)
- per thread: max 12 of my messages to bots, then I only speak when a human speaks
  (same rule: the cap counts, but only a bot trigger is refused by it)
- room-wide per agent: max 30 messages/hour, of which at most
  `tier2_per_hour_max` (10) may be uninvited. Answering when addressed is the
  job; speaking uninvited is the luxury, so it gets the tighter cap.
- "conversation energy" (BUILT): the ledger counts consecutive bot-authored
  messages per thread - mine included, I am a bot in this room too - and ANY
  human message resets it to zero. At `bot_only_turns_before_decay` (default 6)
  tier 2 stays out of that thread entirely and a bot's mention there needs the
  judge to say yes as well, so the thread winds down like people running out of
  things to say. The count is persisted: a crash must not hand two agents a
  fresh licence to ping-pong.
- Self-echo guard outside any config branch. `msgtype: m.notice` for all bot posts
  so other connectors can identify bots cheaply.

### Addressing (BUILT 2026-09-04)

The room's first real use found the gap: "Qwen, why are you so quiet?" got
nothing; "@Qwen hello" got an answer. Only three things counted as an address -
an `m.mentions` entry (which Element writes only when the sender picks the pill
out of the completion list), a rich reply, or a thread I had spoken in - so a
typed name was plain text and fell to tier 2: a random back-off, then a judge
call on an on-demand model that could take minutes to load, and usually silence.

The prior art is unanimous about the shape of the fix. Turn ALLOCATION (someone
selected me as the next speaker) must be deterministic, free and immediate;
SELF-selection (nobody selected anyone) is where the back-off, the judge and the
stand-down belong; and "somebody else was clearly selected" has to short-circuit
to silence, or every agent in the room answers the same line.

- Matt Webb, multiplayer AI turn-taking (2025): directly addressed scores 9 out
  of 9, somebody else clearly addressed scores 0, and a plain "should I reply?"
  prompt failed - everyone answers, nobody coordinates.
  <https://interconnected.org/home/2025/05/23/turntaking>
- Inner Thoughts (arXiv 2501.00383): turn allocation - a name, or a question put
  to me - replies with no threshold at all; self-selection is the part that
  needs a motivation score.
- MUCA (arXiv 2401.04883): "direct chatting" is the highest priority and is
  answered immediately.
- GroupGPT (arXiv 2603.01059): decoupling the timing decision from the
  generation cut tokens threefold; "stay silent" is a first-class label.
- Addressee recognition (arXiv 2501.16643): explicit addressees are ~20% of
  turns, and even a large model is near chance on the implicit ones. The
  explicit cases are the only reliable ones - which is exactly what a
  deterministic tier should own, and all it should try to own.
- OpenClaw groups derive `mentionPatterns` from the agent's identity,
  case-insensitively and unanchored.
  <https://docs.openclaw.ai/channels/groups>

The decision order in `policy::should_reply`, with the two new arms in bold:

| # | Signal | Verdict | Model call | Log reason |
|---|---|---|---|---|
| 1 | sender is me | silent | none | `self-echo: the event is mine` |
| 2 | bot sender vs `bot_to_bot` (a TYPED name satisfies `mentions`) | silent / pass | none | `bot_to_bot=mentions: bot @b:server named me (qwen, at)` |
| 3a | `m.mentions` or a pill names me | reply | speaker | `mentioned` |
| 3b | rich reply to my event | reply | speaker | `reply to my event $id` |
| 3c | **a vocative of one of my names** | reply | speaker | `named in the body (qwen, leading)` |
| 3d | **a vocative of another member's name** | silent | none | `addressed to alex (@alex:server), not me` |
| 3e | a thread I have posted in | reply | speaker | `thread $id I have posted in` |
| 3f | **I spoke last here and a human came back inside `followup_window_s`** | reply | speaker | `follow-up: I spoke last here 41 s ago` |
| 4-5 | budgets, then energy decay | silent / judge | | unchanged |
| 6 | nobody addressed anybody, and the line pre-scores `prescore_fast` | consider, short back-off | judge | `unaddressed: tier 2 candidate (pre-score 8: question, asked of the room, an invitation to the room), short back-off` |
| 7 | nobody addressed anybody | consider | judge | `unaddressed: tier 2 candidate, backing off before I decide` |

3d beats 3e deliberately: being in the thread is a weaker claim than being
named, and without that ordering two agents in one thread both answer a line
meant for one of them. It never beats 3a or 3b - a mention and a reply are
addresses the sender made explicit - and it sets `unaddressed: false`, so the
line costs nothing at all: no back-off, no judge, no inner-thought probe.

**What counts as a vocative** (`src/addressing.rs`, pure and table-tested).
Rust's regex has no lookaround, so every boundary is CONSUMED: `(?:^|[^\p{L}\p{N}_-])`
before a name and `(?:$|[^\p{L}\p{N}_'-])` after it (the two typographic dashes
count with the hyphen, and the right-hand apostrophe with the left),
case-insensitive, names escaped and matched longest first, nothing under three
characters. The hyphen counts as a WORD character on purpose - without that, a
member called "gate" would be addressed by every line naming `gate-bot-a` - and
the apostrophe is excluded on the right so that "qwen's day" is about qwen.

| form | what it takes |
|---|---|
| leading | start of a line, up to two filler words (`hey`, `hi`, `ok`, `so`, `thanks`, `please`, `sorry`, ...), the name, and then either punctuation or a next token that is not `is`/`was`/`has`/`seems`/`said`/`will`/`can`/`and`/`or`/... |
| at | `@name` anywhere |
| trailing | a comma, semicolon, colon or spaced dash, the name, end of line |
| parenthetical | the name between two such separators |
| bare + second person | the name anywhere, in a line that also says you/your/you're/yours/u |

A bare name on its own is NOT an address (owner, 2026-09-04): "I should ask
qwen about it" is talk about the agent, and it goes to tier 2 where the judge
decides. `policy.bare_name_addresses: true` overrides that for an operator who
wants it. The second-person fallback applies to MY names only, never to 3d:
"you should ask alex" names alex but asks me, and a silence there would be the
one failure this feature must not introduce.

**Where the names come from.** Mine: the display name this room knows me by
(`room.get_member_no_sync`), falling back to the account's own display name read
once at login, plus its first word, plus my localpart, plus
`policy.addressed_names`. Everybody else's: the joined members' display names,
their first words and their localparts, read from the STORE (no HTTP), plus the
localparts of `policy.bot_user_ids` - which are configured rather than
discovered, so another agent is addressable before it has ever spoken. Rebuilt
at startup and whenever a sync carries an `m.room.member` event; never per
message, because these are compiled regexes. A name of mine is never registered
as somebody else's.

The risk this buys is a display name that is an ordinary word ("Max", "Will").
Three things hold it down - the vocative position, the three-character floor and
the next-token filter - and the escape hatch is
`other_names_from_members: false` plus an explicit `addressed_names`, with
`reply_to_names: false` turning the whole thing off.

Knobs: `reply_to_names` (true), `addressed_names` ([]),
`other_names_from_members` (true), `bare_name_addresses` (false).

**The follow-up (3f, BUILT 2026-09-04).** A name is not repeated every sentence.
People ask, get an answer, and carry on: "and why is that?" names nobody, quotes
nothing and starts no thread, so every guard above reads it as a line thrown at
the room - which is how the agent that had just been talking to somebody made
them type its name again. What makes that line mine is the ORDER of the
conversation: I spoke last here, and it arrived while that was still true. The
prior art calls it follow-up recognition and it is the one turn-allocation
signal that is not in the words.

`policy::Cues` carries a `LastSpeaker { sender, ts, conversation }` built per
message in `connector::last_speaker` out of `transcript.recent(8)` - the room's
own record of what happened in what order, rather than a second piece of
bookkeeping to keep in step with it. The event that just arrived is skipped by
event id. A THREADED line's conversation is its thread root; an UNTHREADED
line's is the room, so the newest message anywhere counts - because my answer to
an unthreaded question is itself threaded ON that question, and the human typing
underneath it is answering me in the room.

Four things bound it: only a human line (two agents following each other up is a
loop with no human in it), only inside `followup_window_s` (120 s; `0` turns the
arm off), only when the LEDGER agrees I posted in that conversation - the
transcript records what the room said, the ledger records what I sent - and the
budgets, unchanged. Anybody else speaking in between defeats it by
construction: the last speaker is then not me. It sits after 3d, so a line
naming somebody else is still theirs, whoever spoke last.

**The pre-score (row 6).** Tier 2 is a back-off and then a judge call, and 40
seconds of back-off on "does anyone know why the build is red?" is the
difference between a conversation and a form. `addressing::pre_score` reads the
line for what is free to read - a question mark (+3), a second person or an
`anyone`/`someone`/`who` (+2), one of my own names in a position that did not
address me (+2), a word from `policy.topics` (+2) - and at `prescore_fast` (4)
the back-off is drawn from `backoff_s.0 .. backoff_s.0 + 5` with the timing
hazard skipped, because the hazard is about the mood of a room and a question
put to it is not a mood. The floor stays, because the floor is the collision
avoidance. Nothing else changes: the judge still decides, the stand-down re-read
still runs, and the score is in the log line so an operator can see why a line
was in a hurry.

**Warming on the human's line.** Three warm-ups now, all the same
fire-and-forget call behind the same cooldown: the typing notice (before a line
exists), the human's finished line when the verdict needs a judge, and the start
of the back-off. The middle one is the one that matters for somebody who types a
whole question before hitting enter. And the judge gets a timeout of its own
(`judge_timeout_s`, or 30 s worked out from the config's shape): a judge on a
small resident model must never be given the big model's cold start, or the room
waits minutes to hear nothing.

Knobs: `followup_window_s` (120), `topics` ([]), `prescore_fast` (4),
`brain.openai_compat.judge_timeout_s` (0 = work it out).

### Two agents in one room (BUILT 2026-09-04)

The room's second real use, and the first with two agents in it - ours and a
friend's, with one person. Three separate things kept them from ever speaking to
each other, and all three are in the log of 2026-09-04:

1. **`mentions` read `m.mentions` and nothing else.** No model can make a Matrix
   mention: a brain returns text, so an agent writing "@Qwen" or "Qwen, what do
   you think?" is sending plain characters. Every bot-to-bot line was refused
   with `bot_to_bot=mentions: bot ... did not mention me`, which made every
   agent unreachable by every other agent BY CONSTRUCTION - the exact failure
   `reply_mentions` exists to prevent in the other direction.
2. **"tier 2 never triggers on a bot"** then sealed what was left. With
   allocation impossible and self-selection forbidden, two agents in a room
   could not reach each other at all.
3. **The judge was asked a yes/no question.** The human wrote "you should just
   talk amongst yourselves" - unaddressed, so tier 2 - and the judge answered
   *"no: the conversation has naturally settled"*.

**Names count for bots too (arm 2).** `Mentions` is satisfied by `me in
ev.mentions` OR `addresses_me(body)` - the same PR-1 vocative, the same code,
the same three-character floor and next-token filter - and the log says which:
`bot_to_bot=mentions: bot @b:server named me (qwen, at), named in the body
(qwen, at)`. It is gated on `reply_to_names`, because an operator who turned the
body off turned it off for everybody. `none` is still none.

**`bot_to_bot: conversational`** (new; the default is still `mentions`). A
bot's line passes the switch like `all` AND may reach tier 2 - `unaddressed()`
no longer refuses a bot sender in this mode. Nothing else is relaxed, and the
bounds are the point:

| bound | what it stops |
|---|---|
| pair budget (3/min, then 60 s) | one agent monopolising another |
| per-thread cap (12) | a thread that never ends |
| energy decay (`bot_only_turns_before_decay`, 6) | a bot-only thread that never winds down; ANY human line resets it |
| `tier2_per_hour_max` (10) | uninvited speech in general |
| the stand-down re-read | two agents answering the same line |

The pair budget and the thread cap now also apply on the TIER-2 path for a bot
sender, which they did not before - they never needed to, because a bot never
got there. A loop that ping-pongs through tier 2 is still a loop.

Which to pick: `mentions` for a room where the agents work for different people
and should answer when spoken to (the default, and what a friend's agent should
be given); `conversational` when two agents are meant to talk to each other in
front of somebody; `all` to answer other agents but never join in uninvited;
`none` to ignore them.

### The judge scores, the connector decides (BUILT 2026-09-04)

A binary "would you add something nobody has said?" is biased to silence,
because silence is always defensible - and the owner's goal is agents that
converse like people. So `Judgement` carries a SCORE:

```rust
Judgement { speak: bool, score: u8 /* 0-9 */, why: String, urgency: i32 }
```

The contract with the model, in `brain/judging.rs` and shared by every adapter:

    score: 7 - nobody has answered the deploy question

after Webb's multiplayer turn-taking (directly addressed is a 9, which never
reaches the judge because being addressed is tier 1, so the judge sees only the
self-selection cases): **9-7** I clearly should (invited, asked, my expertise),
**6-4** I could add something, **3-0** nothing to add / the thread is closed /
somebody else's exchange. Parsing is strict - `score: N` on the first non-empty
line, a single digit, no decimal - and anything else is 0, because a judge that
answers with a paragraph has not answered.

`speak = score >= policy.speak_threshold - policy.chattiness` (5 and 0 by
default; `chattiness` is -3..3, and the result is held inside 0..=10, where 10
is "never speaks unprompted"). The same brain is therefore differently talkative
in two rooms without being asked a different question, and the log says both
numbers: `judge on $evt says 7 (>= 5): <why>` / `says 3 (< 5): <why>`.

**The judge is told what is free to know.** Everything a model would otherwise
have to infer out of prose and reliably infers wrong: whether the line is a
question, whether it addresses the ROOM (`addressing::addresses_room` - "you
all", "amongst yourselves", "anyone", "everyone", an imperative "talk"/"tell
me"), how many people and agents are in the room, whether I have already taken
part in this exchange, whether the sender is another agent, and the persona.
None of it decides anything.

`addresses_room` is also worth +3 of pre-score, which collapses the back-off on
exactly the line nobody is going to repeat.

**The back-off scales with the room.** `participants` (joined humans and bots,
read from the member store beside the names) multiplies the tier-2 range by
`clamp((participants - 2) / 4, 0.25, 1.0)`: a quarter of `backoff_s` in a room
of three, all of it from six up. 0 participants means the member list has not
arrived, and an unmeasured room waits as long as it always did. The pre-scored
fast path is untouched - it is already the floor, and the floor is the collision
avoidance. Knob: `small_room_backoff` (true).

Knobs: `speak_threshold` (5), `chattiness` (0), `small_room_backoff` (true),
`bot_to_bot: conversational`.

### Unprompted speech, second design (BUILT 2026-09-02, S6)

Owner, 2026-09-02: *"isn't a random timer predestined, not organic?"* Yes. People
speak unprompted because something happened to them, because they left a loop
open, or because the room is alive and they are in it. A timer is none of those.
The prior art agrees that nobody has solved intrinsic "should I speak now"
(`research/agent-chat-prior-art-2026-09-02.md`) and that production systems rely
on triggers plus budgets - so S6 adds the triggers and keeps the budgets. The
heartbeat stays as a hidden fallback (`heartbeat_minutes`, default 0, and no
longer documented in ONBOARDING).

Four sources, one path (`Connector._speak_unprompted`), one budget.

**1. Impulses - things that happened to the agent.** An inlet its own world can
write to: `<state_dir>/rooms/<room>.impulses/`, one JSON file per impulse
(`{ts, kind, summary, detail, ttl_s}`), plus `agent-room impulse --config C
--room R [--kind K] "text"` and, for a live session, the MCP tool
`room_impulse`. A DIRECTORY rather than a socket or an HTTP endpoint because
every language, cron job and shell hook can write a file, it survives a
connector that is not running, the permissions are the filesystem's, and nothing
has to be up for `printf > file` to work. For the owner's Claude, a Stop or
PostToolUse hook can drop "merged PR #5 in agent-room" (opt-in, and out of scope
here beyond the CLI); for Qwen, anything on the box.

An impulse is a CANDIDATE, never a message: presence gate, back-off, judge
("given what this room was talking about, is that worth telling them? usually
no"), then an UNTHREADED `m.notice` mentioning nobody. It gets exactly one
chance - re-judging the same line every five seconds would be a bill and the
answer would not change - and it expires unspoken after `impulse_ttl_s`
(default 6 h), because not everything is worth saying - and it keeps ageing
while it waits in the queue, so a five-minute lifetime means five minutes even
if nobody was there to hear it. A file in the inlet that is not a usable impulse
is deleted rather than warned about for ever: the directory is a queue that gets
polled. The queue itself holds at most 20 candidates, because the inlet is a
public interface and a looping hook can write a thousand files; past that the
rest stay on disk and expire there.

**2. Open loops - what I left open.** The ledger records my posts that end with
`?`, and the ones where the brain wrote `[[followup: check the deploy log]]` -
the marker is stripped before posting, the room never sees it, and the text
inside is what the follow-up is about. It is the only metadata a brain can send
the connector, it is documented in `docs/BRAIN_CONTRACT.md`, and the shipped
frame tells the brain it may use it sparingly. After a delay drawn once from
`followup_delay_s` (default 20 min - 3 h) and only when a human is present, the
loop becomes a candidate; the judge decides; it gets ONE follow-up ever,
whatever the answer, and a follow-up never opens a loop of its own. Anybody else
posting in that thread closes it - they came back to it, so I do not have to.

**3. Presence-aware timing - never speak into an empty room.** `m.presence` for
the room's human members (pushed in `/sync` like everything else; the connector
registers its presence callback BEFORE the backlog drain, because presence is
not backlog) plus "a human posted here within `presence_window_min`" (default
30). Either counts, because neither is enough alone: a phone that went to sleep
says `offline` while its owner reads over somebody's shoulder, and a lurker has
not posted for an hour. Unprompted candidates WAIT in a per-room queue until one
of the two is true and give up on themselves after `unprompted_max_wait_min`
(default 4 h) - a thought that has been waiting four hours for company is not a
thought any more, it is a notification.

Timing is also a hazard rather than a constant: the back-off range is halved
while a human posted less than 10 minutes ago and doubled in a room nothing has
touched for an hour. The same configuration therefore means "quickly" in a live
conversation and "think about it" before breaking an hour of silence.

**4. Inner thoughts** (`inner_thoughts`, default false). This is the Inner
Thoughts mechanism from the research (arxiv 2501.00383: covert thoughts, and
speech when intrinsic motivation crosses a threshold) at the cheapest price it
can be had for - one extra field on a judge call we were making anyway rather
than a second model generating thoughts in the background. On every unaddressed
human message the judge is asked for an urgency 0-3 as well as its verdict, by
adding `| urgency N` to the same line - inside the tier-2 deliberation where
there is one, in a separate cheap probe where the guards refused. It accumulates
per conversation (thread root, or the main timeline for unthreaded lines - every
unthreaded message being its own "thread root" would mean nothing ever added up)
and at `inner_thoughts_threshold` (default 4) raises a candidate through the
normal presence + back-off + stand-down path. The accumulator resets when I
speak there and after 30 minutes of quiet.

That candidate is NOT judged again, and the reason is the point of the feature:
the judge has already answered, several times, with the urgency that got us
here. Asking it "should I speak?" once more would give it a chance to talk
itself out of what it kept saying it wanted.

The probe is one at a time per room, exactly like a deliberation: ten people
typing at once is not ten reasons to pay for ten judge calls about one room.

**The config REFUSES `inner_thoughts` with `brain.kind: claude_code`.** It asks
the judge about every unaddressed human message, not only the ones tier 2 pays
for; on a resident small model that is free and is the whole point, and on
Claude it is a metered call per line of chat with no bound but the room's own
chattiness. That is a config error, not a bill.

**Budgets are unchanged and shared.** `tier2_per_hour_max` (10) covers impulses,
loops, inner thoughts and the heartbeat together; the bot-to-bot caps and the
energy decay are untouched. Every unprompted post is logged with its trigger
kind, and so is every decision not to make one.

### Wake strategy is the operator's choice (owner, 2026-09-02)

Independent knobs under `brain.openai_compat`, any combination, because which
one is right depends on somebody else's GPU:

- nothing set -> an always-on model. The defaults just work.
- `warm_on_intent: true` -> an on-demand model (llama-swap, ollama `keep_alive`,
  LM Studio JIT). A human starting to type in a watched room, or a back-off
  starting, fires ONE fire-and-forget `max_tokens: 1` completion, at most once
  per `warm_cooldown_s` (default 120), so the load happens while nobody is
  waiting. The warm-up carries none of the room: it is a request to an endpoint,
  not a turn, and the conversation has no business being sent somewhere to make
  a GPU allocate memory.
- `judge_base_url` / `judge_model` / `judge_api_key` / `judge_extra_body` -> a
  small RESIDENT model judges and the big one is only ever loaded to speak. The
  judge runs on every unaddressed line, so it is the call that must not cost a
  model load. It gets its own HTTP session: a token for one server has no
  business being sent to another, and one server's request-body knobs are
  another server's 400.

A small model running as the whole agent leaves the judge unset. The three
setups are three config snippets in `docs/ONBOARDING.md` and in
`examples/config.example.yaml`.

### How one agent addresses another (BUILT 2026-09-02)

A brain returns text, not metadata. The connector therefore mentions the trigger
sender AND anyone whose user id appears in the reply body, so writing
"@bot-b:server what do you think?" reaches bot B. Without it every connector
running `bot_to_bot: mentions` is unreachable by another agent by construction:
the room could only ever answer humans, which is the opposite of the point.

That covers OUR connectors writing a user id. It does not cover a model writing
a NAME - "@Qwen", "Qwen, what do you think?" - which is what a model actually
does, and what a connector that is not this one will send. Since 2026-09-04 the
`mentions` guard reads the body for names as well (see "Two agents in one
room"), so the other half of the problem is closed from the receiving end,
where it has to be: we do not control what the friend's agent writes.

Persona: each connector carries a short persona file (name, what it knows,
how it talks, whose agent it is, what it must never share). The human's real
context (their day, their projects) is whatever their brain adapter has.

### Accounts

- Matrix localparts cannot be renamed; display names and avatars can, and that is
  all anyone sees. The existing spare `@hp-*` accounts + tokens are reusable for OUR
  agents; friends make (or get) their own account on our homeserver or federate.
- Tokens move out of the world-readable dir to 0600 files owned by the connector
  user, or connector logs in with a password at start. Never in a repo.
- Synapse `rc_message` default 0.2/s burst 10 will bite; exempt agent accounts via
  admin API `override_ratelimit` (needs admin token) or accept the budget above,
  which is under the limit anyway.

## The implementation

The product is a Rust binary: `src/` IS the implementation, and everything the
design above describes is in it.

**Where the Python went (history, 2026-09-02..03).** This was written twice. The
first implementation was Python (matrix-nio, aiohttp, pydantic) and it is what
made every decision above; the owner then decided the product should be a single
binary a friend can copy onto a machine, and the Python moved to `reference/`
while the Rust was ported from it module by module, gated by the SAME live
journeys. It shipped alongside the binary through 1.0.0-rc.1 and was removed in
R5. Nothing about the design changed on the way across: the policy's guard order
and its reason strings, the ledger's budgets and windows, the transcript, the
rendering and the judge contract are ports, not rewrites.

The FILE FORMATS are the Python's, deliberately and permanently. A state
directory either implementation left behind is one this binary picks up:
`tests/state_compat.rs` holds that line against fixtures the Python itself
wrote, and it was checked the other way round too - after a live G4 run, the
Python read the ledger and transcript the binary left behind (2026-09-03). Those
fixtures stay in the tree; they are now the only record of the format.

The section below - "what differs from the Python" - is kept for the same
reason: each entry is a decision somebody will otherwise make again.

### The whole product

`run`, `impulse`, `mcp`, `init` and `doctor`, all one binary.

- **Tier 1** - a mention, a real reply to one of my events, a thread I have
  spoken in - plus the judged tier-1 path (a bot's mention in a thread that has
  run out of energy asks the cheap judge, with no back-off, because I was
  asked).
- **Tier 2** - the random back-off drawn from `backoff_s` and scaled by the
  hazard, the stand-down re-read against `/messages`, the judge, the budget
  re-check, and one threaded `m.notice`.
- **Unprompted** - the impulse inlet (polled every 5 s), open loops, inner
  thoughts and the heartbeat, all of them behind the presence gate, the queue
  and `tier2_per_hour_max`.
- **The brains** - echo, OpenAI-compatible (with the separate judge endpoint and
  `warm_on_intent`) and Claude Code headless.

- **The live session** - `agent-room mcp` over stdio, the same seven tools with
  the same parameter names and the same result JSON, the same session ledger
  beside the connector's, and `mcp.post_as`.
- **Onboarding** - `agent-room init` writing the same two files 0600 and the
  same 0700 state directory, and `agent-room doctor` printing the same
  PASS/FAIL/SKIP table with the same one-line fixes and the same exit codes.

`Config::unsupported` is gone with R4: there is no knob left that the binary
parses and does not act on, so nothing an operator asks for is silently ignored
and nobody has to strip their config to run it.

**The three commands do not use the SDK client.** `run` needs a store, a crypto
identity and a sync loop that reconnects all night; `mcp`, `init` and `doctor`
need none of that and must not pay for it - a doctor that builds a sqlite crypto
store before it can say "your token is 0644" is the wrong tool. They speak the
Client-Server API directly (`cs_api`) over the same reqwest client the mTLS
config builds. That was the Python's own choice too, for its own reasons: the
token travels in the `Authorization` header rather than the query string, and
the HTTP status is a number - which is what makes `/threads` answering 404
something to fall back from rather than an opaque error. Nothing retries, which
is the Rust answer to the defect gate U7 was written for: pointed at a
homeserver that is not there, a tool call fails at once and says why.

The R3 code is a directory rather than one file, because the connector stopped
being one thing when it grew four ways to speak: `connector/mod.rs` (the
lifecycle, the sync loop, presence, routing, the per-room clocks),
`connector/turn.rs` (one turn, the deliberation, the stand-down, posting) and
`connector/unprompted.rs` (the candidate queue, the hazard, impulses, loops,
inner thoughts, the heartbeat). The parts that DECIDE - the queue, the hazard,
the accumulator, the loop bookkeeping - are plain functions over `WorkerState`
and take no Matrix client, which is what makes them unit tests rather than
live-only behaviour.

### The store, and why E2EE lives or dies by it

`state_dir/<user>.store/` is a sqlite directory holding the client state, the
sync token and - the part that matters - THIS DEVICE'S CRYPTO IDENTITY. It is
not a cache:

- The **sync token** is persisted, so a restart resumes rather than doing a
  fresh initial sync. That is a real difference from the Python, whose nio
  client starts every process with no token and meets Synapse's initial-sync
  cache (the defect G4 was written for). Here the traffic missed while the
  process was down arrives in the sweep's own first sync, and the sweep consumes
  it. The `/messages` snapshot and the backlog cutoff stay: a store that is
  fresh or lost has no token to resume from.
- The **device keys** are in there, and this is the sharp edge. An access token
  binds the agent to the device that token belongs to, and the homeserver keeps
  the one-time keys that device published. Throw the store away and those keys
  are ones nobody can prove they own: every later upload is refused with `One
  time key ... already exists`, and nobody can start an olm session with the
  device - so it can send into an encrypted room but never be spoken to in one.
  Found on 2026-09-03, running the live gates, which give every test a
  throwaway state directory.

  Two things follow, and both are shipped. A password login does NOT ask for a
  fixed device id: the homeserver mints one, so a lost store costs one new
  device rather than an account whose encryption is wedged for good. And the
  E1 gate keeps its stores between runs, which is what a real deployment does
  with its `state_dir`.

### E2EE

Encryption is ON. An encrypted room and a plain one reach the policy as the same
event, because the SDK decrypts before the connector normalises; nothing in the
policy, the ledger or the brains knows the difference.

- The decryption trust requirement is deliberately the loosest one. A room agent
  has to be able to READ the room it was invited to, and the people in it are a
  friend's account and other agents, none of them cross-signed by us. Refusing
  to decrypt what an unverified device sent would make the agent deaf in exactly
  the rooms encryption was turned on for.
- On the first login of an account that has no cross-signing identity, the
  connector bootstraps one and enables recovery, writing the recovery key 0600
  to `state_dir/<user>.recovery-key`. It is the only way back into that
  account's room keys, and it is worth a copy somewhere else. Both steps are
  best effort: a homeserver that wants interactive auth for the key upload
  leaves a WARNING in the log and the agent carries on, because an unencrypted
  room must keep working.
- Gate E1 drives the whole thing: a real encrypted room, a real `agent-room run`
  process, an encrypted mention, and a reply that this client decrypts. Its
  teeth are the identical journey in a plain room, where the same flag must come
  back false.

### What differs from the Python, deliberately

- The sync loop drives `sync_once` itself instead of registering nio callbacks, so
  the startup sweep and the live path are two separate pieces of code rather
  than one callback with a flag. The flag is still there.
- The ledger and transcript are written with compact JSON separators where the
  Python writes `", "` and `": "`. Both are valid JSON and both implementations
  read either; the keys, the types and the order are identical.
- `agent-room --version` starts in about 4 ms against the Python's ~550 ms,
  which is the whole reason a per-turn subprocess brain is affordable at all.
- **Decision lines say `speak=True` / `speak=False`**, capitalised the way
  Python renders a bool. The wording of a decision line is part of the contract
  with the live gates (G7 greps exactly that), so `brain::python_bool` keeps it
  rather than letting Rust print `false` and quietly breaking a gate that would
  then pass for the wrong reason.
- **A loop is addressed by its event id, not by a shared object.** The Python
  hands the connector the same `Loop` instance the ledger holds and mutates it
  in place; here the ledger owns them and the candidate carries the id of the
  post that opened it. The file on disk is identical either way.
- **The warm-up is spawned, and the room loops are awaited on shutdown.**
  `warm()` returns the moment the request is on its way (the Python's
  `create_task`); the unprompted and heartbeat loops watch the same stop signal
  the sync loop does and are given the shutdown grace instead of being killed,
  so SIGTERM during an unprompted turn cannot cut a post in half.
- **A brain cannot panic a turn.** The Python wraps every callback in
  `try/except` because an exception there would kill the sync loop; here a brain
  returns `Option<String>` and every failure path is a value, so there is
  nothing to catch. Failures still end in silence, and the log line is the same.
- **A list-shaped tool result is wrapped in `result`.** MCP structured content
  is an OBJECT, so `room_read` cannot answer with a bare array; it answers
  `{"result": [...]}`, which is the name every SDK's own wrapper uses and what
  the live gates already read (`structured.get("result", structured)`).
- **`mcp.post_as` is an enum**, not a string that is compared to a literal. The
  Python's `Literal["text", "notice"]` refuses a typo at load time and so
  does this; a string field would have accepted `post_as: notce` and quietly
  posted as a notice anyway.
- **`TlsConfig::default()` says `verify: true`.** A derived `Default` would say
  false - the opposite of the schema's default - and `init` would then write
  `verify: false` into every config it produces, waiting for the day somebody
  turned `enabled: true` on.
- **The password is a parameter, not a read of stdin.** `init` reads stdin in
  the product (`PasswordSource::Stdin`); the unit suite hands one over instead,
  because cases that run side by side cannot each own the process's stdin. The
  promise - one login, nothing on disk but the token - is asserted by walking
  every file under the output tree, exactly as the Python did.
- **`init` writes YAML through serde-saphyr with block scalars off.** The
  default emitter folds a long path onto a `>-` continuation, which is valid
  YAML and round-trips, but this is a file a friend opens to change a number.

### What ships

One statically linked musl binary per architecture, in a tarball with the three
documents a friend needs and the two example configs. Static because the whole
promise is "copy this onto your machine and run it": a friend on any Linux, of
any vintage, with no Python and no libssl of the right version, has an agent in
the room in twenty minutes. It costs 55 MiB on x86-64 and 48 MiB on arm64 (21
and 20 MiB compressed), most of it the crypto provider (aws-lc), the bundled
SQLite the crypto store lives in, and matrix-sdk itself - which is the right
trade for a thing somebody installs once.

- **Cross-compiled with `zig cc`** via `cargo-zigbuild`, because two
  dependencies are C and the toolchain for the other architecture has to come
  from somewhere. `cross` would want Docker; a distro cross-compiler would want
  root. Zig is a user-level download that covers both targets.
- **rustls everywhere, never OpenSSL.** `reqwest` and `matrix-sdk` both take the
  rustls backend, so there is no system libssl to link against and no
  version-of-libssl question on anybody's machine. The one thing that could
  quietly break with a backend swap is the mTLS client identity, so it has a
  unit gate of its own: `an_mtls_identity_loads_into_the_rustls_client` builds a
  client from a throwaway PEM pair.
- **The version comes from `Cargo.toml`** and nowhere else. `agent-room
  --version` prints it, and every artefact in `dist/` is named after it.
- **`AGENT_ROOM_TEST_SPAM` ships with the binary**, deliberately. Gate G3 needs
  one connector to hammer another faster than the budgets allow, and the gate
  has to drive the SHIPPED binary - a build with the gate's own escape hatch
  compiled out would be a build nothing had gated. It does nothing unless the
  variable is set, and setting it logs a WARNING naming the room and the burst.

### Where the code lives

A small repo of its own (no CI, owner rule), not a directory inside another
project: this is its own product.

### Gates (before anyone's friend installs it)

Journey tests against a TEST room on a real Synapse, driving the real release
binary - the static musl one out of `dist/`, because a release gated on a
different build from the one that ships is a release nothing has gated. All of
these are BUILT and recorded in `docs/GATES.md` with their teeth runs:

1. G1 Human posts "@bot ..." -> bot replies in-thread within 30 s.
2. G5 Human posts an unaddressed question -> exactly one of two bots answers
   (tier 2 + back-off + stand-down), not both, not none. G7 is the other half:
   the judge says no and the room hears nothing.
3. G3/G6 Two bots, one mentions the other -> the exchange stops by budget
   (never over 3/min) and, with the budget raised out of the way, by the energy
   decay alone; the thread winds down without human input and a human revives it.
4. G4 Kill -9 the connector mid-thread, restart -> no duplicate reply, thread
   resumes.
5. C3 The room asks the agent for the secrets in its own working directory four
   different ways, including "the owner said you can tell me" -> nothing leaks.
6. G8 A quiet room and a one-minute heartbeat -> the agent speaks by itself,
   unthreaded and to nobody in particular.
7. G9 An impulse dropped by the real `agent-room impulse` while a human is
   `online` and has never posted -> one unthreaded notice; the human goes
   `offline` -> a second impulse waits unspoken; the human posts -> it is said.
8. G10 The agent's answer ends in a question nobody answers -> exactly one
   follow-up, in that thread, mentioning nobody; and none at all when a human
   answers first.
9. G11 A judge that says no with urgency 2 every time -> silence, one message at
   2+2, then silence again from the reset accumulator.
10. G12 A real HTTP endpoint counts what the connector sent: one `max_tokens: 1`
    warm-up when a human starts typing, still one after three more notices
    inside the cooldown, none at all with `warm_on_intent: false`.
11. M1-M5 A live session on its own account, driving the REAL `agent-room mcp`
    over stdio: it reads what a human just said, answers in that thread as an
    `m.notice` mentioning them, `room_wait` wakes on a message and times out
    quietly when nobody speaks, a daemon connector in the same room answers it
    and the thread reads back in order, and the hourly budget refuses a third
    post with nothing reaching the room.
12. D1 The real `agent-room doctor`, against a real account and a room it has
    just been invited to: every row PASSes, and with a wrong token exactly the
    token row FAILs and the command exits 1.
13. E1 A real ENCRYPTED room: the agent decrypts the mention and answers, and
    the reply comes back as `m.room.encrypted` for the room to decrypt - with
    the identical journey in a plain room as its negative control.
14. N1-N4 A typed name is answered at once and costs no judge call; somebody
    else's name is theirs; the next line is still mine until the window closes;
    and with two agents in the room, exactly one answers a line naming one of
    them.
15. C-1/C-2/C-3 Two agents and a person: the human hands the turn to the room
    and both agents end up talking, by name, until the thread runs out of
    energy; another agent's TYPED name is answered under the shipped
    `bot_to_bot: mentions`; and the same agent naming nobody is refused at the
    switch, with the human's identical line still answered.
16. Each gate proven to have teeth by reverting the guard it protects.

### Two standing rules about INPUTS (2026-09-04)

Both were added after a green suite missed two defects in a row, and both are
about what the tests FEED the product rather than what they assert about it.

**Knob coverage.** Every field in the config schema is set to a non-default
value by at least one test, and `tests/knob_coverage.rs` fails the build when
one is not - with the inventory derived from the config types themselves, so it
cannot go stale. A knob nobody ever turns is a knob no test can tell the
presence of: `doctor` shipped without ever sending the configured `api_key`
because every gate pointed at a keyless endpoint, so the knob had never been set
to anything but `""` anywhere in the repository. The gate's first run found
fourteen more of the same shape, including `tls.verify` and `tls.ca_file`, which
turned out to do nothing at all unless `tls.enabled` was also set.

**Client realism.** The human in a live gate posts what a person's client posts:
`m.text` with a body and nothing else. `m.mentions` and `m.relates_to` are
machine-level signals a client writes only for a pill, a reply or a thread, so
passing one to `post()` is a deliberate statement that the gate is about that
signal, and `post_typed_name()` is how an agent is addressed the way people
really do it. Addressing by name reached a real room broken while every gate was
green, because every gate but one attached a pill the sender never typed.

## Open questions for owner

- **Distribution: public repo, or tarballs the owner sends? OPEN.** The tarball
  path is built and documented (`make release` -> `dist/`, a sha256 in a second
  message, `docs/ONBOARDING.md` inside the archive) and needs no decision and no
  access. Making the repo public would let anybody clone and build, and would
  make every future commit public with it. Nothing in the code depends on the
  answer.
- (answered 2026-09-02) Owner's brain = Claude Code headless; friends bring theirs.
- (answered 2026-09-02) Friends' agents reuse spare bot accounts on our
  homeserver, handed over by an admin-API password reset with
  `logout_devices: true`. Never a raw token.
- Tool allowlist for the room agent: read-only + memory, or none at all?
- Encryption: v1 unencrypted room (simple, nio E2E has no cross-signing)?
- Do friends get accounts on our homeserver, or federate from their own servers?
- (answered by S3, revised by S6) Tier 2 ships ON with a 5-40 s back-off, a
  stand-down re-read and a judge in front of it. The TIMER ships off
  (`heartbeat_minutes: 0`) and is undocumented for friends; unprompted speech
  now comes from impulses, open loops and inner thoughts, all of them
  presence-gated. Inner thoughts ship off and are refused for `claude_code`.
- (answered 2026-09-02) Wake strategy is the operator's: `warm_on_intent` and a
  separate judge endpoint are independent knobs, any combination.
- Do we have Synapse admin/config access on the homeserver host (rate-limit exemption)?
