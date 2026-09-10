// SPDX-License-Identifier: GPL-3.0-or-later

use std::{fs, path::Path, sync::Arc};

use rcgen::generate_simple_self_signed;
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    ServerConfig,
};
use sha2::{Digest, Sha256};

pub struct RigIdentity {
    pub cert_der: Vec<u8>,
    key_der: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: String,
}

impl RigIdentity {
    pub fn server_config(&self) -> Arc<ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("valid rustls protocol versions")
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(self.cert_der.clone())],
                    PrivateKeyDer::from(self.key_der.clone_key()),
                )
                .expect("generated certificate and key are valid"),
        )
    }
}

pub fn host_display_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "rig".to_string())
}

pub fn load_or_generate(dir: &Path) -> Result<RigIdentity, String> {
    let cert_path = dir.join("rig-cert.pem");
    let key_path = dir.join("rig-key.pem");
    if cert_path.exists() && key_path.exists() {
        return load_existing(&cert_path, &key_path);
    }

    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let certified = generate_simple_self_signed(vec![host_display_name(), "localhost".to_string()])
        .map_err(|error| error.to_string())?;
    fs::write(&cert_path, certified.cert.pem()).map_err(|error| error.to_string())?;
    fs::write(&key_path, certified.key_pair.serialize_pem()).map_err(|error| error.to_string())?;
    identity(
        certified.cert.der().to_vec(),
        certified.key_pair.serialize_der(),
    )
}

fn load_existing(cert_path: &Path, key_path: &Path) -> Result<RigIdentity, String> {
    use rustls::pki_types::pem::PemObject;

    let cert = CertificateDer::from_pem_file(cert_path).map_err(|error| error.to_string())?;
    let key = PrivatePkcs8KeyDer::from_pem_file(key_path).map_err(|error| error.to_string())?;
    identity(cert.as_ref().to_vec(), key.secret_pkcs8_der().to_vec())
}

fn identity(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<RigIdentity, String> {
    let digest = Sha256::digest(&cert_der);
    let fingerprint = digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    Ok(RigIdentity {
        cert_der,
        key_der: PrivatePkcs8KeyDer::from(key_der),
        fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_has_colon_fingerprint() {
        let dir =
            std::env::temp_dir().join(format!("simracecenter-rig-identity-{}", std::process::id()));
        let first = load_or_generate(&dir).unwrap();
        assert_eq!(first.fingerprint.len(), 95);
        assert!(first
            .fingerprint
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                if index % 3 == 2 {
                    *byte == b':'
                } else {
                    byte.is_ascii_hexdigit()
                }
            }));
        let second = load_or_generate(&dir).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        fs::remove_dir_all(dir).ok();
    }
}
