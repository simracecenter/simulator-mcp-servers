// SPDX-License-Identifier: GPL-3.0-or-later

//! Certificate fingerprint pinning for the local-destination transport.
//!
//! When `[local] cert_fingerprint` is configured the transport builds a
//! `rustls` client that accepts exactly one server certificate — the one whose
//! DER bytes hash to the pinned SHA-256 fingerprint. Signature verification on
//! the handshake is still performed via the `ring` provider's algorithms.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};

/// Hex-encoded SHA-256 digest of `der` (64 lowercase hex chars).
pub fn sha256_hex(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// [`ServerCertVerifier`] that accepts only the certificate whose SHA-256
/// fingerprint matches `fingerprint`, delegating signature verification to the
/// provider's WebPKI algorithms.
#[derive(Debug)]
struct PinnedVerifier {
    fingerprint: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if digest.as_ref() == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General(
                "server certificate fingerprint mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn parse_fingerprint(hex: &str) -> Result<[u8; 32], String> {
    let hex = hex.trim().to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("cert_fingerprint must be 64 lowercase hex characters (SHA-256)".to_owned());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| "cert_fingerprint contains invalid hex".to_owned())?;
    }
    Ok(out)
}

/// Build a [`rustls::ClientConfig`] that accepts only the certificate matching
/// `fingerprint_hex` (64 lowercase hex chars, SHA-256 of the DER end-entity).
pub fn pinned_client_config(fingerprint_hex: &str) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedVerifier {
        fingerprint: parse_fingerprint(fingerprint_hex)?,
        provider: provider.clone(),
    };

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS protocol version setup failed: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Build a `ureq::Agent` whose TLS connections are pinned to
/// `fingerprint_hex`. See [`pinned_client_config`].
pub fn pinned_agent(fingerprint_hex: &str) -> Result<ureq::Agent, String> {
    let cfg = pinned_client_config(fingerprint_hex)?;
    Ok(ureq::AgentBuilder::new().tls_config(cfg).build())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_known_answer() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn parse_fingerprint_rejects_bad_input() {
        assert!(parse_fingerprint(&"ab".repeat(32)).is_ok());
        assert!(parse_fingerprint("xyz").is_err());
        assert!(parse_fingerprint(&"ab".repeat(31)).is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
    }
}
