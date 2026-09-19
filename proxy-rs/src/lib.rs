//! Values and decisions for the network boundary.
mod config;
mod connect;
mod diagnostics;
mod domain;
mod headers;
mod server;
mod shutdown;

pub use config::Config;
pub use shutdown::Shutdown;

/// Runs the configured proxy until its listener has drained after shutdown.
///
/// # Errors
///
/// Returns any startup, broker, listener, or service error.
pub async fn run(config: Config, shutdown: Shutdown) -> anyhow::Result<()> {
    let tls = connect::tls::production_client_config()?;
    let local = if let Some(value) = &config.local {
        let connector = connect::broker::BrokerConnector::with_tls_config(
            value.broker_addr.clone(),
            config::load_broker_token(value)?,
            tls.clone(),
        );
        connector.ready().await?;
        Some(connector)
    } else {
        None
    };
    let context = std::sync::Arc::new(server::router::RequestContext::new(
        config,
        connect::public::PublicConnector::system_with_tls(tls),
        local,
        shutdown.clone(),
    ));
    let listener = server::listener::bind(context.config.listen).await?;
    server::listener::serve(listener, context, shutdown).await
}
