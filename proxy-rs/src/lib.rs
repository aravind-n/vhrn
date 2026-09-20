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

pub(crate) struct Bootstrap {
    pub(crate) listener: tokio::net::TcpListener,
    pub(crate) config: Config,
    pub(crate) audit: diagnostics::AuditService,
    pub(crate) health: std::sync::Arc<diagnostics::HealthService>,
    pub(crate) public: connect::public::PublicConnector,
    pub(crate) local: Option<connect::broker::BrokerConnector>,
    pub(crate) shutdown: Shutdown,
}

/// Runs the configured proxy until its listener has drained after shutdown.
///
/// # Errors
///
/// Returns any startup, broker, listener, or service error.
pub async fn run(config: Config, shutdown: Shutdown) -> anyhow::Result<()> {
    let Some(bootstrap) = bootstrap(config, shutdown).await? else {
        return Ok(());
    };
    server::listener::serve(bootstrap).await
}

async fn bootstrap(config: Config, shutdown: Shutdown) -> anyhow::Result<Option<Bootstrap>> {
    if shutdown.is_requested() {
        return Ok(None);
    }
    let public_policy = domain::policy::PolicyReader::load_public_strict(
        config.allowlists.as_slice(),
        &config.mode_file,
    );
    tokio::select! {
        biased;
        () = shutdown.cancelled() => return Ok(None),
        result = public_policy => result.map_err(|_| anyhow::anyhow!("startup_public_policy_invalid"))?,
    };
    if let Some(local) = &config.local {
        let local_policy =
            domain::policy::PolicyReader::load_local_strict(local.policy_paths.as_array());
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(None),
            result = local_policy => result.map_err(|_| anyhow::anyhow!("startup_local_policy_invalid"))?,
        };
    }
    let audit = config.deny_log.clone();
    tokio::select! {
        biased;
        () = shutdown.cancelled() => return Ok(None),
        opened = audit.verify_open() => if !opened {
            return Err(anyhow::anyhow!("startup_denial_log_unavailable"));
        },
    }
    let listener = tokio::select! {
        biased;
        () = shutdown.cancelled() => return Ok(None),
        result = server::listener::bind(config.listen) => result?,
    };
    let token = if let Some(local) = &config.local {
        let load = config::load_broker_token(local);
        Some(tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(None),
            result = load => result.map_err(|_| anyhow::anyhow!("startup_broker_token_invalid"))?,
        })
    } else {
        None
    };
    let local = if let (Some(value), Some(token)) = (&config.local, token) {
        let connector = connect::broker::BrokerConnector::new(value.broker_addr.clone(), token);
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(None),
            result = connector.ready() => {
                result.map_err(|_| anyhow::anyhow!("startup_broker_readiness_failed"))?;
            }
        }
        Some(connector)
    } else {
        None
    };
    let health = std::sync::Arc::new(diagnostics::HealthService::new(
        config.allowlists.as_slice().to_vec(),
        config.mode_file.clone(),
        config
            .local
            .as_ref()
            .map(|local| local.policy_paths.as_array().clone()),
        audit.clone(),
        shutdown.clone(),
    ));
    Ok(Some(Bootstrap {
        listener,
        config,
        audit,
        health,
        public: connect::public::PublicConnector::system(),
        local,
        shutdown,
    }))
}
