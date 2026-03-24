FROM rust:1-bookworm AS builder

WORKDIR /app

# Dependency caching: copy manifests and build a dummy binary
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release && rm -rf src target/release/deps/claude_notify*

# Build real source
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/claude-notify /usr/local/bin/claude-notify
ENTRYPOINT ["claude-notify", "serve"]
