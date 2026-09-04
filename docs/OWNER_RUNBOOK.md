# Owner runbook

For whoever runs the homeserver and cuts the releases. Two jobs: **build the
binary a friend installs**, and **hand them the Matrix account it runs as**.

## Cut a release

**A release is a tag.** Pushing `vX.Y.Z` runs `.github/workflows/release.yml`,
which builds both musl targets by running `scripts/release.sh` - the same script
`make release` runs here, so the path remapping, the leak check, the tarball
layout and `SHA256SUMS` are identical - then RUNS both binaries, publishes the
GitHub Release with the tarballs and their checksums, attests their provenance,
and pushes the multi-arch container image.

What that workflow cannot do is the part that matters most: the **live gates**
need this homeserver, these accounts and the tokens in
`~/.config/agent-room/live.env`, and a hosted runner has none of them. Handing
them to one would mean handing a third party a running deployment's
credentials. So the tag is not the gate - the tag is you SAYING the gate passed.
Everything below happens here, in this order, and the tag goes last.

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

You need this even though CI builds the release too: the live gates in step 5
drive the musl binary, so the musl binary has to exist HERE first. The workflow
installs its own pinned zig and `cargo-zigbuild` and does not touch yours.

**Every time:**

1. Set the version in `Cargo.toml`. It is what `agent-room --version` prints and
   what every artefact is named after.
2. `make gate` - fmt, clippy pedantic with warnings as errors, the unit tests.
   Two of those are standing rules rather than tests of a behaviour: no tracked
   file names a deployment or an account (`tests/publish_clean.rs`), and every
   knob in the config schema is set to a non-default value by some test
   (`tests/knob_coverage.rs`) - a knob nobody turns is a knob nothing is
   testing. Both fail with the `file:line` to fix.
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
7. Commit, push, and **tag**. This is the whole publish:

       git tag v1.0.0-rc.4
       git push origin main
       git push origin v1.0.0-rc.4

   The tag has a leading `v`; the version inside it must equal the one in
   `Cargo.toml`, and the first thing the workflow does is refuse the tag if it
   does not ("tag v1.2.3 but Cargo.toml says 1.2.4"). A tag with a hyphen in it
   (`v1.0.0-rc.4`) is published as a **pre-release** and does not move the
   image's `latest`.
8. Watch it, and check what came out:

       gh run watch
       gh release view v1.0.0-rc.4

   The four jobs are `build` (once per target), `smoke`, `release` and `image`.
   `smoke` is the one that matters most: it runs BOTH binaries, and it is the
   first time the arm64 build has ever been executed anywhere - a cross-compile
   that links and does not start dies here rather than on a friend's box.

**Verify the published release** the way a reader would, from a directory that
holds nothing of yours:

    gh release download v1.0.0-rc.4
    sha256sum -c SHA256SUMS
    gh attestation verify agent-room-1.0.0-rc.4-x86_64-unknown-linux-musl.tar.gz \
        --repo mtclab/agent-room
    docker run --rm ghcr.io/mtclab/agent-room:1.0.0-rc.4 --version
    gh attestation verify oci://ghcr.io/mtclab/agent-room:1.0.0-rc.4 \
        --repo mtclab/agent-room

The image is built FROM the tarballs the same run produced, so a friend pulling
it and a friend unpacking a tarball get the same bytes.

**On the FIRST release only:** a new registry package starts **private**, and a
public repository does not make it public. Until it is switched over once -
Packages, then `agent-room`, then package settings, then change visibility -
`docker pull` works for you and 401s for everybody else, so the `docker run`
above proves nothing about what a friend can reach. Check it from a logged-out
shell (`docker logout ghcr.io`) the first time.

**If the tag was wrong**, delete it and tag again - the workflow only ever runs
on a tag being pushed, so nothing happens until one is:

    git push origin :refs/tags/v1.0.0-rc.4 && git tag -d v1.0.0-rc.4

If the release was already created, delete that too (`gh release delete`); a
release GitHub still holds is one somebody can download.

`make release` has not gone anywhere and is the **offline** path: it produces
the identical artefacts on this machine with no network beyond crates.io, for
handing somebody a tarball directly (below) or for cutting a build when GitHub
is not part of the plan. `TARGETS='x86_64-unknown-linux-musl' make release`
builds just the one, for a quick check.

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

**That question is settled now.** The repository is public, so releases are
published on it and anybody can `git clone` and `cargo build --release`. Sending
a tarball by hand is still the friendlier route for a first friend - they get
the file and the checksum from you, over a channel you already trust, and never
have to know what a release page is - but it is no longer the only one, and
`tests/publish_clean.rs` is the standing gate that keeps a public tree from
naming this deployment.

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
