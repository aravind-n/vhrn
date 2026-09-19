//! Authenticated connector for the host-side loopback broker.
#![allow(dead_code)] // Loopback routing consumes the broker connector.

mod http;
mod protocol;

#[allow(unused_imports)] // Loopback routing consumes the broker connector.
pub(crate) use http::BrokerConnector;
pub(crate) use protocol::BrokerToken;
