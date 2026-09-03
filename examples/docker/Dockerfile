# agent-room in a container: the static musl binary and nothing else.
#
# Build context = this directory with the binary from the release tarball
# copied in as ./agent-room (see README.md here). No package manager, no
# shell, no libc: the binary is static, so `scratch` is enough. The process
# runs as an unprivileged user; /config and /state are volumes.
# A CA bundle for the scratch image: reqwest verifies servers through the
# platform trust store, and scratch has none - without this the connector
# cannot build its HTTP client at all (found on the first container install).
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS certs
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends ca-certificates >/dev/null && rm -rf /var/lib/apt/lists/*

FROM scratch
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
COPY --chmod=0755 agent-room /agent-room
# uid 10001, no home, no shell. The volumes must be owned by this uid.
USER 10001:10001
VOLUME ["/config", "/state"]
ENTRYPOINT ["/agent-room"]
CMD ["run", "--config", "/config/config.yaml"]
