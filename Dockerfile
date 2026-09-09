FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Build from the complete source snapshot so every workspace path dependency is present.
COPY . .
RUN cargo build --locked --release -p agent-hub-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/agent-hub-server /usr/local/bin/agent-hub-server
ENTRYPOINT ["agent-hub-server"]
