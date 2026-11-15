//! TLS at the server, so a proxy in front is an option and not a
//! requirement, as Kestrel's has been since 2.0: rustls on its aws-lc-rs
//! provider, TLS 1.3 and 1.2 and nothing older, no renegotiation,
//! which rustls never offers, and ALPN `http/1.1` (RFC 7301 §3.2), the
//! one protocol stage 1 serves. Each handshake's certificate is chosen by
//! a resolver asked once per handshake ([`Certificates`]), as OTP's
//! `sni_fun` and Kestrel's `ServerCertificateSelector` are, the object
//! stage 2's reload swaps (research/production-server.r2.md M16).

use std::collections::HashMap;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio_rustls::TlsAcceptor;

use crate::options::{CertificateFiles, Tls};

/// The handshakes of the TLS listener `tls` names, each certificate read
/// and its key held to it before the server starts.
///
/// # Errors
///
/// A certificate's files not read, holding no certificate, or a key that
/// is not the certificate's: each names the files.
pub(crate) fn acceptor(tls: &Tls) -> Result<TlsAcceptor, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let certificates = Certificates {
        default: tls
            .default
            .as_ref()
            .map(|files| certified(files, &provider))
            .transpose()?,
        by_host: tls
            .by_host
            .iter()
            .map(|(host, files)| Ok((host.clone(), certified(files, &provider)?)))
            .collect::<Result<_, String>>()?,
    };
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("TLS: {e}"))?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(certificates));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// One certificate's chain and key, read from their files and held to
/// each other.
fn certified(
    files: &CertificateFiles,
    provider: &CryptoProvider,
) -> Result<Arc<CertifiedKey>, String> {
    let named = |e: &dyn std::fmt::Display| {
        format!(
            "--certificate {},{}: {e}",
            files.chain.display(),
            files.key.display()
        )
    };
    let chain = CertificateDer::pem_file_iter(&files.chain)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| named(&e))?;
    if chain.is_empty() {
        return Err(named(&"the chain holds no certificate"));
    }
    let key = PrivateKeyDer::from_pem_file(&files.key).map_err(|e| named(&e))?;
    CertifiedKey::from_der(chain, key, provider)
        .map(Arc::new)
        .map_err(|e| named(&e))
}

/// The certificate each handshake gets: its host's, by the name it sends
/// (SNI, which rustls lowercases), or the default where it sends none or
/// one with no certificate of its own; with no default, that handshake
/// fails.
#[derive(Debug)]
struct Certificates {
    default: Option<Arc<CertifiedKey>>,
    by_host: HashMap<String, Arc<CertifiedKey>>,
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        hello
            .server_name()
            .and_then(|host| self.by_host.get(host))
            .or(self.default.as_ref())
            .cloned()
    }
}
