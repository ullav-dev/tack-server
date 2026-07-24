# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1.91-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        curl \
        g++ \
    && rm -rf /var/lib/apt/lists/*
# g++ (libstdc++) is required at link time by fastembed's dependencies
# (onnxruntime via `ort`, `onig` for tokenizer regex) -- both link against
# the C++ standard library, which `rust:*-slim` doesn't ship by default.

# Path dependency — checked out as a sibling in CI and copied here.
COPY ullav-mcp-auth /ullav-mcp-auth

# Cache dependencies before copying source
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main(){}' > src/main.rs \
    && echo 'fn main(){}' > src/bin/tack-indexer.rs \
    && echo '' > src/lib.rs
RUN cargo build --release
# Must also clear the `lib*` artifacts (the crate is both a lib and two bins)
# and the .fingerprint dirs -- rm'ing only the deps/tack_server* binary
# artifact leaves the stale, empty dummy libtack_server.rlib/.rmeta in place,
# and Cargo's fingerprint cache then happily links the real binaries against
# that stale empty lib instead of recompiling it (caught by actually building
# this image, not just `cargo build` locally against the un-copied source).
RUN rm -rf target/release/deps/*tack_server* target/release/deps/*tack_indexer* \
           target/release/.fingerprint/tack-server-*

# Build real binary
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1001 tack
WORKDIR /app

COPY --from=builder /app/target/release/tack-server ./tack-server
# The embedding model is downloaded into EMBEDDING_MODEL_CACHE_DIR
# (default ./.embedding-models, i.e. /app/.embedding-models) on first run --
# /app is root-owned by default, so the unprivileged `tack` user can't write
# there without this. Mount a persistent volume over this same path in any
# deployed environment so the model survives container restarts/redeploys.
RUN mkdir -p .embedding-models && chown -R tack:tack /app

USER tack
EXPOSE 8087

CMD ["./tack-server"]
