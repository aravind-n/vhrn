//! Shared outbound TLS client configuration.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use rustls::pki_types::ServerName;

use crate::domain::target::{LocalTlsIdentity, LoopbackAuthority};

/// Builds the single production client configuration used by all outbound HTTPS connectors.
pub(crate) fn production_client_config() -> Result<Arc<rustls::ClientConfig>> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    client_config(roots)
}

/// Builds a verified client configuration for production and focused TLS tests.
pub(crate) fn client_config(roots: rustls::RootCertStore) -> Result<Arc<rustls::ClientConfig>> {
    Ok(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("configure supported TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ))
}

/// Produces the verified TLS identity for an already validated local authority.
pub(crate) fn local_server_name(authority: &LoopbackAuthority) -> Result<ServerName<'static>> {
    match authority.tls_identity() {
        LocalTlsIdentity::DnsLocalhost => {
            ServerName::try_from("localhost").map_err(|_| anyhow::anyhow!("TLS identity"))
        }
        LocalTlsIdentity::Ip(address) => Ok(ServerName::from(address)),
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use rcgen::{CertificateParams, KeyPair, SanType};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;

    fn certificate(identity: &str) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let mut parameters = CertificateParams::new(Vec::new()).unwrap();
        parameters
            .subject_alt_names
            .push(match identity.parse::<IpAddr>() {
                Ok(address) => SanType::IpAddress(address),
                Err(_) => SanType::DnsName(identity.try_into().unwrap()),
            });
        let key = KeyPair::generate().unwrap();
        let certificate = parameters.self_signed(&key).unwrap();
        (
            certificate.der().clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }

    async fn handshake(certificate_identity: &str, authority: &str) -> bool {
        let (certificate, key) = certificate(certificate_identity);
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let (client, server_io) = tokio::io::duplex(4096);
        let server =
            tokio::spawn(
                async move { TlsAcceptor::from(Arc::new(server)).accept(server_io).await },
            );
        let authority = LoopbackAuthority::parse(authority).unwrap();
        let client = TlsConnector::from(client_config(roots).unwrap())
            .connect(local_server_name(&authority).unwrap(), client)
            .await;
        let _ = server.await.unwrap();
        client.is_ok()
    }

    #[tokio::test]
    async fn local_tls_identity_uses_trusted_dns_and_ip_names_without_a_socket() {
        for (certificate, matching, mismatch) in [
            ("localhost", "localhost:443", "127.0.0.1:443"),
            ("127.0.0.1", "127.0.0.1:443", "[::1]:443"),
            ("::1", "[::1]:443", "localhost:443"),
        ] {
            assert!(
                handshake(certificate, matching).await,
                "{certificate} should match"
            );
            assert!(
                !handshake(certificate, mismatch).await,
                "{certificate} must not match {mismatch}"
            );
        }
    }
}
