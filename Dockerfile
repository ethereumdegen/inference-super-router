FROM rust:1.88 AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/inference-super-router /app/inference-super-router
COPY endpoints.ron /app/endpoints.ron
COPY prompts/ /app/prompts/

RUN mkdir -p /app/data /app/public/.well-known

EXPOSE 8080
CMD ["./inference-super-router"]
