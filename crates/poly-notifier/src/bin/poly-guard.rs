use anyhow::{bail, Result};
use dotenvy::dotenv;
use pn_common::config::AppConfig;
use poly_lp::guard::{build_guard_runtime_config, run_guard};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenv();
    init_tracing()?;

    let config = AppConfig::load()?;
    if !config.guard.enabled {
        bail!("guard is disabled; set guard.enabled=true or APP__GUARD__ENABLED=true");
    }

    let admin_password =
        std::env::var("ADMIN_PASSWORD").expect("ADMIN_PASSWORD must be set for poly-guard");
    let runtime_config = build_guard_runtime_config(&config, admin_password);
    let cancel = CancellationToken::new();
    let (listen_addr, handle) = run_guard(runtime_config, cancel.clone()).await?;

    info!(%listen_addr, "poly-guard started; waiting for Ctrl+C");
    tokio::signal::ctrl_c().await?;
    cancel.cancel();
    handle.await??;
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
