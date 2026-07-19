# syntax=docker/dockerfile:1.7

ARG RUST_IMAGE=docker.io/library/rust:1.94.0-bookworm@sha256:365468470075493dc4583f47387001854321c5a8583ea9604b297e67f01c5a4f
ARG RUNTIME_IMAGE=docker.io/library/debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df

FROM ${RUST_IMAGE} AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates
COPY tools ./tools
COPY fixtures ./fixtures
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/source/target,sharing=locked \
    cargo build --locked --release -p imagegen-bridge-cli --bin imagegen-bridge && \
    install -Dm0755 target/release/imagegen-bridge /out/imagegen-bridge

FROM ${RUNTIME_IMAGE} AS codex
ARG TARGETARCH
ARG CODEX_VERSION=0.144.0
RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    case "${TARGETARCH}" in \
      amd64) target=x86_64-unknown-linux-musl; sha256=725883fc20ab4af3072829aaa0edf6d12c216238f9f7315a6656b950fb05c8bb ;; \
      arm64) target=aarch64-unknown-linux-musl; sha256=c7c44a7950bdb555c743f5bb5f7ac3ec2ee7c311970effe92fd39e82eccc6b51 ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    curl --fail --location --proto '=https' --tlsv1.2 \
      --output /tmp/codex.tar.gz \
      "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${target}.tar.gz" && \
    echo "${sha256}  /tmp/codex.tar.gz" | sha256sum --check --strict && \
    tar --extract --gzip --file /tmp/codex.tar.gz --directory /tmp && \
    install -Dm0755 "/tmp/codex-${target}" /out/codex

FROM ${RUNTIME_IMAGE} AS runtime
ARG IMAGE_VERSION=0.1.3
LABEL org.opencontainers.image.title="Imagegen Bridge" \
      org.opencontainers.image.description="Provider-neutral Codex OAuth image-generation bridge" \
      org.opencontainers.image.source="https://github.com/Crimsab/imagegen-bridge" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates curl tini tzdata && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 10001 imagegen && \
    useradd --uid 10001 --gid 10001 --home-dir /home/imagegen --create-home \
      --shell /usr/sbin/nologin imagegen && \
    install -d -o imagegen -g imagegen /config /codex-home /data/artifacts /data/state /workspace

COPY --from=build /out/imagegen-bridge /usr/local/bin/imagegen-bridge
COPY --from=codex /out/codex /usr/local/bin/codex

ENV HOME=/home/imagegen \
    CODEX_HOME=/codex-home \
    TZ=Europe/Rome \
    RUST_BACKTRACE=0
USER 10001:10001
WORKDIR /workspace
EXPOSE 8787
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
  CMD ["curl", "--fail", "--silent", "--show-error", "http://127.0.0.1:8787/health/live"]
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["imagegen-bridge", "--config", "/config/imagegen-bridge.toml", "serve", "--quiet"]
