use std::sync::Arc;

use anyhow::Result;
use dotenvy::dotenv;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use pn_admin::create_router;
use pn_alert::AlertEngine;
use pn_bot::run_bot;
use pn_common::config::AppConfig;
use pn_common::db::init_db;
use pn_common::events::{NotificationRequest, PriceUpdate};
use pn_monitor::MonitorService;
use pn_notify::service::NotifyService;
use pn_notify::telegram::BotRegistry;
use pn_polymarket::clob::ClobClient;
use pn_polymarket::gamma::GammaClient;
use pn_scheduler::DailySummaryJob;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file (ignore if missing)
    let _ = dotenv();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("poly_notifier=info".parse()?)
                .add_directive("pn_monitor=info".parse()?)
                .add_directive("pn_alert=info".parse()?)
                .add_directive("pn_bot=info".parse()?),
        )
        .init();

    info!("Starting Poly-Notifier...");

    // Load configuration
    let config = AppConfig::load()?;
    info!("Configuration loaded");

    // Initialize database
    let pool = init_db(&config.database.url, config.database.max_connections).await?;
    info!("Database initialized");

    // Create channels
    let (price_tx, _) = broadcast::channel::<PriceUpdate>(4096);
    let (notify_tx, notify_rx) = mpsc::channel::<NotificationRequest>(1024);

    // Create cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // Build bot registry from tokens
    let bot_tokens: Vec<String> = std::env::var("TELEGRAM_BOT_TOKENS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .collect();

    if bot_tokens.is_empty() {
        anyhow::bail!("TELEGRAM_BOT_TOKENS environment variable is required");
    }

    let mut bot_registry = BotRegistry::new();
    for token in &bot_tokens {
        bot_registry.register(token);
        info!("Registered bot: {}", token.split(':').next().unwrap_or("unknown"));
    }
    let bot_registry = Arc::new(bot_registry);

    // Create API clients
    let clob_client = Arc::new(ClobClient::new());
    let gamma_client = Arc::new(GammaClient::new());

    // Spawn monitor service
    let monitor = MonitorService::new(pool.clone(), price_tx.clone(), config.monitor.clone());
    let monitor_cancel = cancel.clone();
    let monitor_handle = tokio::spawn(async move {
        if let Err(e) = monitor.run(monitor_cancel).await {
            error!("Monitor service error: {}", e);
        }
    });

    // Spawn alert engine
    let alert_engine = AlertEngine::new(
        pool.clone(),
        price_tx.subscribe(),
        notify_tx.clone(),
        config.alert.clone(),
    );
    let alert_cancel = cancel.clone();
    let alert_handle = tokio::spawn(async move {
        if let Err(e) = alert_engine.run(alert_cancel).await {
            error!("Alert engine error: {}", e);
        }
    });

    // Spawn notification service
    let notify_service = NotifyService::new(
        pool.clone(),
        notify_rx,
        bot_registry.clone(),
        config.telegram.rate_limit_per_user,
    );
    let notify_cancel = cancel.clone();
    let notify_handle = tokio::spawn(async move {
        if let Err(e) = notify_service.run(notify_cancel).await {
            error!("Notification service error: {}", e);
        }
    });

    // Spawn daily summary scheduler
    let scheduler = DailySummaryJob::new(
        pool.clone(),
        notify_tx.clone(),
        config.scheduler.daily_summary_cron.clone(),
    );
    let scheduler_cancel = cancel.clone();
    let scheduler_handle = tokio::spawn(async move {
        if let Err(e) = scheduler.run(scheduler_cancel).await {
            error!("Scheduler error: {}", e);
        }
    });

    // Spawn admin API
    let admin_password = std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
    let admin_router = create_router(pool.clone(), admin_password);
    let admin_port = config.admin.port;
    let admin_cancel = cancel.clone();
    let admin_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", admin_port))
            .await
            .expect("Failed to bind admin API port");
        info!("Admin API listening on port {}", admin_port);
        axum::serve(listener, admin_router)
            .with_graceful_shutdown(admin_cancel.cancelled_owned())
            .await
            .expect("Admin API server error");
    });

    // Spawn Telegram bots
    let mut bot_handles = Vec::new();
    for token in &bot_tokens {
        let bot = teloxide::Bot::new(token);
        let bot_id = token.split(':').next().unwrap_or("unknown").to_string();
        let pool = pool.clone();
        let clob = clob_client.clone();
        let gamma = gamma_client.clone();
        let bot_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            info!("Starting bot {}", bot_id);
            if let Err(e) = run_bot(bot, bot_id.clone(), pool, clob, gamma, bot_cancel).await {
                error!("Bot {} error: {}", bot_id, e);
            }
        });
        bot_handles.push(handle);
    }

    info!("All services started. Press Ctrl+C to shutdown.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received, stopping services...");
    cancel.cancel();

    // Wait for all services to complete
    let _ = tokio::join!(
        monitor_handle,
        alert_handle,
        notify_handle,
        scheduler_handle,
        admin_handle,
    );
    for handle in bot_handles {
        let _ = handle.await;
    }

    info!("Poly-Notifier shutdown complete.");
    Ok(())
}
