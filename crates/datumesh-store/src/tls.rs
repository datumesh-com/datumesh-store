//! The TLS connector every `PgStore::connect` / `connect_with` uses: rustls with the ring
//! provider, chosen here for this connector (no process-wide default provider is installed
//! or consulted), verifying the server's certificate and host name against the Mozilla roots
//! and the roots of [`PoolOptions`]. Whether a connection uses TLS is the URL's `sslmode`.

use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::error::{Error, Result};
use crate::pool::PoolOptions;

pub(crate) fn connector(options: &PoolOptions) -> Result<MakeRustlsConnect> {
    let mut roots = RootCertStore::empty();
    if options.webpki_roots {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    for cert in &options.tls_roots {
        // Checked when added to the options; checked again rather than trusted.
        roots.add(cert.clone()).map_err(|e| Error::Config(format!("root certificate: {e}")))?;
    }
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Config(format!("TLS: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}
