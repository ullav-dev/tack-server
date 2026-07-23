# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1.91-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Path dependency — checked out as a sibling in CI and copied here.
COPY ullav-mcp-auth /ullav-mcp-auth

# Cache dependencies before copying source
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs
RUN cargo build --release
RUN rm -f target/release/deps/tack_server*

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

USER tack
EXPOSE 8087

CMD ["./tack-server"]
