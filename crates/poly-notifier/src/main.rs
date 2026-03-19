use std::collections::HashMap;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
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
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use pn_admin::create_router;
use pn_common::config::AppConfig;
use pn_common::db::init_db;
use pn_lp::{
    AccountSnapshot, ControlCommand, ExchangeAdapter, ExchangeEvent, FlattenIntent, LpService,
    ManagedOrder, MarketMetadata, PositionSnapshot, QuoteIntent, QuoteSide, ReconciliationSnapshot,
    Reporter, RuntimeFlags, RuntimeState, ServiceConfig, TokenMetadata, TradeFill,
};
use pn_notify::telegram::BotRegistry;
use pn_polymarket::{
    BootstrapState, ExecutionConfig, PolymarketExecutionClient, QuoteRequest,
    QuoteSide as SdkQuoteSide, StreamEvent as SdkStreamEvent,
};
use pn_lp::types::SignalState;

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
                size: intent.size,
                use_fok: intent.use_fok,
            })
            .await?;
        Ok(Some(TradeFill {
            trade_id: fill.trade_id,
            order_id: fill.order_id,
            asset_id: fill.asset_id,
            side: map_sdk_side(fill.side),
            price: fill.price,
            size: fill.size,
            status: fill.status,
            received_at: Utc::now(),
        }))
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

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenv();
    let config = AppConfig::load()?;
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
    let include_inventory_approval = config.lp.inventory.auto_split_on_startup
        && config.lp.inventory.startup_split_amount > 0.0;
    let approval_status = sdk_client.check_approvals(include_inventory_approval).await?;
    if !approval_status.is_ready() {
        if config.lp.approvals.auto_approve_on_startup {
            warn!(
                missing = ?approval_status.missing_permissions(),
                "startup approvals missing; auto-approve enabled"
            );
            sdk_client.ensure_approvals(include_inventory_approval).await?;
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

    if config.lp.inventory.auto_split_on_startup && config.lp.inventory.startup_split_amount > 0.0 {
        let _ = control.send(ControlCommand::Split {
            amount: config.lp.inventory.startup_split_amount.to_string(),
            reason: "startup auto split".to_string(),
        });
    }

    let cancel = CancellationToken::new();

    let service_cancel = cancel.clone();
    let service_handle = tokio::spawn(async move {
        if let Err(error) = service.run(service_cancel).await {
            error!(?error, "LP service exited with error");
        }
    });

    let admin_password = std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
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

fn build_reporter(
    config: &AppConfig,
    bot_registry: Arc<BotRegistry>,
) -> Option<Arc<dyn Reporter>> {
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
        fills: Vec::new(),
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
        flags: RuntimeFlags::default(),
        last_market_event_at: Some(now),
        last_user_event_at: Some(now),
        last_heartbeat_at: None,
        last_heartbeat_id: None,
        last_decision_reason: Some("bootstrap".to_string()),
    }
}

fn decimal(value: f64) -> Result<Decimal> {
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

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
