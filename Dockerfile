# ── Build stage ───────────────────────────────────────────────────────────────
# trixie (Debian 13), not bookworm (Debian 12): ort-sys's prebuilt onnxruntime
# binary (pulled in transitively via the fastembed 5.x upgrade -- see
# CLAUDE.md's "Production startup OOM" note) requires glibc >= 2.38
# (references __isoc23_strtoll/strtoull/strtol, C23-conformance symbol
# variants only glibc 2.38+ exports) and a libstdc++ ABI bookworm's GCC 12
# doesn't have (`_M_replace_cold`). bookworm ships glibc 2.36 -- linking
# failed outright with "undefined symbol" errors for both, confirmed via a
# real `docker build`, not just `cargo build` locally (this dependency only
# gets linked in the actual release build, not `cargo check`). The runtime
# stage below must use a matching trixie-based image too -- a binary linked
# against glibc 2.38+ symbols won't run on an older glibc at all, so bumping
# only this stage would trade a build failure for a runtime crash instead.
#
# 1.91 -> 1.97: the surrealdb dependency (added for backfill-clann-notes.rs)
# pulls in fastnum (declared rust-version 1.94, a hard MSRV floor) and
# diskann 0.54.0, which fails to compile under 1.91.1 specifically with
# `error[E0311]: ... may not live long enough` on several `impl
# SendFuture<...>` return-position-impl-trait methods in its graph/index.rs
# -- a real rustc-version-sensitive lifetime-inference gap, not a platform
# issue (confirmed via a real `docker build` on this exact image, not
# assumed from `cargo build --release` succeeding natively on a newer local
# toolchain, which does not reproduce it). 1.97 (only trixie tag comfortably
# past 1.94 available at the time of this fix) compiles both cleanly.
FROM rust:1.97-slim-trixie AS builder

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
FROM debian:trixie-slim

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
