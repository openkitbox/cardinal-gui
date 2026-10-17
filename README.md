# Cardinal GUI

A native macOS management and monitoring dashboard for the [Cardinal](../cardinal) LLM gateway.

## Features

- **Overview** — Aggregated usage statistics, speed metrics, activity feed, and usage trends
- **Providers** — Manage provider accounts, API keys, quota display, and model whitelists
- **Routes** — Configure routing strategies (ordered/smart), availability rules, and view live request dispatch
- **Requests** — Full request ledger with filtering, token counts, timing, cost, and error details
- **Settings** — Gateway connection configuration and app info

## Quick Start

### Prerequisites

- Rust stable toolchain
- A running [Cardinal](../cardinal) gateway

### Configuration

Create `~/.ox/config.toml`:

```toml
[gateway]
gateway_url = "http://127.0.0.1:8787"
key = "your-admin-key"
```

For a remote gateway, set `gateway_url` to the remote address (e.g., `https://gateway.example.com`).

### Run

```bash
cargo build --release
./target/release/cardinal-gui
```

### Or run in development

```bash
cargo run
```

## Architecture

The GUI is a pure HTTP client of the gateway. It communicates via:

- **REST API** (`GatewayHttpClient`) — all management operations
- **SSE stream** (`/api/activity`) — real-time event feed

There is no embedded gateway logic; all state lives in the gateway process.

## License

GPL-3.0-only
