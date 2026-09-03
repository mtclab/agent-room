# agent-room in a container: the static musl binary and nothing else.
#
# Build context = this directory with the binary from the release tarball
# copied in as ./agent-room (see README.md here). No package manager, no
# shell, no libc: the binary is static, so `scratch` is enough. The process
# runs as an unprivileged user; /config and /state are volumes.
FROM scratch
COPY --chmod=0755 agent-room /agent-room
# uid 10001, no home, no shell. The volumes must be owned by this uid.
USER 10001:10001
VOLUME ["/config", "/state"]
ENTRYPOINT ["/agent-room"]
CMD ["run", "--config", "/config/config.yaml"]
