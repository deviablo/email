// SPDX-License-Identifier: GPL-3.0-or-later

//! Which certificates we trust. Normally the ones of the system; tests
//! against a local mail server add their own certificate authority.

use std::sync::{Arc, LazyLock};

use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

#[derive(Clone)]
pub struct Trust {
    config: Arc<ClientConfig>,
    extra_roots: Vec<Vec<u8>>,
}

impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trust")
            .field("extra_roots", &self.extra_roots.len())
            .finish()
    }
}

static SYSTEM: LazyLock<Trust> = LazyLock::new(|| Trust::build(Vec::new(), true));

impl Trust {
    pub fn system() -> Trust {
        SYSTEM.clone()
    }

    /// Trusts only the given certificate authorities (DER encoded).
    pub fn only(roots: Vec<Vec<u8>>) -> Trust {
        Trust::build(roots, false)
    }

    fn build(extra_roots: Vec<Vec<u8>>, system: bool) -> Trust {
        let mut roots = RootCertStore::empty();
        if system {
            let native = rustls_native_certs::load_native_certs();
            for error in &native.errors {
                tracing::debug!("skipping system certificate: {error}");
            }
            let (added, _) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                tracing::warn!("no system certificates found, encrypted connections will fail");
            }
        }
        roots.add_parsable_certificates(
            extra_roots
                .iter()
                .map(|der| CertificateDer::from(der.clone())),
        );
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Trust {
            config: Arc::new(config),
            extra_roots,
        }
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.config.clone()
    }

    /// Certificate authorities trusted on top of (or instead of) the system ones.
    pub fn extra_roots(&self) -> &[Vec<u8>] {
        &self.extra_roots
    }

    pub fn uses_system_roots(&self) -> bool {
        Arc::ptr_eq(&self.config, &SYSTEM.config)
    }
}
