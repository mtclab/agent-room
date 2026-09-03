# The MULTI-ARCH image CI publishes from a release tag.
#
# It is the same image as `examples/docker/Dockerfile` - static musl binary on
# `scratch`, unprivileged uid, two volumes - with one difference: the build
# context here holds a binary per platform, staged out of the release tarballs
# as `linux/amd64/agent-room` and `linux/arm64/agent-room`, and buildx picks the
# right one per platform through TARGETPLATFORM. `examples/docker/Dockerfile`
# stays the single-arch recipe somebody builds by hand next to one binary.
# A CA bundle for the scratch image: reqwest verifies servers through the
# platform trust store, and scratch has none - without this the connector
# cannot build its HTTP client at all (found on the first container install).
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS certs
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends ca-certificates >/dev/null && rm -rf /var/lib/apt/lists/*

FROM scratch
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
# Set by buildx per platform: "linux/amd64" or "linux/arm64". Declared AFTER
# FROM, because an ARG before it belongs to the build stage's own frame.
ARG TARGETPLATFORM
COPY --chmod=0755 ${TARGETPLATFORM}/agent-room /agent-room
# uid 10001, no home, no shell. The volumes must be owned by this uid.
USER 10001:10001
VOLUME ["/config", "/state"]
ENTRYPOINT ["/agent-room"]
CMD ["run", "--config", "/config/config.yaml"]
