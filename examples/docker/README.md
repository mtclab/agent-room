# Running an agent in a container

For a machine that runs its model server in containers already (a GPU box
with compose-managed services), the connector fits the same shape: one static
binary, one service, two volumes.

    mkdir -p agent-room && cd agent-room
    # from the release tarball for this architecture:
    tar xzf agent-room-<version>-<arch>.tar.gz
    cp agent-room-<version>-<arch>/agent-room .
    cp agent-room-<version>-<arch>/Dockerfile agent-room-<version>-<arch>/compose.yaml .   # if shipped, else from examples/docker
    mkdir -p config state
    # config.yaml, persona.md and the token file go in ./config. Inside the
    # container they are /config/...; the state dir is /state. So in config.yaml:
    #   access_token_file: /config/token
    #   persona_file: /config/persona.md
    #   state_dir: /state
    chown -R 10001:10001 config state && chmod 600 config/*
    docker compose up -d --build
    docker compose logs -f

## Or pull the image instead of building one

Every release also publishes a multi-arch image, so a host with no tarball on
it can skip the build entirely:

    docker pull ghcr.io/mtclab/agent-room:<version>

In `compose.yaml`, comment out `build: .` and `image: agent-room:local` and
uncomment the `ghcr.io` line, then `docker compose up -d` with no `--build`.
Everything else on this page is the same: the same two volumes, the same uid,
the same config layout.

It is the same binary as the tarball for that architecture - the image is built
FROM the released tarballs, never from a second compilation - and it carries a
signed provenance attestation. `docs/ONBOARDING.md`, "Install", has the
`gh attestation verify` command for both the image and the tarball. **Pin the
version**: `:latest` follows the newest stable release and will move under a
running host.

Moving an agent that already ran elsewhere: copy its state directory INTO
`./state` before the first start and stop the old instance first. The state
directory holds the device's encryption identity; a fresh one on an old token
makes the connector stop with exit code 3 (see ONBOARDING, "Back it up").

## Exit code 3 restarts for ever under compose

Exit 3 means the device is wedged, and no number of restarts cures it: the
homeserver keeps the encryption keys the token's device published, and this
state directory is not the one that published them. The systemd unit says so
with `RestartPreventExitStatus=3`; **compose has no per-exit-code policy**, so
`restart: unless-stopped` will bring a wedged connector back every few seconds
for ever, logging the same line each time.

Two ways to live with that, and neither is a workaround for the fix:

- `restart: on-failure:5` instead of `unless-stopped`. A crash is still
  retried, and a container that keeps exiting stops after five tries and stays
  stopped, where `docker compose ps` shows it.
- Keep `unless-stopped` and watch for the line: `docker compose logs --tail=20`
  says `device ... is wedged` on every start.

The fix either way is in ONBOARDING, "Back it up": restore that account's state
directory, or get a new token whose device is new too (a password login mints
one). Setting `allow_wedged_device: true` only makes sense for a room that is
not encrypted - such a device can send into an encrypted room and can never be
spoken to in one.

With `network_mode: host` an on-demand model server on the same machine is
reachable as `http://127.0.0.1:<port>/v1` from the container.

Images before 1.0.0-rc.2 carried no CA bundle and failed at startup with
"cannot build an HTTP client"; mounting the host's `/etc/ssl/certs` read-only
into the container was the workaround. From rc.2 the image ships its own.
