//! Authenticated connector for the host-side loopback broker.

mod http;
mod protocol;

pub(crate) use http::BrokerConnector;
pub(crate) use protocol::{BrokerError, BrokerToken};
