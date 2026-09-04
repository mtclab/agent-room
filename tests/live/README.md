# The live gates

Everything in this directory drives the SHIPPED binary against a REAL
homeserver. There are no mocks: each gate creates a throwaway private room,
starts real `agent-room` processes, and asserts what the room sees - then leaves
and forgets the room with every account it used.

The product itself is a single static binary with no runtime. **These tests are
the one thing in the repo that still needs Python**, because the human side of
each journey has to be a Matrix client and the MCP gates have to be an MCP
client. That runner environment is `tests/live/.venv` and it is entirely
separate from anything the product needs.

## Setting it up

    make live-env                      # creates tests/live/.venv
    cp tests/live/live.env.example ~/.config/agent-room/live.env && chmod 600 ~/.config/agent-room/live.env
    $EDITOR ~/.config/agent-room/live.env   # homeserver, tokens file, the five accounts
    make build                         # the binary the gates drive

`~/.config/agent-room/live.env` lives outside the repository. Which homeserver these gates run against is a
deployment detail and does not belong in the source; `live.env.example` is the
documented shape of it.

| Variable | What it is |
|---|---|
| `AGENT_ROOM_LIVE_HOMESERVER` | e.g. `https://matrix.example.com` |
| `AGENT_ROOM_LIVE_SERVER_NAME` | the part after the colon in a user id |
| `AGENT_ROOM_TOKENS_FILE` | JSON, `{"localpart": "access-token", ...}`, mode 0600 |

Anything already exported wins over the file, so one value can be overridden for
a single run.

`AGENT_ROOM_BIN` names the binary under test; without it the gates drive
`target/release/agent-room`. The teeth runner sets it to point at a mutant
build, and a RELEASE is gated by pointing it at the artefact that will actually
be sent:

    export AGENT_ROOM_BIN=$PWD/target/x86_64-unknown-linux-musl/release/agent-room

That binary and `target/release/agent-room` are different files - different C
library, different hash - and gating one while shipping the other proves
nothing. `make live-e2ee` reads the same variable.

## The accounts

Six accounts, and they must not be accounts anything else is using - a gate
sends bursts, and a homeserver rate-limits per user.

| Role (the variable naming it) | Used by |
|---|---|
| `AGENT_ROOM_LIVE_HUMAN` | plays the HUMAN in every journey |
| `AGENT_ROOM_LIVE_S3_BOT_A`, `..._S3_BOT_B` | the two agents under test (G1-G12, N1/N2/N4, C1-C3, D1) |
| `AGENT_ROOM_LIVE_S3_BOT_A` | also the live MCP session (M1-M5) |
| `AGENT_ROOM_LIVE_BOT_B` | the daemon connector beside that session (M4) |
| `AGENT_ROOM_LIVE_E1_BOT` | the encrypted-room gate E1, whose store is kept between runs |

Which real accounts those are is a deployment detail: it lives in
`~/.config/agent-room/live.env`, never here.

E1 is a Rust test rather than a pytest one, because the Python client here has
no crypto store and could not tell an encrypted reply from a plain one.

## What the human posts

The human here posts what a person's client posts: `m.text` with a body and
nothing else. `m.mentions` is written only when the sender picks a name out of
the completion list, and `m.relates_to` only when they use the reply or thread
affordance - so those are opt-in (`post(..., mentions=[...])`,
`thread_root=...`), and passing one says the gate is about that signal.
`post_typed_name()` addresses an agent the way people actually do, by typing its
name. The rule and the reason are at the top of `tests/conftest.py`.

## Running them

    make live          # G1-G12, M1-M5, D1
    make live-mcp      # M1-M5 and D1 on their own
    make live-claude   # C1-C3: real `claude -p` turns, ~$0.10 of the owner's quota
    make live-e2ee     # E1, the encrypted room

Every gate is expected to have TEETH - to fail when the guard it protects is
removed. `teeth.py` is what proves that, one mutation at a time:

    AGENT_ROOM_LIVE=1 tests/live/.venv/bin/python tests/live/teeth.py [G1 ...]

It edits `src/`, rebuilds, runs one gate, and restores the file with `git
checkout`; it refuses to start on a dirty `src/`.

## After the bots are swapped

`tests/live/.venv` is disposable: delete it and run `make live-env` again. The
repository's own root `.venv`, if one is still on your machine, is a leftover
from the Python reference implementation that R5 removed, and can go.
