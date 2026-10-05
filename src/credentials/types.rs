//! Credential **references**, **keys**, **provenance** and the value-free view.
//!
//! Two *disjoint* key spaces (borrowed from DSH `packages/credentials/`, where
//! `CredentialRef` and `CredentialKey` are separate types on purpose):
//!
//! - [`CredentialRef`] — an **environment variable name** (`OPENAI_API_KEY`).
//!   This is what a config file or a session parameter may name. It is
//!   resolved, per operation, against the four trust layers in
//!   [`super::CredentialStore`].
//! - [`CredentialKey`] — `<scope>/<id>`, where `scope` is the **registrant**
//!   (who owns the credential). It never names an env var, so a key handed to
//!   an authorization flow cannot be used to read `std::env`.
//!
//! Keeping them disjoint is what lets the read side ([`CredentialInfo`]) travel
//! to a frontend: it has **no field that can carry a value**, by construction.

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::error::{Error, Result};

/// Machine-readable discriminator carried by [`Error::Credential`].
///
/// [`Error::Credential`]: crate::error::Error::Credential
///
/// Callers branch on this instead of matching the message: an authorization
/// **decline** (a human said no) and an authorization **failure** (the UI, the
/// file or the network broke) need different copy and different retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialErrorCode {
    /// A reference or key is malformed (empty, whitespace, `=`, bad shape).
    Invalid,
    /// The reference resolved to nothing — no layer produced a value.
    /// An empty value is *absence*, never "configured".
    Missing,
    /// A credential file is readable by group/other. Refused before its
    /// contents are read.
    InsecureFile,
    /// A credential file could not be parsed. The report carries the location
    /// only — never the offending source line (which holds a secret).
    Parse,
    /// Authorization was declined by the user/UI.
    Declined,
    /// Authorization failed for an infrastructure reason, or a claimed commit
    /// could not be observed.
    Failed,
    /// The flow reported success but the commit was not observed.
    Unverified,
    /// A flow is already registered for this key.
    DuplicateFlow,
    /// No flow is registered for this key.
    NoFlow,
    /// Another attempt for this key is already in flight.
    InProgress,
}

impl CredentialErrorCode {
    /// Stable wire/log form. Never change an existing string — downstream
    /// error handling keys on it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "credential_invalid",
            Self::Missing => "credential_missing",
            Self::InsecureFile => "credential_file_insecure",
            Self::Parse => "credential_parse",
            Self::Declined => "authorization_declined",
            Self::Failed => "authorization_failed",
            Self::Unverified => "authorization_unverified",
            Self::DuplicateFlow => "authorization_duplicate_flow",
            Self::NoFlow => "authorization_no_flow",
            Self::InProgress => "authorization_in_progress",
        }
    }
}

impl fmt::Display for CredentialErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An environment variable name a credential is resolved through.
///
/// Names are not secrets — they may appear in logs, config files and error
/// messages. Values must not.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Validate and wrap `name` as a reference.
    pub fn parse(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_component(&name, "credential reference")?;
        Ok(Self(name))
    }

    /// The underlying env var name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for CredentialRef {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// A registered credential identity: `<scope>/<id>`, `scope` = registrant.
///
/// This is the key an [`super::AuthorizationRegistry`] flow is registered
/// under. It is deliberately *not* a [`CredentialRef`]: a frontend that only
/// knows a `CredentialKey` cannot ask for the value of an env var.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct CredentialKey {
    scope: String,
    id: String,
}

impl CredentialKey {
    /// Build a key from its two parts. Both must be non-empty and free of
    /// whitespace, control characters and `=`.
    pub fn new(scope: impl Into<String>, id: impl Into<String>) -> Result<Self> {
        let scope = scope.into();
        let id = id.into();
        validate_component(&scope, "credential key scope")?;
        validate_component(&id, "credential key id")?;
        if scope.contains('/') || id.contains('/') {
            return Err(Error::Credential {
                code: CredentialErrorCode::Invalid,
                message: "credential key parts must not contain '/'".to_string(),
            });
        }
        Ok(Self { scope, id })
    }

    /// Parse the `<scope>/<id>` wire form. Exactly one `/` is required.
    pub fn parse(s: &str) -> Result<Self> {
        let mut parts = s.split('/');
        let scope = parts.next().unwrap_or_default();
        let id = parts.next().unwrap_or_default();
        if parts.next().is_some() || scope.is_empty() || id.is_empty() {
            return Err(Error::Credential {
                code: CredentialErrorCode::Invalid,
                message: format!("credential key {s:?} must have the form <scope>/<id>"),
            });
        }
        Self::new(scope, id)
    }

    /// The registrant part.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The id part.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for CredentialKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.scope, self.id)
    }
}

impl FromStr for CredentialKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Where a resolved credential came from, in trust order (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialSource {
    /// The process environment. Read-only: it wins, and nothing can shadow it.
    InheritedEnv,
    /// The per-user credentials file (`<user-data-dir>/.credentials.yaml`).
    /// The only writable layer.
    CredentialsFile,
    /// `<cwd>/.env`.
    CwdEnvFile,
    /// `<user-data-dir>/.env`.
    HomeEnvFile,
}

impl CredentialSource {
    /// Stable wire/log form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InheritedEnv => "inherited-env",
            Self::CredentialsFile => "credentials-file",
            Self::CwdEnvFile => "cwd-env-file",
            Self::HomeEnvFile => "home-env-file",
        }
    }

    /// Whether a value from this layer can be changed by writing the local
    /// credentials file. Inherited environment cannot — it shadows the file.
    pub fn writable(self) -> bool {
        !matches!(self, Self::InheritedEnv)
    }
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The read side of the credentials layer.
///
/// **Structurally value-free**: `configured` / `source` / `writable` are the
/// only fields, so this type is safe to hand to a frontend or to serialize
/// over the Remote API. The absence of a value slot is the guarantee — do not
/// add one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialInfo {
    key: CredentialKey,
    configured: bool,
    source: Option<CredentialSource>,
    writable: bool,
}

impl CredentialInfo {
    /// Build a view from a key and where (if anywhere) its value was found.
    pub fn from_source(key: CredentialKey, source: Option<CredentialSource>) -> Self {
        Self {
            // `map_or` over `Option::is_none_or` — the latter needs Rust 1.82
            // and this crate declares MSRV 1.75.
            writable: source.map_or(true, CredentialSource::writable),
            configured: source.is_some(),
            source,
            key,
        }
    }

    /// The credential this view describes.
    pub fn key(&self) -> &CredentialKey {
        &self.key
    }

    /// Whether any layer produced a non-empty value.
    pub fn configured(&self) -> bool {
        self.configured
    }

    /// Which layer produced the value, if any.
    pub fn source(&self) -> Option<CredentialSource> {
        self.source
    }

    /// Whether writing the local credentials file would take effect. `false`
    /// when an inherited env var shadows it.
    pub fn writable(&self) -> bool {
        self.writable
    }
}

fn validate_component(value: &str, what: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::Credential {
            code: CredentialErrorCode::Invalid,
            message: format!("{what} must not be empty"),
        });
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '=')
    {
        return Err(Error::Credential {
            code: CredentialErrorCode::Invalid,
            message: format!("{what} must not contain whitespace, control characters, or '='"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_ref_accepts_env_names() {
        let r = CredentialRef::parse("OPENAI_API_KEY").expect("valid ref");
        assert_eq!(r.as_str(), "OPENAI_API_KEY");
        assert_eq!(r.to_string(), "OPENAI_API_KEY");
        assert_eq!(
            "DEEPSEEK_API_KEY"
                .parse::<CredentialRef>()
                .unwrap()
                .as_str(),
            "DEEPSEEK_API_KEY"
        );
    }

    #[test]
    fn credential_ref_rejects_empty_whitespace_control_and_equals() {
        for bad in ["", " ", "A B", "A\tB", "A=B", "A\nB"] {
            let err = CredentialRef::parse(bad).unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::Credential {
                        code: CredentialErrorCode::Invalid,
                        ..
                    }
                ),
                "{bad:?} must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn credential_key_round_trips_through_its_wire_form() {
        let key = CredentialKey::new("provider", "openai").expect("valid key");
        assert_eq!(key.to_string(), "provider/openai");
        assert_eq!(CredentialKey::parse("provider/openai").unwrap(), key);
        assert_eq!(key.scope(), "provider");
        assert_eq!(key.id(), "openai");
    }

    #[test]
    fn credential_key_rejects_ill_formed_wire_forms() {
        for bad in ["", "/", "a/", "/b", "a/b/c", "a b/c"] {
            assert!(
                CredentialKey::parse(bad).is_err(),
                "{bad:?} must not parse as a credential key"
            );
        }
        // A scope or id may not smuggle a second separator in via `new`.
        assert!(CredentialKey::new("a/b", "c").is_err());
    }

    #[test]
    fn credential_info_has_no_value_slot() {
        // The value-free guarantee is structural: assert the exact serialized
        // field set so a future "helpful" `value` field fails this test, and
        // assert a secret-shaped string still cannot appear.
        let key = CredentialKey::new("provider", "openai").unwrap();
        let info = CredentialInfo::from_source(key, Some(CredentialSource::CredentialsFile));
        let json = serde_json::to_value(&info).unwrap();
        let mut fields: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        fields.sort_unstable();
        assert_eq!(fields, ["configured", "key", "source", "writable"]);

        let text = serde_json::to_string(&info).unwrap();
        assert!(
            !text.contains("sk-secret"),
            "a value-free view must never carry a secret: {text}"
        );
    }

    #[test]
    fn credential_info_reports_provenance_and_writability() {
        let key = CredentialKey::new("provider", "openai").unwrap();

        let unset = CredentialInfo::from_source(key.clone(), None);
        assert!(!unset.configured());
        assert_eq!(unset.source(), None);
        assert!(
            unset.writable(),
            "an unset credential can be configured locally"
        );

        let inherited =
            CredentialInfo::from_source(key.clone(), Some(CredentialSource::InheritedEnv));
        assert!(inherited.configured());
        assert_eq!(inherited.source(), Some(CredentialSource::InheritedEnv));
        assert!(
            !inherited.writable(),
            "an inherited env var shadows the local file"
        );

        let file = CredentialInfo::from_source(key, Some(CredentialSource::CredentialsFile));
        assert!(file.writable());
        assert_eq!(file.key().to_string(), "provider/openai");
    }

    #[test]
    fn credential_sources_have_stable_names_and_writability() {
        assert_eq!(CredentialSource::InheritedEnv.as_str(), "inherited-env");
        assert_eq!(
            CredentialSource::CredentialsFile.as_str(),
            "credentials-file"
        );
        assert_eq!(CredentialSource::CwdEnvFile.as_str(), "cwd-env-file");
        assert_eq!(CredentialSource::HomeEnvFile.as_str(), "home-env-file");
        assert!(!CredentialSource::InheritedEnv.writable());
        assert!(CredentialSource::CredentialsFile.writable());
    }

    #[test]
    fn error_codes_are_stable_and_distinguish_decline_from_failure() {
        assert_eq!(
            CredentialErrorCode::Declined.as_str(),
            "authorization_declined"
        );
        assert_eq!(CredentialErrorCode::Failed.as_str(), "authorization_failed");
        assert_ne!(
            CredentialErrorCode::Declined.as_str(),
            CredentialErrorCode::Failed.as_str(),
            "a decline and an infrastructure failure must differ at the code level"
        );
        assert_eq!(
            CredentialErrorCode::InsecureFile.to_string(),
            "credential_file_insecure"
        );
    }
}
