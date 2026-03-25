use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use axum::{
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use pn_common::config::AppConfig;
use pn_polymarket::DirectCancelConfig;
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub const HEARTBEAT_PATH: &str = "/heartbeat";
const HEALTH_PATH: &str = "/health";

#[derive(Debug, Clone)]
pub struct GuardRuntimeConfig {
    pub bind_addr: String,
    pub port: u16,
    pub heartbeat_timeout: Duration,
    pub check_interval: Duration,
}

#[derive(Debug)]
struct GuardState {
    started_at: Instant,
    last_heartbeat_at: Instant,
    cancel_triggered: bool,
    cancel_count: u64,
    last_cancel_error: Option<String>,
}

impl GuardState {
    fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            last_heartbeat_at: now,
            cancel_triggered: false,
            cancel_count: 0,
            last_cancel_error: None,
        }
    }
}

#[derive(Clone)]
struct SharedState {
    guard: Arc<Mutex<GuardState>>,
}

#[async_trait]
pub trait CancelAllTransport: Send + Sync {
    async fn cancel_all(&self, reason: &str) -> Result<()>;
}

pub fn guard_heartbeat_url(config: &AppConfig) -> Option<String> {
    if !config.guard.enabled {
        return None;
    }

    Some(format!(
        "http://{}:{}{}",
        loopback_target_host(&config.guard.bind_addr),
        config.guard.port,
        HEARTBEAT_PATH
    ))
}

pub fn build_guard_runtime_config(config: &AppConfig) -> GuardRuntimeConfig {
    GuardRuntimeConfig {
        bind_addr: config.guard.bind_addr.clone(),
        port: config.guard.port,
        heartbeat_timeout: Duration::from_secs(config.guard.heartbeat_timeout_secs),
        check_interval: Duration::from_secs(config.guard.check_interval_secs),
    }
}

pub fn build_guard_cancel_config(config: &AppConfig) -> DirectCancelConfig {
    DirectCancelConfig {
        clob_base_url: config.lp.trading.clob_base_url.clone(),
        chain_id: config.lp.trading.chain_id,
    }
}

pub async fn send_heartbeat(client: &Client, heartbeat_url: &str) -> Result<()> {
    let response = client
        .post(heartbeat_url)
        .send()
        .await
        .with_context(|| format!("failed to post heartbeat to {heartbeat_url}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "guard heartbeat returned unexpected status {}",
            response.status()
        ));
    }
    Ok(())
}

pub fn spawn_guard_heartbeat_loop(
    heartbeat_url: String,
    interval_duration: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let request_timeout = interval_duration.max(Duration::from_secs(1));
        let client = match Client::builder().timeout(request_timeout).build() {
            Ok(client) => client,
            Err(error) => {
                warn!(?error, "failed to build guard heartbeat client");
                return;
            }
        };

        if let Err(error) = send_heartbeat(&client, &heartbeat_url).await {
            warn!(%heartbeat_url, ?error, "guard heartbeat failed");
        }

        let mut tick = interval(interval_duration);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tick.tick().await;

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if let Err(error) = send_heartbeat(&client, &heartbeat_url).await {
                        warn!(%heartbeat_url, ?error, "guard heartbeat failed");
                    }
                }
            }
        }
    })
}

pub async fn run_guard<T>(
    config: GuardRuntimeConfig,
    transport: T,
    cancel: CancellationToken,
) -> Result<(SocketAddr, JoinHandle<Result<()>>)>
where
    T: CancelAllTransport + 'static,
{
    let GuardRuntimeConfig {
        bind_addr,
        port,
        heartbeat_timeout,
        check_interval,
    } = config;
    let shared_state = SharedState {
        guard: Arc::new(Mutex::new(GuardState::new(Instant::now()))),
    };
    let listener = TcpListener::bind(format!("{}:{}", bind_addr, port))
        .await
        .with_context(|| format!("failed to bind guard listener on {}:{}", bind_addr, port))?;
    let listen_addr = listener.local_addr().context("guard listener local addr")?;
    let router = Router::new()
        .route(HEARTBEAT_PATH, post(heartbeat))
        .route(HEALTH_PATH, get(health))
        .with_state(shared_state.clone());

    let server_cancel = cancel.clone();
    let watchdog_cancel = cancel.clone();
    let watchdog_state = shared_state.clone();

    let task = tokio::spawn(async move {
        let watchdog = tokio::spawn(async move {
            let mut tick = interval(check_interval);
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            tick.tick().await;

            loop {
                tokio::select! {
                    () = watchdog_cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(error) =
                            maybe_cancel_all(&watchdog_state, &transport, heartbeat_timeout).await
                        {
                            warn!(?error, "guard cancel-all attempt failed");
                        }
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        let serve_result = axum::serve(listener, router)
            .with_graceful_shutdown(server_cancel.cancelled_owned())
            .await
            .context("guard HTTP server failed");

        let watchdog_result = watchdog.await.context("guard watchdog task join failed")?;

        serve_result?;
        watchdog_result?;
        Ok(())
    });

    info!(
        %listen_addr,
        heartbeat_timeout_secs = heartbeat_timeout.as_secs(),
        "guard listening"
    );

    Ok((listen_addr, task))
}

async fn heartbeat(State(state): State<SharedState>) -> impl IntoResponse {
    let mut guard = state.guard.lock().await;
    guard.last_heartbeat_at = Instant::now();
    guard.cancel_triggered = false;
    Json(serde_json::json!({ "status": "ok" }))
}

async fn health(State(state): State<SharedState>) -> impl IntoResponse {
    let guard = state.guard.lock().await;
    Json(serde_json::json!({
        "status": "ok",
        "cancel_triggered": guard.cancel_triggered,
        "cancel_count": guard.cancel_count,
        "last_heartbeat_ms_ago": guard.last_heartbeat_at.elapsed().as_millis(),
        "uptime_ms": guard.started_at.elapsed().as_millis(),
        "last_cancel_error": guard.last_cancel_error.clone(),
    }))
}

async fn maybe_cancel_all(
    state: &SharedState,
    transport: &dyn CancelAllTransport,
    heartbeat_timeout: Duration,
) -> Result<()> {
    {
        let guard = state.guard.lock().await;
        if guard.cancel_triggered || guard.last_heartbeat_at.elapsed() < heartbeat_timeout {
            return Ok(());
        }
    }

    let reason = format!(
        "guard heartbeat timeout after {}s",
        heartbeat_timeout.as_secs_f64()
    );
    if let Err(error) = transport.cancel_all(&reason).await {
        let mut guard = state.guard.lock().await;
        guard.last_cancel_error = Some(error.to_string());
        return Err(error);
    }

    let mut guard = state.guard.lock().await;
    guard.cancel_triggered = true;
    guard.cancel_count += 1;
    guard.last_cancel_error = None;
    Ok(())
}

fn loopback_target_host(bind_addr: &str) -> &str {
    match bind_addr {
        "0.0.0.0" => "127.0.0.1",
        "::" => "[::1]",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pn_common::config::{
        AdminConfig, AlertConfig, AppConfig, DatabaseConfig, GuardConfig, LpApprovalConfig,
        LpConfig, LpControlConfig, LpInventoryConfig, LpLoggingConfig, LpReportingConfig,
        LpRiskConfig, LpStrategyConfig, LpTradingConfig, MonitorConfig, SchedulerConfig,
        TelegramConfig,
    };

    use super::*;

    struct RecordingTransport {
        calls: AtomicUsize,
        last_reason: Mutex<Option<String>>,
    }

    #[async_trait]
    impl CancelAllTransport for RecordingTransport {
        async fn cancel_all(&self, reason: &str) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_reason.lock().await = Some(reason.to_string());
            Ok(())
        }
    }

    struct FailingTransport;

    #[async_trait]
    impl CancelAllTransport for FailingTransport {
        async fn cancel_all(&self, _reason: &str) -> Result<()> {
            Err(anyhow!("direct cancel failed"))
        }
    }

    fn test_config() -> AppConfig {
        AppConfig {
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
                port: 36363,
            },
            guard: GuardConfig {
                enabled: true,
                bind_addr: "0.0.0.0".to_string(),
                port: 37373,
                heartbeat_interval_secs: 5,
                heartbeat_timeout_secs: 15,
                check_interval_secs: 1,
            },
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
        }
    }

    fn test_state() -> SharedState {
        SharedState {
            guard: Arc::new(Mutex::new(GuardState::new(Instant::now()))),
        }
    }

    #[test]
    fn guard_heartbeat_url_uses_loopback_when_bind_addr_is_wildcard() {
        let config = test_config();
        assert_eq!(
            guard_heartbeat_url(&config).as_deref(),
            Some("http://127.0.0.1:37373/heartbeat")
        );
    }

    #[test]
    fn build_guard_cancel_config_uses_only_direct_cancel_settings() {
        let config = test_config();
        let cancel = build_guard_cancel_config(&config);

        assert_eq!(cancel.clob_base_url, "https://clob.example");
        assert_eq!(cancel.chain_id, 137);
    }

    #[tokio::test]
    async fn heartbeat_rearms_guard_after_cancel_trigger() {
        let state = test_state();
        {
            let mut guard = state.guard.lock().await;
            guard.cancel_triggered = true;
            guard.last_heartbeat_at = Instant::now() - Duration::from_secs(30);
        }

        let _ = heartbeat(State(state.clone())).await.into_response();

        let guard = state.guard.lock().await;
        assert!(!guard.cancel_triggered);
        assert!(guard.last_heartbeat_at.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn maybe_cancel_all_uses_direct_transport_once_per_missed_heartbeat_window() {
        let state = test_state();
        {
            let mut guard = state.guard.lock().await;
            guard.last_heartbeat_at = Instant::now() - Duration::from_secs(30);
        }
        let transport = RecordingTransport {
            calls: AtomicUsize::new(0),
            last_reason: Mutex::new(None),
        };

        maybe_cancel_all(&state, &transport, Duration::from_secs(15))
            .await
            .expect("direct cancel should be triggered");
        maybe_cancel_all(&state, &transport, Duration::from_secs(15))
            .await
            .expect("repeat check should be ignored after first cancel");

        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        assert!(transport
            .last_reason
            .lock()
            .await
            .as_deref()
            .unwrap_or_default()
            .contains("guard heartbeat timeout"));

        let _ = heartbeat(State(state.clone())).await.into_response();
        {
            let mut guard = state.guard.lock().await;
            guard.last_heartbeat_at = Instant::now() - Duration::from_secs(30);
        }
        maybe_cancel_all(&state, &transport, Duration::from_secs(15))
            .await
            .expect("direct cancel should trigger again after a new heartbeat window");

        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn maybe_cancel_all_records_error_and_does_not_trip_guard_when_direct_cancel_fails() {
        let state = test_state();
        {
            let mut guard = state.guard.lock().await;
            guard.last_heartbeat_at = Instant::now() - Duration::from_secs(30);
        }

        let error = maybe_cancel_all(&state, &FailingTransport, Duration::from_secs(15))
            .await
            .expect_err("direct cancel should fail");
        assert!(error.to_string().contains("direct cancel failed"));

        let guard = state.guard.lock().await;
        assert!(!guard.cancel_triggered);
        assert_eq!(guard.cancel_count, 0);
        assert_eq!(
            guard.last_cancel_error.as_deref(),
            Some("direct cancel failed")
        );
    }
}
