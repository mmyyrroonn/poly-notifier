use std::collections::HashMap;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use dotenvy::dotenv;
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use pn_admin::create_router;
use pn_common::config::AppConfig;
use pn_common::db::init_db;
use pn_lp::types::SignalState;
use pn_lp::{
    AccountSnapshot, ExchangeAdapter, ExchangeEvent, FlattenIntent, LpService, ManagedOrder,
    MarketMetadata, PositionSnapshot, QuoteIntent, QuoteSide, ReconciliationSnapshot, Reporter,
    RuntimeFlags, RuntimeState, ServiceConfig, TokenMetadata, TradeFill,
};
use pn_notify::telegram::BotRegistry;
use pn_polymarket::{
    BootstrapState, ExecutionConfig, PolymarketExecutionClient, QuoteRequest,
    QuoteSide as SdkQuoteSide, StreamEvent as SdkStreamEvent,
};

#[derive(Clone)]
struct LiveExchangeAdapter {
    client: Arc<PolymarketExecutionClient>,
}

#[async_trait]
impl ExchangeAdapter for LiveExchangeAdapter {
    fn start(&self, event_tx: mpsc::UnboundedSender<ExchangeEvent>, cancel: CancellationToken) {
        let (sdk_tx, mut sdk_rx) = mpsc::unbounded_channel();
        self.client.start_streams(sdk_tx, cancel.clone());

        tokio::spawn(async move {
            while let Some(event) = sdk_rx.recv().await {
                let mapped = match event {
                    SdkStreamEvent::Book(book) => ExchangeEvent::Book(pn_lp::BookSnapshot {
                        asset_id: book.asset_id,
                        bids: book
                            .bids
                            .into_iter()
                            .map(|level| pn_lp::types::BookLevel {
                                price: level.price,
                                size: level.size,
                            })
                            .collect(),
                        asks: book
                            .asks
                            .into_iter()
                            .map(|level| pn_lp::types::BookLevel {
                                price: level.price,
                                size: level.size,
                            })
                            .collect(),
                        received_at: Utc::now(),
                    }),
                    SdkStreamEvent::Order(order) => ExchangeEvent::Order(ManagedOrder {
                        order_id: order.order_id,
                        asset_id: order.asset_id,
                        side: map_sdk_side(order.side),
                        price: order.price,
                        size: order.size,
                        created_at: Utc::now(),
                        status: order.status,
                    }),
                    SdkStreamEvent::Trade(fill) => ExchangeEvent::Trade(TradeFill {
                        trade_id: fill.trade_id,
                        order_id: fill.order_id,
                        asset_id: fill.asset_id,
                        side: map_sdk_side(fill.side),
                        price: fill.price,
                        size: fill.size,
                        status: fill.status,
                        received_at: Utc::now(),
                    }),
                    SdkStreamEvent::TickSize {
                        asset_id,
                        new_tick_size,
                    } => ExchangeEvent::TickSize {
                        asset_id,
                        new_tick_size,
                    },
                };

                if event_tx.send(mapped).is_err() {
                    break;
                }
            }
        });
    }

    async fn reconcile(&self) -> Result<ReconciliationSnapshot> {
        let snapshot = self.client.reconcile().await?;
        Ok(ReconciliationSnapshot {
            books: snapshot
                .books
                .into_iter()
                .map(|book| pn_lp::BookSnapshot {
                    asset_id: book.asset_id,
                    bids: book
                        .bids
                        .into_iter()
                        .map(|level| pn_lp::types::BookLevel {
                            price: level.price,
                            size: level.size,
                        })
                        .collect(),
                    asks: book
                        .asks
                        .into_iter()
                        .map(|level| pn_lp::types::BookLevel {
                            price: level.price,
                            size: level.size,
                        })
                        .collect(),
                    received_at: Utc::now(),
                })
                .collect(),
            open_orders: snapshot
                .open_orders
                .into_iter()
                .map(|order| ManagedOrder {
                    order_id: order.order_id,
                    asset_id: order.asset_id,
                    side: map_sdk_side(order.side),
                    price: order.price,
                    size: order.size,
                    created_at: Utc::now(),
                    status: order.status,
                })
                .collect(),
            positions: snapshot
                .positions
                .into_iter()
                .map(|position| PositionSnapshot {
                    asset_id: position.asset_id,
                    size: position.size,
                    avg_price: position.avg_price,
                })
                .collect(),
            account: AccountSnapshot {
                usdc_balance: snapshot.account.usdc_balance,
                token_balances: snapshot.account.token_balances,
                updated_at: Utc::now(),
            },
        })
    }

    async fn post_quotes(&self, quotes: &[QuoteIntent]) -> Result<Vec<ManagedOrder>> {
        let requests = quotes
            .iter()
            .map(|quote| QuoteRequest {
                asset_id: quote.asset_id.clone(),
                side: map_lp_side(quote.side.clone()),
                price: quote.price,
                size: quote.size,
            })
            .collect::<Vec<_>>();
        let posted = self.client.post_quotes(&requests).await?;
        Ok(posted
            .into_iter()
            .map(|order| ManagedOrder {
                order_id: order.order_id,
                asset_id: order.asset_id,
                side: map_sdk_side(order.side),
                price: order.price,
                size: order.size,
                created_at: Utc::now(),
                status: order.status,
            })
            .collect())
    }

    async fn cancel_orders(&self, order_ids: &[String]) -> Result<()> {
        self.client.cancel_orders(order_ids).await
    }

    async fn cancel_all(&self) -> Result<()> {
        self.client.cancel_all().await
    }

    async fn flatten(&self, intent: &FlattenIntent) -> Result<Option<TradeFill>> {
        let fill = self
            .client
            .flatten(&pn_polymarket::FlattenRequest {
                asset_id: intent.asset_id.clone(),
                side: map_lp_side(intent.side.clone()),
                price: intent.price,
                size: intent.size,
                use_fok: intent.use_fok,
            })
            .await?;
        Ok(map_flatten_fill(fill))
    }

    async fn post_heartbeat(&self, last_heartbeat_id: Option<&str>) -> Result<String> {
        self.client.post_heartbeat(last_heartbeat_id).await
    }

    async fn split(&self, amount: Decimal) -> Result<String> {
        self.client.split_position(amount).await
    }

    async fn merge(&self, amount: Decimal) -> Result<String> {
        self.client.merge_positions(amount).await
    }
}

#[derive(Clone)]
struct TelegramReporter {
    bot_registry: Arc<BotRegistry>,
    bot_id: String,
    chat_ids: Vec<i64>,
}

#[async_trait]
impl Reporter for TelegramReporter {
    async fn send(&self, report_type: &str, message: &str) -> Result<()> {
        if self.chat_ids.is_empty() {
            return Ok(());
        }

        let payload = format!(
            "<b>{}</b>\n<pre>{}</pre>",
            html_escape(report_type),
            html_escape(message),
        );

        for chat_id in &self.chat_ids {
            self.bot_registry
                .send_message(&self.bot_id, *chat_id, &payload)
                .await
                .map_err(anyhow::Error::msg)?;
        }

        Ok(())
    }
}

static RUSTLS_PROVIDER_READY: OnceLock<()> = OnceLock::new();

fn ensure_rustls_crypto_provider() -> Result<()> {
    if RUSTLS_PROVIDER_READY.get().is_some() {
        return Ok(());
    }

    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install rustls ring crypto provider"))?;
    }

    let _ = RUSTLS_PROVIDER_READY.set(());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    ensure_rustls_crypto_provider()?;
    let _ = dotenv();
    let config = AppConfig::load()?;
    validate_condition_id(&config.lp.trading.condition_id)?;
    let _logging_guards = init_tracing(&config)?;

    let pool = init_db(&config.database.url, config.database.max_connections).await?;

    let bot_registry = build_bot_registry();
    let reporter = build_reporter(&config, bot_registry.clone());

    let sdk_client = Arc::new(
        PolymarketExecutionClient::connect(ExecutionConfig {
            condition_id: config.lp.trading.condition_id.clone(),
            clob_base_url: config.lp.trading.clob_base_url.clone(),
            data_api_base_url: config.lp.trading.data_api_base_url.clone(),
            chain_id: config.lp.trading.chain_id,
        })
        .await?,
    );
    let include_inventory_approval =
        config.lp.inventory.auto_split_on_startup && config.lp.inventory.startup_split_amount > 0.0;
    let approval_status = sdk_client
        .check_approvals(include_inventory_approval)
        .await?;
    if !approval_status.is_ready() {
        if config.lp.approvals.auto_approve_on_startup {
            warn!(
                missing = ?approval_status.missing_permissions(),
                "startup approvals missing; auto-approve enabled"
            );
            sdk_client
                .ensure_approvals(include_inventory_approval)
                .await?;
        } else if config.lp.approvals.require_on_startup {
            anyhow::bail!(
                "missing required approvals: {}",
                approval_status.missing_permissions().join(", ")
            );
        } else {
            warn!(
                missing = ?approval_status.missing_permissions(),
                "startup approvals missing; continuing because require_on_startup=false"
            );
        }
    } else {
        info!("startup approvals verified");
    }
    let bootstrap = sdk_client.bootstrap().await?;
    let initial_state = build_initial_state(&config, bootstrap);

    let exchange: Arc<dyn ExchangeAdapter> = Arc::new(LiveExchangeAdapter {
        client: sdk_client.clone(),
    });

    let (service, control) = LpService::new(
        exchange,
        pool.clone(),
        build_service_config(&config)?,
        reporter,
        initial_state,
    );

    let cancel = CancellationToken::new();

    let service_cancel = cancel.clone();
    let service_handle = tokio::spawn(async move {
        if let Err(error) = service.run(service_cancel).await {
            error!(?error, "LP service exited with error");
        }
    });

    let admin_password = std::env::var("ADMIN_PASSWORD")
        .expect("ADMIN_PASSWORD env var must be set - the admin API controls live trading");
    let router = create_router(pool.clone(), admin_password, Some(control.clone()));
    let admin_addr = format!("{}:{}", config.lp.control.bind_addr, config.admin.port);
    let admin_cancel = cancel.clone();
    let admin_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .expect("failed to bind admin API");
        info!(%admin_addr, "admin API listening");
        axum::serve(listener, router)
            .with_graceful_shutdown(admin_cancel.cancelled_owned())
            .await
            .expect("admin API server error");
    });

    info!("LP daemon started; waiting for Ctrl+C");
    tokio::signal::ctrl_c().await?;
    cancel.cancel();

    let _ = tokio::join!(service_handle, admin_handle);
    Ok(())
}

fn init_tracing(config: &AppConfig) -> Result<(WorkerGuard, WorkerGuard)> {
    fs::create_dir_all(&config.lp.logging.directory).with_context(|| {
        format!(
            "failed to create log directory {}",
            config.lp.logging.directory
        )
    })?;

    let env_filter = EnvFilter::from_default_env()
        .add_directive("poly_lp=info".parse()?)
        .add_directive("pn_lp=info".parse()?)
        .add_directive("pn_polymarket=info".parse()?)
        .add_directive("pn_admin=info".parse()?);

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(&config.lp.logging.file_prefix)
        .filename_suffix("log")
        .max_log_files(config.lp.logging.max_files)
        .build(&config.lp.logging.directory)?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let stdout_appender = tracing_appender::non_blocking(std::io::stdout());
    let (stdout_writer, stdout_guard) = stdout_appender;

    let stdout_layer = fmt::layer()
        .compact()
        .with_target(true)
        .with_writer(stdout_writer);

    let file_layer = if config.lp.logging.json {
        fmt::layer()
            .json()
            .with_target(true)
            .with_ansi(false)
            .with_writer(file_writer)
            .boxed()
    } else {
        fmt::layer()
            .compact()
            .with_target(true)
            .with_ansi(false)
            .with_writer(file_writer)
            .boxed()
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    Ok((stdout_guard, file_guard))
}

fn build_bot_registry() -> Arc<BotRegistry> {
    let mut registry = BotRegistry::new();
    let tokens: Vec<String> = std::env::var("TELEGRAM_BOT_TOKENS")
        .unwrap_or_default()
        .split(',')
        .filter(|token| !token.trim().is_empty())
        .map(|token| token.trim().to_string())
        .collect();

    for token in tokens {
        registry.register(&token);
    }

    Arc::new(registry)
}

fn build_reporter(config: &AppConfig, bot_registry: Arc<BotRegistry>) -> Option<Arc<dyn Reporter>> {
    if config.lp.reporting.operator_chat_ids.is_empty() {
        return None;
    }

    let explicit_bot_id = config
        .lp
        .reporting
        .operator_bot_id
        .clone()
        .filter(|bot_id| !bot_id.trim().is_empty());

    let bot_id = explicit_bot_id.or_else(|| bot_registry.first_bot_id());
    let Some(bot_id) = bot_id else {
        warn!("operator chat ids configured but no Telegram bot token is available");
        return None;
    };

    Some(Arc::new(TelegramReporter {
        bot_registry,
        bot_id,
        chat_ids: config.lp.reporting.operator_chat_ids.clone(),
    }))
}

fn build_service_config(config: &AppConfig) -> Result<ServiceConfig> {
    Ok(ServiceConfig {
        decision: pn_lp::DecisionConfig {
            quote_size: decimal(config.lp.strategy.quote_size)?,
            min_spread: decimal(config.lp.strategy.min_spread)?,
            min_depth: decimal(config.lp.strategy.min_depth)?,
            quote_offset_ticks: config.lp.strategy.quote_offset_ticks,
            min_usdc_balance: decimal(config.lp.inventory.min_usdc_balance)?,
            min_token_balance: decimal(config.lp.inventory.min_token_balance)?,
        },
        risk: pn_lp::RiskConfig {
            max_position: decimal(config.lp.risk.max_position)?,
            flat_position_tolerance: decimal(config.lp.risk.flat_position_tolerance)?,
            stale_feed_after: chrono::Duration::seconds(
                config.lp.risk.stale_feed_after_secs as i64,
            ),
            auto_flatten_after_fill: config.lp.risk.auto_flatten_after_fill,
            flatten_use_fok: config.lp.risk.flatten_use_fok,
        },
        startup_split_amount: if config.lp.inventory.auto_split_on_startup
            && config.lp.inventory.startup_split_amount > 0.0
        {
            Some(decimal(config.lp.inventory.startup_split_amount)?)
        } else {
            None
        },
        heartbeat_interval: Duration::from_secs(config.lp.control.heartbeat_interval_secs),
        reconciliation_interval: Duration::from_secs(
            config.lp.control.reconciliation_interval_secs,
        ),
        report_interval: Duration::from_secs(config.lp.reporting.summary_interval_secs),
        snapshot_interval: Duration::from_secs(config.lp.logging.snapshot_interval_secs),
        max_quote_age: Duration::from_secs(config.lp.strategy.max_quote_age_secs),
    })
}

fn build_initial_state(config: &AppConfig, bootstrap: BootstrapState) -> RuntimeState {
    let now = Utc::now();
    let orderbook_active = bootstrap
        .books
        .iter()
        .all(|book| !book.bids.is_empty() && !book.asks.is_empty());

    RuntimeState {
        market: MarketMetadata {
            condition_id: bootstrap.market.condition_id,
            question: bootstrap.market.question,
            tokens: bootstrap
                .market
                .tokens
                .into_iter()
                .map(|token| TokenMetadata {
                    asset_id: token.asset_id,
                    outcome: token.outcome,
                    tick_size: token.tick_size,
                })
                .collect(),
        },
        books: bootstrap
            .books
            .into_iter()
            .map(|book| {
                (
                    book.asset_id.clone(),
                    pn_lp::BookSnapshot {
                        asset_id: book.asset_id,
                        bids: book
                            .bids
                            .into_iter()
                            .map(|level| pn_lp::types::BookLevel {
                                price: level.price,
                                size: level.size,
                            })
                            .collect(),
                        asks: book
                            .asks
                            .into_iter()
                            .map(|level| pn_lp::types::BookLevel {
                                price: level.price,
                                size: level.size,
                            })
                            .collect(),
                        received_at: now,
                    },
                )
            })
            .collect::<HashMap<_, _>>(),
        open_orders: bootstrap
            .open_orders
            .into_iter()
            .map(|order| ManagedOrder {
                order_id: order.order_id,
                asset_id: order.asset_id,
                side: map_sdk_side(order.side),
                price: order.price,
                size: order.size,
                created_at: now,
                status: order.status,
            })
            .collect(),
        positions: bootstrap
            .positions
            .into_iter()
            .map(|position| {
                (
                    position.asset_id.clone(),
                    PositionSnapshot {
                        asset_id: position.asset_id,
                        size: position.size,
                        avg_price: position.avg_price,
                    },
                )
            })
            .collect(),
        account: AccountSnapshot {
            usdc_balance: bootstrap.account.usdc_balance,
            token_balances: bootstrap.account.token_balances,
            updated_at: now,
        },
        signals: HashMap::from([
            (
                "orderbook".to_string(),
                SignalState {
                    active: orderbook_active,
                    reason: if orderbook_active {
                        "bootstrap book healthy".to_string()
                    } else {
                        "bootstrap book incomplete".to_string()
                    },
                },
            ),
            (
                "external".to_string(),
                SignalState {
                    active: config.lp.strategy.default_external_signal,
                    reason: "default external gate".to_string(),
                },
            ),
        ]),
        flags: RuntimeFlags {
            paused: false,
            flattening: false,
            heartbeat_healthy: false,
            market_feed_healthy: false,
            user_feed_healthy: true,
        },
        last_market_event_at: None,
        last_user_event_at: Some(now),
        last_heartbeat_at: None,
        last_heartbeat_id: None,
        last_decision_reason: Some("bootstrap".to_string()),
    }
}

fn decimal(value: f64) -> Result<Decimal> {
    // TOML numeric literals currently deserialize into f64 for this config surface.
    // Keep the conversion explicit and covered by tests for the expected LP ranges.
    Decimal::from_str(&value.to_string()).with_context(|| format!("invalid decimal {value}"))
}

fn map_sdk_side(side: SdkQuoteSide) -> QuoteSide {
    match side {
        SdkQuoteSide::Sell => QuoteSide::Sell,
        SdkQuoteSide::Buy => QuoteSide::Buy,
    }
}

fn map_lp_side(side: QuoteSide) -> SdkQuoteSide {
    match side {
        QuoteSide::Sell => SdkQuoteSide::Sell,
        QuoteSide::Buy => SdkQuoteSide::Buy,
    }
}

fn map_flatten_fill(fill: pn_polymarket::TradeFill) -> Option<TradeFill> {
    if fill.size <= Decimal::ZERO {
        return None;
    }

    Some(TradeFill {
        trade_id: fill.trade_id,
        order_id: fill.order_id,
        asset_id: fill.asset_id,
        side: map_sdk_side(fill.side),
        price: fill.price,
        size: fill.size,
        status: fill.status,
        received_at: Utc::now(),
    })
}

fn validate_condition_id(condition_id: &str) -> Result<()> {
    if condition_id.trim().is_empty() || condition_id == "replace-me" {
        anyhow::bail!(
            "lp.trading.condition_id is not configured. Set APP__LP__TRADING__CONDITION_ID or update config/default.toml"
        );
    }

    Ok(())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pn_common::config::{
        AdminConfig, AlertConfig, AppConfig, DatabaseConfig, LpApprovalConfig, LpConfig,
        LpControlConfig, LpInventoryConfig, LpLoggingConfig, LpReportingConfig, LpRiskConfig,
        LpStrategyConfig, LpTradingConfig, MonitorConfig, SchedulerConfig, TelegramConfig,
    };
    use pn_polymarket::{
        AccountSnapshot as SdkAccountSnapshot, BookLevel as SdkBookLevel,
        BookSnapshot as SdkBookSnapshot, BootstrapState, ManagedOrder as SdkManagedOrder,
        MarketMetadata as SdkMarketMetadata, PositionSnapshot as SdkPositionSnapshot,
        QuoteSide as SdkQuoteSide, TokenMetadata as SdkTokenMetadata,
    };

    use super::{
        build_initial_state, decimal, ensure_rustls_crypto_provider, map_flatten_fill,
        validate_condition_id,
    };

    #[test]
    fn decimal_round_trip_preserves_expected_lp_config_values() {
        assert_eq!(decimal(50.0).unwrap(), "50.0".parse().unwrap());
        assert_eq!(decimal(25.0).unwrap(), "25.0".parse().unwrap());
        assert_eq!(decimal(0.01).unwrap(), "0.01".parse().unwrap());
        assert_eq!(decimal(1.25).unwrap(), "1.25".parse().unwrap());
    }

    #[test]
    fn map_flatten_fill_drops_zero_size_acknowledgements() {
        assert!(map_flatten_fill(pn_polymarket::TradeFill {
            trade_id: "trade-1".to_string(),
            order_id: Some("order-1".to_string()),
            asset_id: "asset-yes".to_string(),
            side: SdkQuoteSide::Sell,
            price: "0".parse().unwrap(),
            size: "0".parse().unwrap(),
            status: "UNMATCHED:unconfirmed".to_string(),
        })
        .is_none());
    }

    #[test]
    fn validate_condition_id_rejects_placeholder_value() {
        assert!(validate_condition_id("replace-me").is_err());
        assert!(validate_condition_id("0xabc").is_ok());
    }

    #[test]
    fn ensure_rustls_crypto_provider_is_idempotent() {
        ensure_rustls_crypto_provider().unwrap();
        ensure_rustls_crypto_provider().unwrap();
    }

    #[test]
    fn build_initial_state_starts_with_market_feed_gated_until_live_book_arrives() {
        let config = AppConfig {
            database: DatabaseConfig {
                url: "sqlite::memory:".to_string(),
                max_connections: 1,
            },
            telegram: TelegramConfig {
                rate_limit_per_user: 60,
            },
            monitor: MonitorConfig {
                subscription_refresh_interval_secs: 60,
                ws_ping_interval_secs: 30,
                reconnect_base_delay_secs: 1,
                reconnect_max_delay_secs: 60,
            },
            alert: AlertConfig {
                cache_refresh_interval_secs: 60,
                default_cooldown_minutes: 5,
                price_flush_interval_secs: 30,
            },
            scheduler: SchedulerConfig {
                daily_summary_cron: "0 0 9 * * *".to_string(),
            },
            admin: AdminConfig { port: 3000 },
            lp: LpConfig {
                trading: LpTradingConfig {
                    condition_id: "0xabc".to_string(),
                    clob_base_url: "https://clob.example".to_string(),
                    gamma_base_url: "https://gamma.example".to_string(),
                    data_api_base_url: "https://data.example".to_string(),
                    chain_id: 137,
                },
                inventory: LpInventoryConfig {
                    min_usdc_balance: 100.0,
                    min_token_balance: 10.0,
                    auto_split_on_startup: false,
                    startup_split_amount: 0.0,
                },
                strategy: LpStrategyConfig {
                    quote_size: 10.0,
                    min_spread: 0.01,
                    min_depth: 50.0,
                    quote_offset_ticks: 1,
                    max_quote_age_secs: 10,
                    default_external_signal: true,
                },
                risk: LpRiskConfig {
                    max_position: 100.0,
                    flat_position_tolerance: 1.0,
                    auto_flatten_after_fill: true,
                    flatten_use_fok: false,
                    stale_feed_after_secs: 15,
                },
                approvals: LpApprovalConfig {
                    require_on_startup: true,
                    auto_approve_on_startup: false,
                },
                reporting: LpReportingConfig {
                    operator_bot_id: None,
                    operator_chat_ids: Vec::new(),
                    summary_interval_secs: 60,
                },
                control: LpControlConfig {
                    bind_addr: "127.0.0.1".to_string(),
                    heartbeat_interval_secs: 30,
                    reconciliation_interval_secs: 30,
                },
                logging: LpLoggingConfig {
                    snapshot_interval_secs: 60,
                    directory: "logs".to_string(),
                    file_prefix: "lp".to_string(),
                    max_files: 3,
                    json: true,
                },
            },
        };
        let state = build_initial_state(
            &config,
            BootstrapState {
                market: SdkMarketMetadata {
                    condition_id: "condition-1".to_string(),
                    question: "Will X happen?".to_string(),
                    tokens: vec![SdkTokenMetadata {
                        asset_id: "asset-yes".to_string(),
                        outcome: "Yes".to_string(),
                        tick_size: "0.01".parse().unwrap(),
                    }],
                },
                books: vec![SdkBookSnapshot {
                    asset_id: "asset-yes".to_string(),
                    bids: vec![SdkBookLevel {
                        price: "0.40".parse().unwrap(),
                        size: "100".parse().unwrap(),
                    }],
                    asks: vec![SdkBookLevel {
                        price: "0.45".parse().unwrap(),
                        size: "100".parse().unwrap(),
                    }],
                }],
                open_orders: vec![SdkManagedOrder {
                    order_id: "order-1".to_string(),
                    asset_id: "asset-yes".to_string(),
                    side: SdkQuoteSide::Buy,
                    price: "0.41".parse().unwrap(),
                    size: "10".parse().unwrap(),
                    status: "LIVE".to_string(),
                }],
                positions: vec![SdkPositionSnapshot {
                    asset_id: "asset-yes".to_string(),
                    size: "10".parse().unwrap(),
                    avg_price: "0.41".parse().unwrap(),
                }],
                account: SdkAccountSnapshot {
                    usdc_balance: "500".parse().unwrap(),
                    token_balances: HashMap::from([(
                        "asset-yes".to_string(),
                        "10".parse().unwrap(),
                    )]),
                },
            },
        );

        assert!(!state.flags.market_feed_healthy);
        assert!(state.flags.user_feed_healthy);
        assert!(state.last_market_event_at.is_none());
        assert!(state.last_user_event_at.is_some());
    }
}
