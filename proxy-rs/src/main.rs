use anyhow::{Result, anyhow};

#[tokio::main]
async fn main() -> Result<()> {
    let config = vhrn_proxy::Config::resolve(|key| std::env::var(key).ok())?;
    let mut signals = ShutdownSignals::new()?;
    let shutdown = vhrn_proxy::Shutdown::new();
    let service = vhrn_proxy::run(config, shutdown.clone());
    tokio::pin!(service);
    tokio::select! {
        result = &mut service => result,
        signal = signals.next() => match signal {
            Ok(()) => {
                shutdown.request();
                tokio::select! {
                    result = &mut service => result,
                    signal = signals.next() => match signal {
                        Ok(()) => {
                            shutdown.force();
                            service.await
                        }
                        Err(error) => {
                            shutdown.force();
                            let _ = service.await;
                            Err(error)
                        }
                    }
                }
            }
            Err(error) => {
                shutdown.force();
                let _ = service.await;
                Err(error)
            }
        }
    }
}

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|_| anyhow!("signal_handler_registration_failed"))?;
        let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|_| anyhow!("signal_handler_registration_failed"))?;
        Ok(Self {
            interrupt,
            terminate,
        })
    }

    async fn next(&mut self) -> Result<()> {
        tokio::select! {
            signal = self.interrupt.recv() => signal
                .ok_or_else(|| anyhow!("signal_wait_failed")),
            signal = self.terminate.recv() => signal
                .ok_or_else(|| anyhow!("signal_wait_failed")),
        }
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) -> Result<()> {
        tokio::signal::ctrl_c()
            .await
            .map_err(|_| anyhow!("signal_wait_failed"))
    }
}
