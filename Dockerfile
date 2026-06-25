FROM rust:1.92.0-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2 AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY routes.default.toml ./routes.default.toml

RUN cargo build --locked --release -p kheish-daemon

FROM debian:12.11-slim@sha256:b1a741487078b369e78119849663d7f1a5341ef2768798f7b7406c4240f86aef AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        procps \
        ripgrep \
        tini \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 10001 kheish \
    && useradd \
        --system \
        --uid 10001 \
        --gid kheish \
        --home-dir /var/lib/kheish/home \
        --shell /usr/sbin/nologin \
        kheish \
    && mkdir -p /var/lib/kheish/state /var/lib/kheish/home /workspace /etc/kheish /run/kheish \
    && chown -R kheish:kheish /var/lib/kheish /workspace /etc/kheish /run/kheish

COPY --from=builder /build/target/release/kheish-daemon /usr/local/bin/kheish-daemon
COPY routes.default.toml /usr/share/kheish/routes.default.toml
COPY docker/entrypoint.sh /usr/local/bin/kheish-entrypoint
COPY docker/healthcheck.sh /usr/local/bin/kheish-healthcheck

RUN chmod 0755 /usr/local/bin/kheish-daemon /usr/local/bin/kheish-entrypoint /usr/local/bin/kheish-healthcheck

ENV HOME=/var/lib/kheish/home \
    KHEISH_BIND=0.0.0.0:4000 \
    KHEISH_STATE_ROOT=/var/lib/kheish/state \
    KHEISH_WORKSPACE_ROOT=/workspace \
    KHEISH_HTTP_AUTH_MODE=bearer \
    KHEISH_MCP_DISCOVERY=disabled \
    RUST_LOG=info

USER kheish:kheish

EXPOSE 4000

VOLUME ["/var/lib/kheish/state", "/workspace"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/kheish-healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/kheish-entrypoint"]
CMD ["serve"]
