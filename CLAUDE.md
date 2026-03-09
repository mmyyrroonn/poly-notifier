# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Poly-Notifier is a Rust application that monitors Polymarket prediction markets and sends real-time price alerts to users via Telegram bots. It supports multiple Telegram bots, tiered user subscriptions, and an admin HTTP API.

## Build & Development Commands

```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo check                    # Type-check without codegen
cargo clippy                   # Lint
cargo test                     # Run all tests
cargo test -p pn-alert         # Test a single crate
cargo run                      # Run the binary
```

SQLx uses offline mode with a SQLite database. Database URL is set via `DATABASE_URL` env var (default: `sqlite:poly-notifier.db`).

## Architecture

### Workspace Crates

The project is a Cargo workspace with 8 library crates and 1 binary crate (`crates/poly-notifier`):

- **pn-common** — Foundation: config (from `config/default.toml`), DB models, pool init, unified error type, channel event types
- **pn-polymarket** — Polymarket API client: Gamma REST (market discovery), CLOB REST (prices), WebSocket (real-time prices)
- **pn-monitor** — WebSocket connection manager with auto-reconnect; emits `PriceUpdate` via `broadcast` channel
- **pn-alert** — Rule engine: evaluates alert rules against prices, deduplicates with cooldowns, emits `NotificationRequest` via `mpsc`
- **pn-notify** — Notification dispatcher: consumes `NotificationRequest`, applies per-user rate limiting, delivers via Telegram bot registry
- **pn-bot** — Telegram bot: commands (/start, /subscribe, /alert, etc.), multi-step dialogues, inline keyboards
- **pn-scheduler** — Background jobs: cron-based daily summary
- **pn-admin** — Axum HTTP admin API with Bearer token auth (users, tiers, stats)

### Data Flow

```
Polymarket WebSocket → pn-monitor → broadcast<PriceUpdate>
                                          ↓
                                     pn-alert (DashMap caches + dedup)
                                          ↓
                                     mpsc<NotificationRequest>
                                          ↓
                                     pn-notify → RateLimiter → BotRegistry → Telegram API
```

All services receive a `CancellationToken` for graceful shutdown. Main spawns: monitor, alert engine, notifier, bot dispatcher(s), scheduler, admin server.

### Key Design Patterns

- **Event-driven coupling**: Tokio broadcast (4096 cap) and mpsc (1024 cap) channels between services
- **Concurrent caches**: `DashMap` for lock-free rule and price caches in pn-alert
- **Multi-bot support**: Multiple Telegram bot tokens from `TELEGRAM_BOT_TOKENS` env var (comma-separated); users keyed by `(telegram_id, bot_id)`
- **Tiered subscriptions**: Users have tier + max_subscriptions limits

### Database

SQLite via SQLx with migrations in `/migrations/`. Tables: `users`, `markets`, `subscriptions`, `alerts`, `notification_log`.

### Configuration

- `config/default.toml` — Runtime config (DB, rate limits, intervals, cron, admin port)
- `.env` (from `.env.example`) — Secrets: `TELEGRAM_BOT_TOKENS`, `ADMIN_PASSWORD`, `DATABASE_URL`, `RUST_LOG`
