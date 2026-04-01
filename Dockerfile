FROM rust:1-bookworm AS builder

WORKDIR /app

# Dependency caching: copy workspace and member manifests, then build a dummy binary
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml server/Cargo.toml
COPY hook/Cargo.toml hook/Cargo.toml
COPY cli/Cargo.toml cli/Cargo.toml
COPY capabilities/Cargo.toml capabilities/Cargo.toml
COPY config/Cargo.toml config/Cargo.toml
COPY gateway/Cargo.toml gateway/Cargo.toml
RUN mkdir -p server/src hook/src cli/src capabilities/src config/src gateway/src \
    && echo 'fn main() {}' > server/src/main.rs \
    && echo 'fn main() {}' > hook/src/main.rs \
    && echo 'fn main() {}' > cli/src/main.rs \
    && echo 'pub fn dummy() {}' > capabilities/src/lib.rs \
    && echo 'pub fn dummy() {}' > config/src/lib.rs \
    && echo 'fn main() {}' > gateway/src/main.rs \
    && cargo build --release -p agent-hub-server \
    && rm -rf server/src hook/src cli/src capabilities/src config/src gateway/src target/release/deps/agent_hub_server* target/release/deps/capabilities* target/release/deps/libcapabilities* target/release/deps/config* target/release/deps/libconfig*

# Build real source (server + capabilities dependency)
COPY capabilities/src/ capabilities/src/
COPY config/src/ config/src/
COPY server/src/ server/src/
COPY server/templates/ server/templates/
RUN cargo build --release -p agent-hub-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/agent-hub-server /usr/local/bin/agent-hub-server
ENTRYPOINT ["agent-hub-server"]
