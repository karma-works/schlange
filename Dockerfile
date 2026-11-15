# Static musl binary on scratch: no shell, no package manager, no git (KONZEPT.md, section 7).
# Build:  docker build -t schlange:dev .
# Size:   scripts/image-size.sh

FROM rust:1.98.1-alpine3.22 AS build
# Optional build-only CA bundle for managed HTTPS proxies. Secret mounts never enter a layer.
RUN --mount=type=secret,id=proxy_ca \
    if [ -f /run/secrets/proxy_ca ]; then export SSL_CERT_FILE=/run/secrets/proxy_ca; fi; \
    apk add --no-cache musl-dev ca-certificates
WORKDIR /src
# Dependencies first, for layer caching.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates crates
COPY tools/inventory tools/inventory
# build.rs of schlange-config embeds the feature map.
COPY .agents/skills/verify-schlange/features .agents/skills/verify-schlange/features
RUN --mount=type=secret,id=proxy_ca \
    if [ -f /run/secrets/proxy_ca ]; then \
      export SSL_CERT_FILE=/run/secrets/proxy_ca \
        CARGO_HTTP_CAINFO=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca; \
    fi; \
    cargo build --locked --release -p schlange --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/schlange /schlange
RUN echo 'schlange:x:65532:65532:schlange:/data:/sbin/nologin' > /passwd \
 && echo 'schlange:x:65532:' > /group \
 && mkdir -p /data

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /passwd /etc/passwd
COPY --from=build /group /etc/group
COPY --from=build --chown=65532:65532 /data /data
COPY --from=build /schlange /usr/local/bin/schlange
USER 65532:65532
WORKDIR /data
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
# `serve` keeps its SQLite database (journal, snapshots, webhook dedup, audit) here; mount a
# volume so a restart resumes from it. Runtime validation uses Schlange's embedded contract.
# An optional upstream schema can be mounted for explicit validation; no Mergify file is baked in.
ENV SCHLANGE_DATA_DIR=/data
VOLUME ["/data"]
# --offline: the health check must not spend GitHub rate limit every 30 seconds.
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["/usr/local/bin/schlange", "doctor", "--offline"]
ENTRYPOINT ["/usr/local/bin/schlange"]
CMD ["doctor"]
