FROM rust:1.88-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/inference-super-router /app/inference-super-router
COPY endpoints.ron /app/endpoints.ron
COPY prompts/ /app/prompts/

RUN mkdir -p /app/data /app/public/.well-known

ENV RUST_LOG=info
EXPOSE 8080
CMD ["./inference-super-router"]
