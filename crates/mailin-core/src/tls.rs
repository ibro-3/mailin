//! TLS setup shared by IMAP and SMTP.
//!
//! Certificates are verified against the operating system trust store through
//! `rustls-platform-verifier`. rustls needs a process wide crypto provider, so
//! one is installed before the first connection; `ring` is used because it
//! builds without a C toolchain beyond the compiler itself.

use std::sync::Arc;

use rustls::ClientConfig;
use rustls_platform_verifier::ConfigVerifierExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::error::{Error, Result};

pub(crate) fn install_crypto_provider() {
    // Installing twice fails harmlessly; another component may have won.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(crate) fn client_config() -> Result<Arc<ClientConfig>> {
    install_crypto_provider();
    let config = ClientConfig::with_platform_verifier()
        .map_err(|e| Error::Tls(format!("could not set up certificate verification: {e}")))?;
    Ok(Arc::new(config))
}

pub(crate) async fn connect(
    config: Arc<ClientConfig>,
    host: &str,
    tcp: TcpStream,
) -> Result<TlsStream<TcpStream>> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|e| Error::Tls(format!("invalid server name {host:?}: {e}")))?;
    TlsConnector::from(config)
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Tls(format!("TLS handshake with {host} failed: {e}")))
}
