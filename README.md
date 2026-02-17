# inference-super-router

A payment-gated AI inference router that multiplexes requests to multiple AI backends (Kimi, OpenAI, MiniMax) using the [x402 payment protocol](https://www.x402.org). Clients pay per-request with on-chain tokens (USDC or Starkbot) and get proxied access to AI models through a unified OpenAI-compatible API.

## How It Works

1. Client sends a chat request to any model endpoint
2. If no payment header is present, the router returns `402 Payment Required` with payment details
3. Client creates an x402 payment proof and re-sends the request with an `X-PAYMENT` header
4. The router verifies payment via the facilitator, proxies the request to the AI backend, and queues settlement

## Available Endpoints

| Endpoint | Route | Model | Cost | Currency |
|---|---|---|---|---|
| Kimi K2.5 | `/kimi` | `kimi-k2.5` | 1,000 | USDC |
| GPT-4.1 Mini | `/openai-mini` | `gpt-4.1-mini` | 500 | USDC |
| MiniMax-01 | `/minimax` | `MiniMax-Text-01` | 50,000 | STARKBOT |

Each endpoint exposes two routes:
- `{prefix}/chat` — custom chat endpoint
- `{prefix}/api/v1/chat/completions` — OpenAI-compatible endpoint

Endpoints are configured in `endpoints.ron` and can be added/removed without code changes.

## Quick Start

### Prerequisites

- Rust 1.83+ (or Docker)
- API keys for your desired AI providers
- An Ethereum wallet address to receive payments
- Access to an [x402 facilitator](https://facilitator.x402.org)

### Run Locally

```bash
cp .env.example .env
# Edit .env with your keys and wallet address
cargo run
```

The server starts on `http://localhost:8080` by default.

### Run with Docker

```bash
docker build -t inference-super-router .
docker run -p 8080:8080 --env-file .env inference-super-router
```

The Dockerfile uses a multi-stage build (Rust 1.83 builder -> Debian bookworm-slim) with dependency caching for fast rebuilds.

## Environment Variables

### Server

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8080` | HTTP server port |
| `TEST_MODE` | `false` | Skip payment verification (for development) |
| `BASE_URL` | — | Public base URL of the service |
| `RUST_LOG` | `info` | Log level filter (`debug`, `info`, `warn`, `error`) |

### Wallet & Payment Infrastructure

| Variable | Required | Description |
|---|---|---|
| `BOT_WALLET_ADDRESS` | Yes | Ethereum address receiving payments (0x format) |
| `FACILITATOR_URL` | Yes | URL of the x402 facilitator service |
| `FACILITATOR_SIGNER` | No | Facilitator signer address (x402 v1 only) |

### USDC Settings (x402 v2)

| Variable | Default | Description |
|---|---|---|
| `USDC_NETWORK` | `eip155:8453` | CAIP-2 network ID (Base mainnet) |
| `USDC_TOKEN_ADDRESS` | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` | USDC contract address on Base |

### Starkbot Settings (x402 v1)

| Variable | Default | Description |
|---|---|---|
| `STARKBOT_NETWORK` | `base` | Network name |
| `STARKBOT_TOKEN_ADDRESS` | `0x587Cd533F418825521f3A1daa7CCd1E7339A1B07` | Starkbot token contract |
| `STARKBOT_TOKEN_SYMBOL` | `STARKBOT` | Token symbol |
| `STARKBOT_TOKEN_DECIMALS` | `18` | Token decimals |
| `STARKBOT_TOKEN_NAME` | `StarkBot` | Token name |
| `STARKBOT_TOKEN_VERSION` | `1` | Token version |

### Settlement

| Variable | Default | Description |
|---|---|---|
| `SETTLEMENT_QUEUE_MAX_SIZE` | `10000` | Max pending settlements in queue |
| `SETTLEMENT_DB_PATH` | `data/settlements.db` | SQLite database path for persistent settlement storage |

### Endpoints & API Keys

| Variable | Default | Description |
|---|---|---|
| `ENDPOINTS_CONFIG` | `endpoints.ron` | Path to the endpoint definitions file |
| `KIMI_API_KEY` | — | API key for Kimi (Moonshot AI) |
| `OPENAI_API_KEY` | — | API key for OpenAI |
| `MINIMAX_API_KEY` | — | API key for MiniMax |

API key variable names are defined per-endpoint in `endpoints.ron` via `api_key_env`, so adding a new provider just means adding a new env var.

## Deployment

### Docker

```bash
docker build -t inference-super-router .
docker run -d \
  --name inference-router \
  -p 8080:8080 \
  -v $(pwd)/data:/app/data \
  --env-file .env \
  inference-super-router
```

Mount `/app/data` to persist the settlement SQLite database across container restarts. The settlement worker automatically recovers pending and in-progress items on startup.

### Health & Monitoring

| Endpoint | Description |
|---|---|
| `GET /` | Service info — lists all available endpoints with pricing and routes |
| `GET /health` | Health check with settlement queue depth |
| `GET /metrics` | Settlement stats, worker stats, verification cache hit rates |

### Volumes & Persistence

- **`/app/data/settlements.db`** — SQLite database storing settlement state. Survives restarts; the worker picks up where it left off.
- **`/app/public/.well-known/`** — Static files directory for ACME/TLS certificate verification.
- **`/app/endpoints.ron`** — Endpoint config. Mount a custom file to change available models.
- **`/app/prompts/`** — System prompt files referenced by endpoints.

### TLS / Reverse Proxy

The server binds to `0.0.0.0:8080` over plain HTTP. For production, put it behind a reverse proxy (nginx, Caddy, Traefik) that handles TLS termination. The `.well-known` static file serving supports ACME challenges.

### CORS

The server allows all origins with `GET`, `POST`, and `OPTIONS` methods. Custom headers `x-payment`, `payment-required`, and `payment-response` are allowed/exposed for the x402 flow.

## Endpoint Configuration

Endpoints are defined in `endpoints.ron` using the [RON format](https://github.com/ron-rs/ron):

```ron
(
  endpoints: [
    (
      name: "kimi-k2.5",
      route_prefix: "/kimi",
      api_endpoint: "https://api.moonshot.ai/v1/chat/completions",
      api_key_env: "KIMI_API_KEY",
      model: "kimi-k2.5",
      archetype: "kimi",
      cost: "1000",
      payment_currency: "usdc",
      max_input_tokens: 50000,
      max_output_tokens: 50000,
      system_prompt_file: Some("prompts/kimi.md"),
      description: "Kimi K2.5 via x402",
    ),
  ],
)
```

| Field | Description |
|---|---|
| `name` | Unique identifier for the endpoint |
| `route_prefix` | URL prefix (e.g., `/kimi` registers `/kimi/chat` and `/kimi/api/v1/chat/completions`) |
| `api_endpoint` | Upstream AI API URL |
| `api_key_env` | Name of the env var holding the API key |
| `model` | Model identifier sent to the upstream API |
| `archetype` | Protocol adapter (`kimi`, `openai`, `minimax`) |
| `cost` | Per-request cost in token units |
| `payment_currency` | `usdc` (x402 v2) or `starkbot` (x402 v1) |
| `max_input_tokens` | Maximum input token limit |
| `max_output_tokens` | Maximum output token limit |
| `system_prompt_file` | Optional path to a system prompt file (`Some("path")` or `None`) |
| `description` | Human-readable description |

## Project Structure

```
src/
  main.rs              # Server setup, route registration
  config.rs            # GlobalConfig from environment
  endpoints.rs         # Endpoint loading from RON config
  handler.rs           # Chat request handler (proxy to AI backends)
  payment.rs           # Payment config and 402 response building
  error.rs             # AppError types and HTTP responses
  middleware/
    x402.rs            # Payment verification, rate limiting, nonce tracking
  models/
    chat.rs            # OpenAI-compatible request/response types
    domains.rs         # Domain types (EthAddress, Bytes32, Uint256)
  services/
    facilitator.rs     # x402 facilitator client (verify + settle)
    inference_client.rs  # Upstream AI API client
    nonce_tracker.rs   # Replay attack prevention
    rate_limiter.rs    # Per-address rate limiting (5 req/s)
    verification_cache.rs  # Verified payer cache (30s TTL)
    settlement_queue.rs    # Async settlement queue
    settlement_store.rs    # SQLite persistence layer
    settlement_worker.rs   # Background settlement processor
endpoints.ron          # Endpoint definitions
prompts/               # System prompt files
Dockerfile             # Multi-stage production build
.env.example           # Environment variable template
```
