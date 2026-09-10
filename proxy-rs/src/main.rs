use anyhow::{Result, bail};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    let (shutdown, _) = watch::channel(false);
    let service = vhrn_proxy::service::run_from_env(shutdown.subscribe());
    tokio::pin!(service);
    tokio::select! {
        result = &mut service => result.map(|_| ()),
        signaled = wait_for_shutdown_signal() => {
            if signaled { let _ = shutdown.send(true); }
            let _ = service.await?;
            bail!("terminated")
        }
    }
}

async fn wait_for_shutdown_signal() -> bool {
    #[cfg(unix)]
    {
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return tokio::signal::ctrl_c().await.is_ok();
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.is_ok(),
            _ = terminate.recv() => true,
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.is_ok()
    }
}
