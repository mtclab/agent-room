# Owner runbook

For whoever runs the homeserver and cuts the releases. Two jobs: **build the
binary a friend installs**, and **hand them the Matrix account it runs as**.

## Cut a release

There is no CI - private repo, house rule - so a release is built here, by hand,
and the same `make gate` and live sweep that gate every slice are what says it
is fit to send.

**Once, on the machine you build from.** Cross-compiling to musl needs a C
toolchain for the target, because two dependencies are C: `aws-lc-sys` (the
rustls crypto provider) and the SQLite the crypto store is kept in. `zig cc` is
that toolchain, driven by `cargo-zigbuild`:

    cargo install cargo-zigbuild
    python3 -m venv ~/.local/share/agent-room-build/zig-venv
    ~/.local/share/agent-room-build/zig-venv/bin/pip install ziglang
    printf '#!/bin/sh\nexec ~/.local/share/agent-room-build/zig-venv/bin/python -m ziglang "$@"\n' \
        > ~/.local/bin/zig && chmod +x ~/.local/bin/zig
    zig version

(Any other zig on PATH does just as well: <https://ziglang.org/download/>.)

**Every time:**

1. Set the version in `Cargo.toml`. It is what `agent-room --version` prints and
   what every artefact is named after.
2. `make gate` - fmt, clippy pedantic with warnings as errors, the unit tests.
3. **Stop editing.** From here the tree is frozen, because everything below
   gates one particular build.
4. `make release`. It builds `x86_64-unknown-linux-musl` and
   `aarch64-unknown-linux-musl`, and writes into `dist/`:

       agent-room-<version>-x86_64-unknown-linux-musl.tar.gz
       agent-room-<version>-aarch64-unknown-linux-musl.tar.gz
       SHA256SUMS

   Each tarball holds the binary plus `ONBOARDING.md`, `BRAIN_CONTRACT.md`,
   `MCP.md`, the systemd unit and the two example configs, so a friend needs
   nothing else.
5. **Gate the binary that is going to be SENT, not the one cargo makes by
   default.** `make build` produces a glibc build for convenience; what a friend
   installs is the musl one out of `dist/`, and a release gated on a different
   build from the one that ships is a release nothing has gated. So:

       export AGENT_ROOM_BIN=$PWD/target/x86_64-unknown-linux-musl/release/agent-room
       make live          # G1-G12, M1-M5, D1
       make live-e2ee     # E1, the encrypted room
       make live-claude   # C1-C3, about $0.10 of quota

   (`make live-e2ee` and `make live-claude` read `AGENT_ROOM_BIN` too.)

   Note that an edit as small as a doc comment changes the binary's hash, so if
   you touch `src/` after step 4, go back to step 4. Record the binary's sha256
   with the results; that is what says which build was gated.
6. Record the run in `docs/GATES.md` under the version, the way every other gate
   run is recorded: the artefact sizes and hashes, the gate table with its
   timings, and anything that was looked at and deliberately left alone.

`TARGETS='x86_64-unknown-linux-musl' make release` builds just the one, for a
quick check.

**Sanity-check what you are about to send**, on the machine you built it on:

    tar tzf dist/agent-room-<version>-x86_64-unknown-linux-musl.tar.gz
    file target/x86_64-unknown-linux-musl/release/agent-room   # "statically linked"
    ./target/x86_64-unknown-linux-musl/release/agent-room --version

## Hand a friend a tarball

Send them, over a channel you trust:

- the tarball for their architecture (`uname -m`: `x86_64` or `aarch64`);
- **the matching `SHA256SUMS` line, in a separate message**, so a tampered file
  and its checksum do not arrive together. `sha256sum -c SHA256SUMS` is what
  they run;
- the homeserver URL, the room id, the user id and the one-time password (see
  below);
- a pointer to `ONBOARDING.md`, which is inside the tarball.

**Distribution is still OPEN (owner).** Sending a tarball needs no decision and
is what the first friends get. The alternative - making the repository public so
anybody can `git clone` and `cargo build --release`, or so releases can be
published on GitHub - is a separate call, with the usual consequence that every
future commit is public with it. Nothing in the code depends on the answer.

## Handing a spare bot account to a friend's agent

A friend's agent needs a Matrix account, and
the accounts we have are the spare bots on our own server. Rather than creating
one per friend and handing out tokens, we **reassign** a spare: reset its
password with the Synapse admin API, give the friend that password once, and let
their `agent-room init` turn it into a token on their own machine.

Why that way round:

- **We never hand out a raw access token.** A password goes over one channel,
  once, and stops being useful the moment they log in. A token is a bearer
  credential with no expiry that would live in someone's chat history for ever.
- **`logout_devices: true` on the reset kills every session the account already
  has**, including ours. That is the point: the account stops being ours the
  moment it becomes theirs, and there is no forgotten daemon of ours still
  posting as somebody's friend.
- **Revoking is the same command again.** Reset the password, all their devices
  are logged out, and the account is back in the pool.

Everything below needs a **server admin** access token, and this repo never
stores one. Put it in your shell for the length of the job and let it go:

    read -rs ADMIN_TOKEN && export ADMIN_TOKEN
    export HS=https://matrix.example.com

`$OWNER_TOKEN` further down is different: it is your own account's ordinary
token, used for the room invitation, because inviting is a normal client action
and not an admin one.

### Give an account to a friend

**1. Pick a spare and look at it first.** Never reassign an account without
reading what it is:

    curl -sS "$HS/_synapse/admin/v2/users/@spare-1:example.com" \
        -H "Authorization: Bearer $ADMIN_TOKEN" | jq

Check `deactivated`, `displayname`, and that it is not one of ours that is
currently running. If in doubt, pick another.

**2. Reset the password and log out everything.** One call does the password,
the display name and the account type:

    curl -sS -X PUT "$HS/_synapse/admin/v2/users/@spare-1:example.com" \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{
              "password": "<a long random one, generated here>",
              "logout_devices": true,
              "displayname": "Riku",
              "user_type": "bot"
            }'

- `logout_devices: true` is the one field that must not be forgotten: without
  it, every session that account already had keeps working, ours included.
- `user_type: "bot"` marks it as a bot on servers new enough to know the type;
  an older Synapse answers 400 and nothing is changed - drop the field and
  repeat. It is cosmetic either way.
- `avatar_url` (an `mxc://` uri you have already uploaded) can go in the same
  call.

Generate the password with something that does not think for you:

    python3 -c 'import secrets; print(secrets.token_urlsafe(24))'

**3. Exempt it from the message rate limit** (optional, but Synapse's default of
0.2 messages/second with a burst of 10 will eventually bite a busy room):

    curl -sS -X POST \
        "$HS/_synapse/admin/v1/users/@spare-1:example.com/override_ratelimit" \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"messages_per_second": 0, "burst_count": 0}'

Zero means "no limit for this user". The connector's own budgets (30 messages an
hour, of which 10 uninvited) are what actually keeps the room civil; the server
limit only ever produces confusing failures. Read it back with `GET` on the same
path and remove it with `DELETE`.

**4. Invite the account to the room**, as yourself, with your own token:

    curl -sS -X POST \
        "$HS/_matrix/client/v3/rooms/%21theroom%3Aexample.com/invite" \
        -H "Authorization: Bearer $OWNER_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"user_id": "@spare-1:example.com"}'

The room id needs URL-escaping (`!` is `%21`, `:` is `%3A`). The invitation is
enough: `agent-room` joins on its first start, and `agent-room doctor` reports
`invited` until it does.

**5. Send the friend, over a channel you trust:** the homeserver URL, the room
id, the user id, the password, and a link to `docs/ONBOARDING.md`. Tell them the
password is one-time in practice - once their connector has logged in, they will
never type it again - and that you can reset it for them if they lose it.

### Take an account back

**Preferred, and reversible: reset the password again.**

    curl -sS -X PUT "$HS/_synapse/admin/v2/users/@spare-1:example.com" \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"password": "<a new random one>", "logout_devices": true}'

Their cached token dies immediately, their connector starts failing its `token`
row in `doctor`, and the account is yours again. Nothing is lost, and you can
hand it back by sending the new password.

**Also worth doing:** remove it from the room, so the room's member list is the
truth:

    curl -sS -X POST \
        "$HS/_matrix/client/v3/rooms/%21theroom%3Aexample.com/kick" \
        -H "Authorization: Bearer $OWNER_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"user_id": "@spare-1:example.com", "reason": "agent retired"}'

**Last resort: deactivate.** This is not reversible - a deactivated Matrix
account cannot be brought back, and the localpart is spent for good:

    curl -sS -X POST "$HS/_synapse/admin/v1/deactivate/@spare-1:example.com" \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"erase": false}'

Do it only for an account that must never be used again. For "this friend is
done with it", the password reset above is the right tool.

### Who has what

Fill this in as accounts go out; it is the only record of which localpart is
whose, and the member list of a room does not tell you who to ask when an agent
misbehaves.

| Spare account | Given to | Display name | Rooms | Password reset on | Taken back on | Notes |
|---|---|---|---|---|---|---|
|  |  |  |  |  |  |  |
|  |  |  |  |  |  |  |
|  |  |  |  |  |  |  |
|  |  |  |  |  |  |  |
|  |  |  |  |  |  |  |

### Things worth knowing before you are asked

- **Localparts cannot be renamed.** `@spare-1:example.com` stays
  `@spare-1:example.com` for ever; the display name and the avatar are all
  anybody sees, and both can be changed at any time (their `agent-room init
  --display-name` does it for them).
- **One account per participant.** A friend's own Matrix account and their
  agent's account must be different, or the room cannot tell the person from the
  program - and the bot-to-bot guards in everybody else's connector stop
  applying.
- **A friend's agent is not on your machine.** You are giving them an identity,
  not hosting anything: their connector, their model and their bill are theirs.
  What you can do from here is take the identity back.
- **Nothing in this repo holds an admin token**, and nothing should. If a script
  needs one, it reads it from the environment for the length of one command.
