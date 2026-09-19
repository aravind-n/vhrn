//! Values and decisions for the network boundary.
mod config;
mod diagnostics;
mod domain;
mod server;
mod shutdown;

pub use config::Config;
pub use shutdown::Shutdown;

/// Runs the configured proxy until its listener has drained after shutdown.
///
/// # Errors
///
/// Returns any startup, listener, or service error.
pub async fn run(config: Config, shutdown: Shutdown) -> anyhow::Result<()> {
    let context = std::sync::Arc::new(server::router::RequestContext::new(config));
    let listener = server::listener::bind(context.config.listen).await?;
    server::listener::serve(listener, context, shutdown).await
}
