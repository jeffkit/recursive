//! Credential **indirection**: references, local storage, and authorization.
//!
//! The gap this closes (issue #130): before this module, the credential
//! surface was "whatever the process environment holds, read once into
//! `Config::api_key`". That has no reference layer, no permission hardening on
//! a local credential file, and no way to agree to / rotate a credential
//! without restarting the process.
//!
//! ## The pieces
//!
//! - [`CredentialRef`] / [`CredentialKey`] — two *disjoint* key spaces.
//!   A reference names an env var and is what config may carry; a key names an
//!   owner (`<scope>/<id>`) and is what an authorization flow is registered
//!   under. Keeping them separate is what stops a UI key from reading the
//!   process environment.
//! - [`CredentialStore`] — the four trust layers (inherited env, then the
//!   writable `<user-data-dir>/.credentials.yaml`, then `<cwd>/.env`, then
//!   `<user-data-dir>/.env`), resolved **per operation** so a rotation lands on
//!   the next request. It refuses group/world-readable credential files before
//!   reading them, and its parse errors report a location only — never the
//!   source line, which holds a secret.
//! - [`CredentialInfo`] — the read side. Structurally value-free, so it is safe
//!   to hand to a frontend or ship over the Remote API.
//! - [`AuthorizationRegistry`] — one flow per key, one attempt at a time,
//!   success verified by an observed commit, and a `settled` event for the
//!   second UI. Declines and infrastructure failures are separated at the
//!   [`CredentialErrorCode`] level.
//!
//! ## Wiring
//!
//! [`ApiKeySource`] is the seam the LLM adapters use: an `OpenAiProvider` /
//! `AnthropicProvider` built with `with_api_key_source` resolves its key
//! immediately before each request instead of holding one from construction,
//! so a rotation committed into the writable layer is sent on the next
//! request.

use std::fmt;
use std::sync::Arc;

use crate::error::{Error, Result};

mod authorization;
mod store;
mod types;

pub use authorization::{
    AuthorizationDecision, AuthorizationFlow, AuthorizationRegistry, AuthorizationSettled,
    CallbackFlow,
};
pub use store::{CredentialStore, CREDENTIALS_FILE_NAME, ENV_FILE_NAME};
pub use types::{
    CredentialErrorCode, CredentialInfo, CredentialKey, CredentialRef, CredentialSource,
};

/// Resolves the outbound API key for a request.
///
/// Implementations must resolve *now* rather than return a cached value: the
/// point of the indirection layer is that a rotation made between two requests
/// is visible to the second one without restarting the process.
pub trait ApiKeySource: Send + Sync + fmt::Debug {
    /// The key to send. `Err` (not an empty string) when nothing is
    /// configured — an empty value must never masquerade as a credential.
    fn resolve_api_key(&self) -> Result<String>;
}

/// An [`ApiKeySource`] backed by a [`CredentialStore`] reference.
///
/// Each call re-resolves through the store, so the four trust layers (and any
/// rotation committed into the writable layer) apply per request.
#[derive(Debug, Clone)]
pub struct StoredCredential {
    store: Arc<CredentialStore>,
    reference: CredentialRef,
}

impl StoredCredential {
    /// Resolve `reference` through `store`.
    pub fn new(store: Arc<CredentialStore>, reference: CredentialRef) -> Self {
        Self { store, reference }
    }

    /// The reference this source resolves.
    pub fn reference(&self) -> &CredentialRef {
        &self.reference
    }
}

impl ApiKeySource for StoredCredential {
    fn resolve_api_key(&self) -> Result<String> {
        self.store
            .resolve(&self.reference)?
            .ok_or_else(|| Error::Credential {
                code: CredentialErrorCode::Missing,
                message: format!(
                    "no value configured for credential reference {}",
                    self.reference
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::env_lock;

    fn store(dir: &tempfile::TempDir) -> Arc<CredentialStore> {
        Arc::new(CredentialStore::new(
            dir.path().join("home"),
            dir.path().join("cwd"),
        ))
    }

    fn write_credentials(store: &CredentialStore, body: &str) {
        let path = store.credentials_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn stored_credential_resolves_through_the_store() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_SOURCE_BASIC";
        std::env::remove_var(name);
        write_credentials(&store, &format!("{name}: sk-source\n"));

        let source = StoredCredential::new(Arc::clone(&store), CredentialRef::parse(name).unwrap());
        assert_eq!(source.resolve_api_key().unwrap(), "sk-source");
        assert_eq!(source.reference().as_str(), name);
    }

    #[test]
    fn a_rotation_is_visible_to_the_next_resolution() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_SOURCE_ROTATE";
        std::env::remove_var(name);
        write_credentials(&store, &format!("{name}: sk-old\n"));

        let source = StoredCredential::new(Arc::clone(&store), CredentialRef::parse(name).unwrap());
        assert_eq!(source.resolve_api_key().unwrap(), "sk-old");

        // Rotate on disk; the same source, resolved again, must see it. This is
        // the "rotation takes effect on the next request" acceptance criterion
        // at the seam the providers use.
        write_credentials(&store, &format!("{name}: sk-new\n"));
        assert_eq!(source.resolve_api_key().unwrap(), "sk-new");
    }

    #[test]
    fn an_unconfigured_reference_is_a_missing_error_not_an_empty_key() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_SOURCE_ABSENT";
        std::env::remove_var(name);

        let source = StoredCredential::new(Arc::clone(&store), CredentialRef::parse(name).unwrap());
        let err = source.resolve_api_key().unwrap_err();
        assert_eq!(err.credential_code(), Some(CredentialErrorCode::Missing));
        assert!(
            err.to_string().contains(name),
            "the error must name the reference but never a value: {err}"
        );
    }

    #[test]
    fn an_empty_stored_value_is_reported_as_missing() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_SOURCE_EMPTY";
        std::env::remove_var(name);
        write_credentials(&store, &format!("{name}: \"\"\n"));

        let source = StoredCredential::new(Arc::clone(&store), CredentialRef::parse(name).unwrap());
        assert_eq!(
            source.resolve_api_key().unwrap_err().credential_code(),
            Some(CredentialErrorCode::Missing)
        );
    }
}
