use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use dotenvy::dotenv;
use futures_util::future::join_all;
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use pn_admin::create_router;
use pn_common::config::{AppConfig, LpMarketConfig};
use pn_common::db::init_db;
use pn_lp::types::SignalState;
use pn_lp::{
    AccountSnapshot, ExchangeAdapter, ExchangeEvent, FlattenIntent, LpService, ManagedOrder,
    MarketMetadata, MultiLpControlHandle, PositionSnapshot, QuoteIntent, QuoteMode, QuoteSide,
    ReconciliationSnapshot, Reporter, RewardSnapshot, RewardState, RuntimeFlags, RuntimeState,
    ServiceConfig, TokenMetadata, TradeFill,
};
use pn_notify::telegram::BotRegistry;
use pn_polymarket::{
    BootstrapState, ExecutionConfig, PolymarketExecutionClient, PolymarketSharedStreamClient,
    QuoteRequest, QuoteSide as SdkQuoteSide, RewardSnapshot as SdkRewardSnapshot,
    SharedStreamConfig, SharedStreamSubscription, StreamEvent as SdkStreamEvent,
};
use poly_lp::guard::{guard_heartbeat_url, spawn_guard_heartbeat_loop};
use poly_lp::shared_streams::{BuyReservationLedger, SharedEventFanout};

#[derive(Clone)]
struct LiveExchangeAdapter {
    condition_id: String,
    client: Arc<PolymarketExecutionClient>,
    reward_fetch_retries: usize,
    reward_fetch_backoff: Duration,
    shared_fanout: Option<SharedEventFanout>,
    asset_ids: Vec<String>,
    buy_ledger: BuyReservationLedger,
}

#[async_trait]
impl ExchangeAdapter for LiveExchangeAdapter {
    fn start(&self, event_tx: mpsc::UnboundedSender<ExchangeEvent>, cancel: CancellationToken) {
        if let Some(fanout) = &self.shared_fanout {
            fanout.register(self.asset_ids.clone(), event_tx);
            return;
        }

        let (sdk_tx, mut sdk_rx) = mpsc::unbounded_channel();
        self.client.start_streams(sdk_tx, cancel.clone());

        tokio::spawn(async move {
            while let Some(event) = sdk_rx.recv().await {
                let mapped = map_sdk_stream_event(event);

                if event_tx.send(mapped).is_err() {
                    break;
                }
            }
        });
    }

    async fn reconcile(&self) -> Result<ReconciliationSnapshot> {
        let snapshot = self.client.reconcile().await?;
        let open_orders = snapshot
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
            .collect::<Vec<_>>();
        self.buy_ledger.refresh_market(
            &self.condition_id,
            snapshot.account.usdc_balance,
            &open_orders,
        );
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
            open_orders,
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

    async fn fetch_reward_snapshot(&self) -> Result<Option<RewardSnapshot>> {
        let snapshot = self
            .client
            .fetch_reward_snapshot(self.reward_fetch_retries, self.reward_fetch_backoff)
            .await?;
        Ok(snapshot.map(map_reward_snapshot))
    }

    async fn post_quotes(&self, quotes: &[QuoteIntent]) -> Result<Vec<ManagedOrder>> {
        let previous_reserved = self
            .buy_ledger
            .reserve_quotes(&self.condition_id, quotes)
            .map_err(anyhow::Error::msg)?;
        let requests = quotes
            .iter()
            .map(|quote| QuoteRequest {
                asset_id: quote.asset_id.clone(),
                side: map_lp_side(quote.side.clone()),
                price: quote.price,
                size: quote.size,
            })
            .collect::<Vec<_>>();
        let posted = match self.client.post_quotes(&requests).await {
            Ok(posted) => posted,
            Err(error) => {
                self.buy_ledger
                    .restore_market(&self.condition_id, previous_reserved);
                return Err(error);
            }
        };
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
        self.client.cancel_market_orders(None).await?;
        self.buy_ledger.clear_market(&self.condition_id);
        Ok(())
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

fn map_sdk_stream_event(event: SdkStreamEvent) -> ExchangeEvent {
    match event {
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
    }
}

#[derive(Clone)]
struct PrefixedReporter {
    inner: Arc<dyn Reporter>,
    label: String,
}

#[async_trait]
impl Reporter for PrefixedReporter {
    async fn send(&self, report_type: &str, message: &str) -> Result<()> {
        self.inner
            .send(report_type, &format!("[{}]\n{}", self.label, message))
            .await
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

#[derive(Clone)]
struct ResolvedMarketConfig {
    condition_id: String,
    slug: Option<String>,
    question_hint: Option<String>,
    buy_budget_usdc: Option<f64>,
    quote_mode: String,
    quote_size: f64,
    quote_offset_ticks: u32,
    min_inside_ticks: u32,
    min_inside_depth_multiple: f64,
    min_usdc_balance: f64,
    min_token_balance: f64,
    auto_split_on_startup: bool,
    startup_split_amount: f64,
    max_position: f64,
    flat_position_tolerance: f64,
    auto_flatten_after_fill: bool,
    flatten_use_fok: bool,
    stale_feed_after_secs: u64,
}

struct MarketRuntimeSeed {
    resolved: ResolvedMarketConfig,
    client: Arc<PolymarketExecutionClient>,
    initial_state: RuntimeState,
    asset_ids: Vec<String>,
}

fn resolved_markets(config: &AppConfig) -> Result<Vec<ResolvedMarketConfig>> {
    if !config.lp.markets.is_empty() {
        let markets = config
            .lp
            .markets
            .iter()
            .filter(|market| market.enabled)
            .map(|market| resolve_market(config, Some(market)))
            .collect::<Result<Vec<_>>>()?;
        if markets.is_empty() {
            anyhow::bail!("no enabled lp markets are configured");
        }
        return Ok(markets);
    }

    let Some(condition_id) = config.lp.trading.condition_id.clone() else {
        anyhow::bail!("no enabled lp market is configured");
    };

    Ok(vec![ResolvedMarketConfig {
        condition_id,
        slug: None,
        question_hint: None,
        buy_budget_usdc: None,
        quote_mode: config.lp.strategy.quote_mode.clone(),
        quote_size: config.lp.strategy.quote_size,
        quote_offset_ticks: config.lp.strategy.quote_offset_ticks,
        min_inside_ticks: config.lp.strategy.min_inside_ticks,
        min_inside_depth_multiple: config.lp.strategy.min_inside_depth_multiple,
        min_usdc_balance: config.lp.inventory.min_usdc_balance,
        min_token_balance: config.lp.inventory.min_token_balance,
        auto_split_on_startup: config.lp.inventory.auto_split_on_startup,
        startup_split_amount: config.lp.inventory.startup_split_amount,
        max_position: config.lp.risk.max_position,
        flat_position_tolerance: config.lp.risk.flat_position_tolerance,
        auto_flatten_after_fill: config.lp.risk.auto_flatten_after_fill,
        flatten_use_fok: config.lp.risk.flatten_use_fok,
        stale_feed_after_secs: config.lp.risk.stale_feed_after_secs,
    }])
}

fn resolve_market(
    config: &AppConfig,
    market: Option<&LpMarketConfig>,
) -> Result<ResolvedMarketConfig> {
    let market = market.expect("market override required");
    if market.condition_id.trim().is_empty() {
        anyhow::bail!("lp.markets.condition_id must not be empty");
    }

    let strategy = market.strategy.as_ref();
    let inventory = market.inventory.as_ref();
    let risk = market.risk.as_ref();
    let capital = market.capital.as_ref();

    Ok(ResolvedMarketConfig {
        condition_id: market.condition_id.clone(),
        slug: market.slug.clone(),
        question_hint: market.question.clone(),
        buy_budget_usdc: capital.and_then(|capital| capital.buy_budget_usdc),
        quote_mode: strategy
            .and_then(|strategy| strategy.quote_mode.clone())
            .unwrap_or_else(|| config.lp.strategy.quote_mode.clone()),
        quote_size: strategy
            .and_then(|strategy| strategy.quote_size)
            .unwrap_or(config.lp.strategy.quote_size),
        quote_offset_ticks: strategy
            .and_then(|strategy| strategy.quote_offset_ticks)
            .unwrap_or(config.lp.strategy.quote_offset_ticks),
        min_inside_ticks: strategy
            .and_then(|strategy| strategy.min_inside_ticks)
            .unwrap_or(config.lp.strategy.min_inside_ticks),
        min_inside_depth_multiple: strategy
            .and_then(|strategy| strategy.min_inside_depth_multiple)
            .unwrap_or(config.lp.strategy.min_inside_depth_multiple),
        min_usdc_balance: inventory
            .and_then(|inventory| inventory.min_usdc_balance)
            .unwrap_or(config.lp.inventory.min_usdc_balance),
        min_token_balance: inventory
            .and_then(|inventory| inventory.min_token_balance)
            .unwrap_or(config.lp.inventory.min_token_balance),
        auto_split_on_startup: inventory
            .and_then(|inventory| inventory.auto_split_on_startup)
            .unwrap_or(config.lp.inventory.auto_split_on_startup),
        startup_split_amount: inventory
            .and_then(|inventory| inventory.startup_split_amount)
            .unwrap_or(config.lp.inventory.startup_split_amount),
        max_position: risk
            .and_then(|risk| risk.max_position)
            .unwrap_or(config.lp.risk.max_position),
        flat_position_tolerance: risk
            .and_then(|risk| risk.flat_position_tolerance)
            .unwrap_or(config.lp.risk.flat_position_tolerance),
        auto_flatten_after_fill: risk
            .and_then(|risk| risk.auto_flatten_after_fill)
            .unwrap_or(config.lp.risk.auto_flatten_after_fill),
        flatten_use_fok: risk
            .and_then(|risk| risk.flatten_use_fok)
            .unwrap_or(config.lp.risk.flatten_use_fok),
        stale_feed_after_secs: risk
            .and_then(|risk| risk.stale_feed_after_secs)
            .unwrap_or(config.lp.risk.stale_feed_after_secs),
    })
}

static RUSTLS_PROVIDER_READY: OnceLock<()> = OnceLock::new();

struct CliArgs {
    config_path: String,
}

fn parse_cli_args(args: Vec<String>) -> Result<CliArgs> {
    let mut config_path = "config/default.toml".to_string();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                config_path = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?;
            }
            "--help" | "-h" => {
                println!("Usage: poly-lp [--config path]");
                std::process::exit(0);
            }
            other => anyhow::bail!("unexpected argument '{other}'"),
        }
    }

    Ok(CliArgs { config_path })
}

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

fn validate_admin_password(admin_password: String) -> Result<String> {
    if admin_password.trim().is_empty() {
        anyhow::bail!("ADMIN_PASSWORD env var must not be empty - the admin API controls live trading");
    }

    Ok(admin_password)
}

fn load_admin_password() -> Result<String> {
    let admin_password = std::env::var("ADMIN_PASSWORD")
        .context("ADMIN_PASSWORD env var must be set - the admin API controls live trading")?;
    validate_admin_password(admin_password)
}

fn build_admin_addr(config: &AppConfig) -> String {
    format!("{}:{}", config.admin.bind_addr, config.admin.port)
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    ensure_rustls_crypto_provider()?;
    let _ = dotenv();
    let args = parse_cli_args(std::env::args().skip(1).collect())?;
    let config = AppConfig::load_from(&args.config_path)?;
    let markets = resolved_markets(&config)?;
    for market in &markets {
        validate_condition_id(&market.condition_id)?;
    }
    let _logging_guards = init_tracing(&config)?;

    let pool = init_db(&config.database.url, config.database.max_connections).await?;

    let bot_registry = build_bot_registry();
    let reporter = build_reporter(&config, bot_registry.clone());
    let cancel = CancellationToken::new();
    let admin_password = load_admin_password()?;
    let include_inventory_approval = markets
        .iter()
        .any(|market| market.auto_split_on_startup && market.startup_split_amount > 0.0);
    let mut approvals_verified = false;
    let mut seeds = Vec::with_capacity(markets.len());
    for market in markets {
        let sdk_client = Arc::new(
            PolymarketExecutionClient::connect(ExecutionConfig {
                condition_id: market.condition_id.clone(),
                clob_base_url: config.lp.trading.clob_base_url.clone(),
                data_api_base_url: config.lp.trading.data_api_base_url.clone(),
                chain_id: config.lp.trading.chain_id,
            })
            .await?,
        );
        if !approvals_verified {
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
            approvals_verified = true;
        }

        let bootstrap = sdk_client.bootstrap().await?;
        let initial_state = build_initial_state(&config, bootstrap);
        let asset_ids = initial_state
            .market
            .tokens
            .iter()
            .map(|token| token.asset_id.clone())
            .collect::<Vec<_>>();
        seeds.push(MarketRuntimeSeed {
            resolved: market,
            client: sdk_client,
            initial_state,
            asset_ids,
        });
    }

    let buy_ledger = BuyReservationLedger::default();
    for seed in &seeds {
        let buy_budget = seed.resolved.buy_budget_usdc.map(decimal).transpose()?;
        buy_ledger.configure_market(&seed.resolved.condition_id, buy_budget);
        buy_ledger.refresh_market(
            &seed.resolved.condition_id,
            seed.initial_state.account.usdc_balance,
            &seed.initial_state.open_orders,
        );
    }

    let shared_fanout = (seeds.len() > 1).then(SharedEventFanout::default);
    let shared_stream_forwarder = if let Some(fanout) = shared_fanout.clone() {
        let subscriptions = seeds
            .iter()
            .map(|seed| SharedStreamSubscription {
                condition_id: seed.resolved.condition_id.clone(),
                asset_ids: seed.asset_ids.clone(),
            })
            .collect::<Vec<_>>();
        let shared_client = PolymarketSharedStreamClient::connect(
            SharedStreamConfig {
                clob_base_url: config.lp.trading.clob_base_url.clone(),
                chain_id: config.lp.trading.chain_id,
            },
            &subscriptions,
        )
        .await?;
        let (sdk_tx, mut sdk_rx) = mpsc::unbounded_channel();
        shared_client.start(sdk_tx, cancel.clone());
        Some(tokio::spawn(async move {
            while let Some(event) = sdk_rx.recv().await {
                fanout.dispatch(map_sdk_stream_event(event));
            }
        }))
    } else {
        None
    };

    let mut controls = BTreeMap::new();
    let mut service_handles = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let label = seed
            .resolved
            .slug
            .clone()
            .or(seed.resolved.question_hint.clone())
            .unwrap_or_else(|| seed.resolved.condition_id.clone());
        let market_reporter = reporter
            .clone()
            .map(|inner| Arc::new(PrefixedReporter { inner, label }) as Arc<dyn Reporter>);
        let exchange: Arc<dyn ExchangeAdapter> = Arc::new(LiveExchangeAdapter {
            condition_id: seed.resolved.condition_id.clone(),
            client: seed.client.clone(),
            reward_fetch_retries: config.lp.strategy.reward_fetch_retries,
            reward_fetch_backoff: Duration::from_millis(config.lp.strategy.reward_fetch_backoff_ms),
            shared_fanout: shared_fanout.clone(),
            asset_ids: seed.asset_ids.clone(),
            buy_ledger: buy_ledger.clone(),
        });
        let (service, control) = LpService::new(
            exchange,
            pool.clone(),
            build_service_config_for_market(&config, &seed.resolved)?,
            market_reporter,
            seed.initial_state,
        );
        controls.insert(seed.resolved.condition_id.clone(), control);
        let service_cancel = cancel.clone();
        service_handles.push(tokio::spawn(async move {
            if let Err(error) = service.run(service_cancel).await {
                error!(?error, "LP service exited with error");
            }
        }));
    }

    let router = create_router(
        pool.clone(),
        admin_password,
        Some(MultiLpControlHandle::new(controls)),
    );
    let admin_addr = build_admin_addr(&config);
    let admin_listener = tokio::net::TcpListener::bind(&admin_addr)
        .await
        .context("failed to bind admin API")?;
    info!(%admin_addr, "admin API listening");
    let admin_cancel = cancel.clone();
    let admin_handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(admin_listener, router)
            .with_graceful_shutdown(admin_cancel.cancelled_owned())
            .await
        {
            error!(?error, "admin API server error");
        }
    });

    let guard_heartbeat_handle = guard_heartbeat_url(&config).map(|heartbeat_url| {
        info!(
            %heartbeat_url,
            interval_secs = config.guard.heartbeat_interval_secs,
            "guard heartbeat sender enabled"
        );
        spawn_guard_heartbeat_loop(
            heartbeat_url,
            Duration::from_secs(config.guard.heartbeat_interval_secs.max(1)),
            cancel.clone(),
        )
    });

    info!("LP daemon started; waiting for shutdown signal");
    wait_for_shutdown_signal().await?;
    cancel.cancel();

    let _ = join_all(service_handles).await;
    let _ = admin_handle.await;
    if let Some(shared_stream_forwarder) = shared_stream_forwarder {
        let _ = shared_stream_forwarder.await;
    }
    if let Some(guard_heartbeat_handle) = guard_heartbeat_handle {
        let _ = guard_heartbeat_handle.await;
    }
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
        .add_directive("lp=info".parse()?)
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

#[cfg(test)]
fn build_service_config(config: &AppConfig) -> Result<ServiceConfig> {
    let market = resolved_markets(config)?
        .into_iter()
        .next()
        .expect("resolved at least one market");
    build_service_config_for_market(config, &market)
}

fn build_service_config_for_market(
    config: &AppConfig,
    market: &ResolvedMarketConfig,
) -> Result<ServiceConfig> {
    let quote_mode = QuoteMode::from_str(&market.quote_mode).map_err(|error| {
        anyhow::anyhow!(
            "invalid lp.strategy.quote_mode '{}': {error}",
            market.quote_mode
        )
    })?;

    Ok(ServiceConfig {
        decision: pn_lp::DecisionConfig {
            quote_mode,
            quote_size: decimal(market.quote_size)?,
            min_spread: decimal(config.lp.strategy.min_spread)?,
            min_depth: decimal(config.lp.strategy.min_depth)?,
            quote_offset_ticks: market.quote_offset_ticks,
            min_usdc_balance: decimal(market.min_usdc_balance)?,
            min_token_balance: decimal(market.min_token_balance)?,
            min_inside_ticks: market.min_inside_ticks,
            min_inside_depth_multiple: decimal(market.min_inside_depth_multiple)?,
            reward_stale_after: chrono::Duration::seconds(
                config.lp.strategy.reward_stale_after_secs as i64,
            ),
        },
        risk: pn_lp::RiskConfig {
            max_position: decimal(market.max_position)?,
            flat_position_tolerance: decimal(market.flat_position_tolerance)?,
            stale_feed_after: chrono::Duration::seconds(market.stale_feed_after_secs as i64),
            auto_flatten_after_fill: market.auto_flatten_after_fill,
            flatten_use_fok: market.flatten_use_fok,
        },
        startup_split_amount: if market.auto_split_on_startup && market.startup_split_amount > 0.0 {
            Some(decimal(market.startup_split_amount)?)
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
        reward_refresh_interval: Duration::from_secs(
            config.lp.strategy.reward_refresh_interval_secs,
        ),
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
        terminal_order_ids: HashSet::new(),
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
        reward: RewardState::default(),
    }
}

fn map_reward_snapshot(snapshot: SdkRewardSnapshot) -> RewardSnapshot {
    RewardSnapshot {
        condition_id: snapshot.condition_id,
        max_spread: snapshot.max_spread,
        min_size: snapshot.min_size,
        total_daily_rate: snapshot.total_daily_rate,
        active_until: snapshot.active_until,
        token_prices: snapshot
            .token_prices
            .into_iter()
            .map(|token| (token.token_id, token.price))
            .collect(),
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
    use std::time::Duration;

    use pn_common::config::{
        AdminConfig, AlertConfig, AppConfig, DatabaseConfig, GuardConfig, LpApprovalConfig,
        LpConfig, LpControlConfig, LpInventoryConfig, LpLoggingConfig, LpReportingConfig,
        LpRiskConfig, LpStrategyConfig, LpTradingConfig, MonitorConfig, SchedulerConfig,
        TelegramConfig,
    };
    use pn_polymarket::{
        AccountSnapshot as SdkAccountSnapshot, BookLevel as SdkBookLevel,
        BookSnapshot as SdkBookSnapshot, BootstrapState, ManagedOrder as SdkManagedOrder,
        MarketMetadata as SdkMarketMetadata, PositionSnapshot as SdkPositionSnapshot,
        QuoteSide as SdkQuoteSide, TokenMetadata as SdkTokenMetadata,
    };

    use super::{
        build_admin_addr, build_initial_state, build_service_config, decimal,
        ensure_rustls_crypto_provider, map_flatten_fill, parse_cli_args,
        validate_admin_password, validate_condition_id, QuoteMode,
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
    fn parse_cli_args_accepts_explicit_config_path() {
        let args = parse_cli_args(vec!["--config".to_string(), "/tmp/multi.toml".to_string()])
            .expect("parse args");

        assert_eq!(args.config_path, "/tmp/multi.toml");
    }

    #[test]
    fn ensure_rustls_crypto_provider_is_idempotent() {
        ensure_rustls_crypto_provider().unwrap();
        ensure_rustls_crypto_provider().unwrap();
    }

    #[test]
    fn validate_admin_password_rejects_empty_values() {
        let error = validate_admin_password(String::new())
            .expect_err("empty password should fail");
        assert!(error.to_string().contains("ADMIN_PASSWORD"));
    }

    #[test]
    fn build_admin_addr_uses_admin_bind_addr() {
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
            admin: AdminConfig {
                bind_addr: "0.0.0.0".to_string(),
                port: 3000,
            },
            guard: GuardConfig::default(),
            lp: LpConfig {
                trading: LpTradingConfig {
                    condition_id: Some("0xabc".to_string()),
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
                    quote_mode: "inside".to_string(),
                    quote_size: 10.0,
                    min_spread: 0.01,
                    min_depth: 50.0,
                    quote_offset_ticks: 1,
                    max_quote_age_secs: 10,
                    reward_refresh_interval_secs: 30,
                    reward_stale_after_secs: 90,
                    reward_fetch_retries: 3,
                    reward_fetch_backoff_ms: 250,
                    min_inside_ticks: 1,
                    min_inside_depth_multiple: 1.5,
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
                markets: Vec::new(),
            },
        };

        assert_eq!(build_admin_addr(&config), "0.0.0.0:3000");
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
            admin: AdminConfig {
                bind_addr: "127.0.0.1".to_string(),
                port: 3000,
            },
            guard: GuardConfig::default(),
            lp: LpConfig {
                trading: LpTradingConfig {
                    condition_id: Some("0xabc".to_string()),
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
                    quote_mode: "inside".to_string(),
                    quote_size: 10.0,
                    min_spread: 0.01,
                    min_depth: 50.0,
                    quote_offset_ticks: 1,
                    max_quote_age_secs: 10,
                    reward_refresh_interval_secs: 30,
                    reward_stale_after_secs: 90,
                    reward_fetch_retries: 3,
                    reward_fetch_backoff_ms: 250,
                    min_inside_ticks: 1,
                    min_inside_depth_multiple: 1.5,
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
                markets: Vec::new(),
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

    #[test]
    fn build_service_config_maps_quote_mode_from_config() {
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
            admin: AdminConfig {
                bind_addr: "127.0.0.1".to_string(),
                port: 3000,
            },
            guard: GuardConfig::default(),
            lp: LpConfig {
                trading: LpTradingConfig {
                    condition_id: Some("0xabc".to_string()),
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
                    quote_mode: "outside".to_string(),
                    quote_size: 10.0,
                    min_spread: 0.01,
                    min_depth: 50.0,
                    quote_offset_ticks: 1,
                    max_quote_age_secs: 10,
                    reward_refresh_interval_secs: 30,
                    reward_stale_after_secs: 90,
                    reward_fetch_retries: 3,
                    reward_fetch_backoff_ms: 250,
                    min_inside_ticks: 1,
                    min_inside_depth_multiple: 1.5,
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
                markets: Vec::new(),
            },
        };

        let service_config = build_service_config(&config).expect("service config");

        assert_eq!(service_config.decision.quote_mode, QuoteMode::Outside);
        assert_eq!(service_config.decision.min_inside_ticks, 1);
        assert_eq!(
            service_config.decision.min_inside_depth_multiple,
            decimal(1.5).unwrap()
        );
        assert_eq!(
            service_config.reward_refresh_interval,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn build_service_config_rejects_unknown_quote_mode() {
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
            admin: AdminConfig {
                bind_addr: "127.0.0.1".to_string(),
                port: 3000,
            },
            guard: GuardConfig::default(),
            lp: LpConfig {
                trading: LpTradingConfig {
                    condition_id: Some("0xabc".to_string()),
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
                    quote_mode: "far_away".to_string(),
                    quote_size: 10.0,
                    min_spread: 0.01,
                    min_depth: 50.0,
                    quote_offset_ticks: 1,
                    max_quote_age_secs: 10,
                    reward_refresh_interval_secs: 30,
                    reward_stale_after_secs: 90,
                    reward_fetch_retries: 3,
                    reward_fetch_backoff_ms: 250,
                    min_inside_ticks: 1,
                    min_inside_depth_multiple: 1.5,
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
                markets: Vec::new(),
            },
        };

        let error = build_service_config(&config).expect_err("invalid quote mode should fail");

        assert!(error
            .to_string()
            .contains("invalid lp.strategy.quote_mode 'far_away'"));
    }
}
