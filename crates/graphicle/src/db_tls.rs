//! TLS for PostgreSQL connections ([`crate::db`]): rustls with the ring
//! provider (the same TLS stack as Graphicle's HTTP client), the system's
//! certificate authorities or a PEM file of them, and the server's host
//! name checked against its certificate (libpq's `sslmode=verify-full`).

use postgres::Socket;
use postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Makes a TLS connector for each server host.
#[derive(Clone)]
pub struct MakeRustls {
    config: Arc<rustls::ClientConfig>,
}

impl MakeRustls {
    /// Trust the certificate authorities in the PEM file `root_cert`, or the
    /// system's when it is `None`.
    pub fn new(root_cert: Option<&Path>) -> crate::db::Result<Self> {
        let failed = |message: String| crate::db::Error::Connect(message);
        let mut roots = rustls::RootCertStore::empty();
        match root_cert {
            Some(path) => {
                use rustls::pki_types::pem::PemObject;
                let certificates =
                    rustls::pki_types::CertificateDer::pem_file_iter(path).map_err(|e| {
                        failed(format!(
                            "could not read the CA file {}: {e}",
                            path.display()
                        ))
                    })?;
                for certificate in certificates {
                    let certificate = certificate.map_err(|e| {
                        failed(format!(
                            "could not read the CA file {}: {e}",
                            path.display()
                        ))
                    })?;
                    roots.add(certificate).map_err(|e| {
                        failed(format!(
                            "a certificate in {} is not usable: {e}",
                            path.display()
                        ))
                    })?;
                }
            }
            None => {
                let found = rustls_native_certs::load_native_certs();
                for certificate in found.certs {
                    // A system certificate rustls cannot use is skipped, as
                    // the HTTP client skips it.
                    let _ = roots.add(certificate);
                }
            }
        }
        if roots.is_empty() {
            return Err(failed(
                "no certificate authority to check the database server's certificate against"
                    .into(),
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| failed(format!("TLS setup failed: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
        })
    }
}

impl MakeTlsConnect<Socket> for MakeRustls {
    type Stream = RustlsStream;
    type TlsConnect = RustlsConnect;
    type Error = rustls::pki_types::InvalidDnsNameError;

    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, Self::Error> {
        let name = rustls::pki_types::ServerName::try_from(domain.to_string())?;
        Ok(RustlsConnect {
            config: self.config.clone(),
            name,
        })
    }
}

/// The TLS handshake with one host.
pub struct RustlsConnect {
    config: Arc<rustls::ClientConfig>,
    name: rustls::pki_types::ServerName<'static>,
}

impl TlsConnect<Socket> for RustlsConnect {
    type Stream = RustlsStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream>> + Send>>;

    fn connect(self, stream: Socket) -> Self::Future {
        Box::pin(async move {
            let connector = tokio_rustls::TlsConnector::from(self.config);
            let stream = connector.connect(self.name, stream).await?;
            Ok(RustlsStream(Box::new(stream)))
        })
    }
}

/// A TLS connection to the server.
pub struct RustlsStream(Box<tokio_rustls::client::TlsStream<Socket>>);

impl AsyncRead for RustlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for RustlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

impl TlsStream for RustlsStream {
    /// No channel binding: SCRAM authentication still works, without
    /// binding the password exchange to this TLS session.
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_ca_file_is_refused_with_its_name() {
        let error = MakeRustls::new(Some(Path::new("/nonexistent/ca.pem")))
            .err()
            .expect("refused");
        assert!(error.to_string().contains("/nonexistent/ca.pem"), "{error}");
    }

    #[test]
    fn a_host_name_becomes_a_server_name() {
        let roots = rustls::RootCertStore::empty();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut make = MakeRustls {
            config: Arc::new(config),
        };
        assert!(make.make_tls_connect("db.example.com").is_ok());
        assert!(make.make_tls_connect("10.0.0.5").is_ok());
        assert!(make.make_tls_connect("not a host").is_err());
    }
}
