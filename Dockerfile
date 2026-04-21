FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Dependency caching: copy workspace and member manifests, then build a dummy binary
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml server/Cargo.toml
COPY cli/Cargo.toml cli/Cargo.toml
COPY capabilities/Cargo.toml capabilities/Cargo.toml
COPY config/Cargo.toml config/Cargo.toml
COPY gateway/Cargo.toml gateway/Cargo.toml
COPY protocol/Cargo.toml protocol/Cargo.toml
COPY local-agent/Cargo.toml local-agent/Cargo.toml
# proto files needed by server build.rs
COPY proto/ proto/
COPY server/build.rs server/build.rs
# protocol has no internal path deps, so copy real source now for correct re-exports
COPY protocol/src/ protocol/src/
RUN mkdir -p server/src cli/src capabilities/src gateway/src config/src local-agent/src \
    && echo 'fn main() {}' > server/src/main.rs \
    && echo 'fn main() {}' > cli/src/main.rs \
    && echo 'pub fn dummy() {}' > capabilities/src/lib.rs \
    && echo 'pub fn dummy() {}' > config/src/lib.rs \
    && echo 'fn main() {}' > gateway/src/main.rs \
    && echo 'fn main() {}' > local-agent/src/main.rs \
    && cargo build --release -p agent-hub-server \
    && rm -rf server/src cli/src capabilities/src gateway/src config/src local-agent/src target/release/deps/agent_hub_server* target/release/deps/capabilities* target/release/deps/libcapabilities*

# Build real source (server + capabilities dependency)
COPY proto/ proto/
COPY protocol/src/ protocol/src/
COPY capabilities/src/ capabilities/src/
COPY config/src/ config/src/
COPY server/build.rs server/build.rs
COPY server/src/ server/src/
COPY server/templates/ server/templates/
RUN cargo build --release -p agent-hub-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/agent-hub-server /usr/local/bin/agent-hub-server
ENTRYPOINT ["agent-hub-server"]
