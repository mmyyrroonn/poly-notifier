use anyhow::{bail, Result};
use async_trait::async_trait;
use dotenvy::dotenv;
use pn_common::config::AppConfig;
use pn_polymarket::PolymarketDirectCancelClient;
use poly_lp::guard::{
    build_guard_cancel_config, build_guard_runtime_config, run_guard, CancelAllTransport,
};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

struct DirectCancelAllTransport {
    client: Arc<PolymarketDirectCancelClient>,
}

#[async_trait]
impl CancelAllTransport for DirectCancelAllTransport {
    async fn cancel_all(&self, reason: &str) -> Result<()> {
        info!(%reason, "guard triggering direct Polymarket cancel-all");
        self.client.cancel_all().await
    }
}

static RUSTLS_PROVIDER_READY: OnceLock<()> = OnceLock::new();

#[tokio::main]
async fn main() -> Result<()> {
    ensure_rustls_crypto_provider()?;
    let _ = dotenv();
    init_tracing()?;

    let config = AppConfig::load()?;
    if !config.guard.enabled {
        bail!("guard is disabled; set guard.enabled=true or APP__GUARD__ENABLED=true");
    }

    let runtime_config = build_guard_runtime_config(&config);
    let cancel_config = build_guard_cancel_config(&config);
    let transport = DirectCancelAllTransport {
        client: Arc::new(PolymarketDirectCancelClient::connect(cancel_config).await?),
    };
    let cancel = CancellationToken::new();
    let (listen_addr, handle) = run_guard(runtime_config, transport, cancel.clone()).await?;

    info!(%listen_addr, "poly-guard started; waiting for Ctrl+C");
    tokio::signal::ctrl_c().await?;
    cancel.cancel();
    handle.await??;
    Ok(())
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

fn init_tracing() -> Result<()> {
    let env_filter = EnvFilter::from_default_env()
        .add_directive("poly_guard=info".parse()?)
        .add_directive("poly_lp=info".parse()?);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().compact().with_target(true))
        .init();

    Ok(())
}
