# Cardinal GUI

A native macOS management and monitoring dashboard for the [Cardinal](https://github.com/openkitbox/cardinal) LLM gateway.

Cardinal GUI is the companion client for the server repository. The Cardinal gateway provides the HTTP API, protocol translation, routing, metering, and provider integration; this repository provides the native macOS interface for managing and monitoring it.

## Features

- **Overview** — Aggregated usage statistics, speed metrics, activity feed, and usage trends
- **Providers** — Manage provider accounts, API keys, quota display, and model whitelists
- **Routes** — Configure routing strategies (ordered/smart), availability rules, and view live request dispatch
- **Requests** — Full request ledger with filtering, token counts, timing, cost, and error details
- **Settings** — Gateway connection, local display preferences, and DB-backed mail/OAuth settings

## Quick Start

### Prerequisites

- Rust stable toolchain
- A running [Cardinal](../cardinal) gateway

### Configuration

Create `~/.ox/config.toml`:

```toml
[gateway]
listen = "127.0.0.1:8787"
gateway_key = "your-admin-key"
```

For a remote gateway, set `listen` to the remote address (e.g., `gateway.example.com:8787`). This file only locates the Gateway and authenticates admin calls; it does not hold portal settings or secrets. See `config.example.toml` for the full example.

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

## Settings: local TOML vs Gateway database

The settings page mixes two kinds of settings:

- The existing 「显示」 cards (timezone and 销售模式) remain local `[gui]` settings in the config file. Manual topup and balance-adjust ledger buttons also remain.
- 「邮件发送」 and 「第三方登录」 (OAuth providers) are written through the Gateway admin API into Gateway PostgreSQL, via `/api/portal/settings` and `/api/portal/providers`. They take effect immediately and are shared with other portal clients; there are no corresponding TOML keys for them in this repository.

Secrets are write-only from the GUI: the API returns `********` for a configured secret and empty for none. On save, leave `********` to keep the stored value or clear the field to remove it; `mail_token`, SMTP password, and provider `client_secret` are never shown in plaintext. OAuth BFF and state secrets (including `bff_secret`) are Cloudflare Worker bindings and never appear in GUI config. The GUI's own tests cover validation and masking only; they do not run against a live Gateway/PostgreSQL deployment.

## License

GPL-3.0-only
