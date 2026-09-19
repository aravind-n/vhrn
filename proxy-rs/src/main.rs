use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let config = vhrn_proxy::Config::resolve(|key| std::env::var(key).ok())?;
    let shutdown = vhrn_proxy::Shutdown::new();
    let service = vhrn_proxy::run(config, shutdown.clone());
    tokio::pin!(service);
    tokio::select! {
        result = &mut service => result,
        signal = wait_for_shutdown_signal() => match signal {
            Ok(()) => { shutdown.request(); service.await }
            Err(error) => { shutdown.request(); let _ = service.await; Err(error) }
        }
    }
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("register SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("wait for Ctrl-C"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("wait for Ctrl-C")
    }
}
