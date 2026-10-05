//! The local credential store: four trust layers, resolved per operation.
//!
//! Layer order (highest first), borrowed from DSH
//! `packages/credentials/credentials-local`:
//!
//! 1. **Inherited environment** — read-only, and it *wins*. Nothing can shadow
//!    what the operator exported into the process.
//! 2. **`<user-data-dir>/.credentials.yaml`** — the only writable layer. This
//!    is where an authorization flow commits a first-time value or a rotation.
//! 3. **`<cwd>/.env`** — project-local convenience.
//! 4. **`<user-data-dir>/.env`** — per-user convenience.
//!
//! Three properties are load-bearing:
//!
//! - **Per-operation resolution.** Nothing is cached across operations, so a
//!   rotation takes effect on the *next* request without restarting the
//!   process.
//! - **Empty means absent.** An empty (or whitespace-only) value never counts
//!   as configured, so a blank line cannot masquerade as a working key.
//! - **Owner-only files.** A world/group-readable credentials file is refused
//!   *before its contents are read*, with a `chmod 600` hint. Parse errors
//!   report a location only — never the offending source line, which holds a
//!   secret.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::types::{
    CredentialErrorCode, CredentialInfo, CredentialKey, CredentialRef, CredentialSource,
};

/// File name of the writable credentials layer, under the user data dir.
pub const CREDENTIALS_FILE_NAME: &str = ".credentials.yaml";
/// File name of the dotenv layers.
pub const ENV_FILE_NAME: &str = ".env";

/// Ordered credential lookup over env + on-disk layers.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    /// Per-user data root (`RECURSIVE_HOME`, else `~/.recursive`).
    home: PathBuf,
    /// Directory whose `.env` forms the third layer.
    cwd: PathBuf,
}

/// One trust layer, in the order they are consulted. The environment is
/// special-cased so it is always read fresh (and never cached).
#[derive(Debug)]
enum Layer {
    Env,
    Map(BTreeMap<String, String>, CredentialSource),
}

impl CredentialStore {
    /// Build a store over an explicit user-data dir and working directory.
    pub fn new(home: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            cwd: cwd.into(),
        }
    }

    /// Build a store from the process environment: [`crate::paths::user_data_dir`]
    /// for the per-user layers and the current working directory for `.env`.
    pub fn discover() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(crate::paths::user_data_dir(), cwd)
    }

    /// The writable credentials file (`<user-data-dir>/.credentials.yaml`).
    pub fn credentials_file(&self) -> PathBuf {
        self.home.join(CREDENTIALS_FILE_NAME)
    }

    /// The project-local dotenv (`<cwd>/.env`).
    pub fn cwd_env_file(&self) -> PathBuf {
        self.cwd.join(ENV_FILE_NAME)
    }

    /// The per-user dotenv (`<user-data-dir>/.env`).
    pub fn home_env_file(&self) -> PathBuf {
        self.home.join(ENV_FILE_NAME)
    }

    /// Resolve `reference` now. `Ok(None)` means "not configured anywhere";
    /// empty values are treated as absent, exactly like a missing layer.
    pub fn resolve(&self, reference: &CredentialRef) -> Result<Option<String>> {
        Ok(self.find(reference)?.map(|(value, _)| value))
    }

    /// Resolve `reference` and report which layer produced it.
    pub fn lookup(&self, reference: &CredentialRef) -> Result<Option<(String, CredentialSource)>> {
        self.find(reference)
    }

    /// The value-free view of `reference`, keyed by `key`. Safe to hand to a
    /// frontend: no field can carry a value.
    pub fn info(&self, key: &CredentialKey, reference: &CredentialRef) -> Result<CredentialInfo> {
        let source = self.find(reference)?.map(|(_, source)| source);
        Ok(CredentialInfo::from_source(key.clone(), source))
    }

    /// Write `value` for `reference` into the writable layer, creating the file
    /// owner-only (mode 600) if needed and restoring mode 600 if it exists.
    ///
    /// This is the commit an authorization flow lands; `verify`-style probes
    /// read it back through [`Self::resolve`].
    ///
    /// An existing file that group/other can read is refused first, with the
    /// same `chmod 600` hint as a read: the store never touches an insecure
    /// credential file, so a "helpful" rewrite cannot quietly launder one.
    pub fn set(&self, reference: &CredentialRef, value: &str) -> Result<()> {
        if value.trim().is_empty() {
            return Err(Error::Credential {
                code: CredentialErrorCode::Invalid,
                message: format!(
                    "refusing to store an empty value for {reference}: \
                     an empty value means 'absent', not 'configured'"
                ),
            });
        }
        let path = self.credentials_file();
        let mut map = self.read_credentials_file_map()?.unwrap_or_default();
        map.insert(reference.as_str().to_string(), value.to_string());
        let text = serialize_credentials(&map)?;
        write_owner_only(&path, &text)
    }

    fn find(&self, reference: &CredentialRef) -> Result<Option<(String, CredentialSource)>> {
        for layer in self.load_layers()? {
            match layer {
                Layer::Env => {
                    if let Ok(value) = std::env::var(reference.as_str()) {
                        if !value.trim().is_empty() {
                            return Ok(Some((value, CredentialSource::InheritedEnv)));
                        }
                    }
                }
                Layer::Map(map, source) => {
                    if let Some(value) = map.get(reference.as_str()) {
                        if !value.trim().is_empty() {
                            return Ok(Some((value.clone(), source)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn load_layers(&self) -> Result<Vec<Layer>> {
        let mut layers = vec![Layer::Env];
        if let Some(map) = self.read_credentials_file_map()? {
            layers.push(Layer::Map(map, CredentialSource::CredentialsFile));
        }
        for (path, source) in [
            (self.cwd_env_file(), CredentialSource::CwdEnvFile),
            (self.home_env_file(), CredentialSource::HomeEnvFile),
        ] {
            if let Some(text) = read_optional(&path)? {
                layers.push(Layer::Map(parse_env_file(&text), source));
            }
        }
        Ok(layers)
    }

    /// Read + parse the credentials file, or `None` when it does not exist.
    ///
    /// The owner-only check runs **before** the content is read: a file that
    /// group/other can read is refused outright, not read-and-then-rejected.
    fn read_credentials_file_map(&self) -> Result<Option<BTreeMap<String, String>>> {
        let path = self.credentials_file();
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        };
        if !metadata.is_file() {
            return Ok(None);
        }
        ensure_owner_only(&path)?;
        let text = std::fs::read_to_string(&path).map_err(Error::Io)?;
        Ok(Some(parse_credentials_yaml(&path, &text)?))
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Parse the credentials YAML, reporting **location only** on failure.
///
/// `serde_yaml_ng`'s own `Display` can echo the offending input (type errors
/// quote the value). That input is a secret, so it must never reach a log: the
/// error below deliberately rebuilds the message from the error's line/column.
fn parse_credentials_yaml(path: &Path, text: &str) -> Result<BTreeMap<String, String>> {
    serde_yaml_ng::from_str(text).map_err(|e| {
        let location = e
            .location()
            .map(|l| format!("line {}, column {}", l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        Error::Credential {
            code: CredentialErrorCode::Parse,
            message: format!(
                "credentials file {} could not be parsed ({location}); \
                 the offending line is not shown because it may contain a secret",
                path.display()
            ),
        }
    })
}

fn serialize_credentials(map: &BTreeMap<String, String>) -> Result<String> {
    serde_yaml_ng::to_string(map).map_err(|_| Error::Internal {
        context: "credentials".to_string(),
        // Deliberately opaque: a serializer error can quote the value.
        message: "failed to serialize the credentials file".to_string(),
    })
}

/// Reject files that group/other can read. No-op on non-unix targets, where
/// there are no POSIX mode bits to enforce.
#[cfg(unix)]
fn ensure_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .map_err(Error::Io)?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(Error::Credential {
            code: CredentialErrorCode::InsecureFile,
            message: format!(
                "credentials file {} is readable by group/other (mode {:03o}); \
                 secrets must be owner-only — run `chmod 600 {}`",
                path.display(),
                mode & 0o777,
                path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(Error::Io)?;
    file.write_all(contents.as_bytes()).map_err(Error::Io)?;
    // `mode` only applies at creation — an existing file keeps its old bits,
    // so tighten explicitly as well.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(Error::Io)
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    std::fs::write(path, contents).map_err(Error::Io)
}

/// Minimal dotenv reader: `KEY=VALUE` lines, `#` comments, optional `export `
/// prefix, optional single/double quotes around the value. Unparseable lines
/// are skipped rather than fatal — a `.env` is a convenience layer, and
/// refusing to start because of a stray line would be worse than ignoring it.
fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        map.insert(key.to_string(), strip_quotes(value.trim()).to_string());
    }
    map
}

fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::env_lock;

    fn store(dir: &tempfile::TempDir) -> CredentialStore {
        CredentialStore::new(dir.path().join("home"), dir.path().join("cwd"))
    }

    fn write_credentials(store: &CredentialStore, body: &str) {
        let path = store.credentials_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        set_mode(&path, 0o600);
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_mode(_path: &Path, _mode: u32) {}

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn write_env(store: &CredentialStore, cwd: bool, body: &str) {
        let path = if cwd {
            store.cwd_env_file()
        } else {
            store.home_env_file()
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }

    #[test]
    fn resolves_from_the_writable_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        write_credentials(&store, "RECURSIVE_TEST_CRED_A: sk-from-file\n");
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_A").unwrap();
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-from-file")
        );
        assert_eq!(
            store.lookup(&reference).unwrap().map(|(_, s)| s),
            Some(CredentialSource::CredentialsFile)
        );
    }

    #[test]
    fn missing_layers_resolve_to_absence_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_ABSENT").unwrap();
        assert_eq!(store.resolve(&reference).unwrap(), None);
        assert_eq!(store.lookup(&reference).unwrap(), None);
        let key = CredentialKey::new("test", "absent").unwrap();
        let info = store.info(&key, &reference).unwrap();
        assert!(!info.configured());
        assert!(info.writable());
    }

    #[test]
    fn trust_order_is_env_then_file_then_cwd_then_home() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_CRED_TRUST_ORDER";
        let reference = CredentialRef::parse(name).unwrap();
        std::env::remove_var(name);

        // Lowest wins when alone: home .env.
        write_env(&store, false, &format!("{name}=sk-home\n"));
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-home")
        );

        // cwd .env outranks home .env.
        write_env(&store, true, &format!("{name}=sk-cwd\n"));
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-cwd")
        );

        // credentials file outranks both dotenv layers.
        write_credentials(&store, &format!("{name}: sk-file\n"));
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-file")
        );

        // inherited env outranks everything.
        std::env::set_var(name, "sk-env");
        assert_eq!(
            store.lookup(&reference).unwrap(),
            Some(("sk-env".to_string(), CredentialSource::InheritedEnv))
        );

        // …and once it goes away the file is visible again (no caching of the
        // "wins" outcome).
        std::env::remove_var(name);
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-file")
        );
    }

    #[test]
    fn an_empty_value_is_absent_not_configured() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_CRED_EMPTY";
        let reference = CredentialRef::parse(name).unwrap();
        std::env::remove_var(name);

        write_credentials(&store, &format!("{name}: \"\"\n"));
        write_env(&store, true, &format!("{name}=\n"));
        write_env(&store, false, &format!("{name}=sk-home-fallback\n"));

        // The blank entries must fall through to the non-empty home layer
        // rather than count as configured.
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-home-fallback")
        );

        // With no non-empty layer at all: absent.
        let only_empty = "RECURSIVE_TEST_CRED_ONLY_EMPTY";
        std::env::remove_var(only_empty);
        write_credentials(&store, &format!("{only_empty}: \"\"\n"));
        assert_eq!(
            store
                .resolve(&CredentialRef::parse(only_empty).unwrap())
                .unwrap(),
            None
        );

        // An inherited env var set to the empty string is absence too.
        std::env::set_var("RECURSIVE_TEST_CRED_ENV_EMPTY", "");
        assert_eq!(
            store
                .resolve(&CredentialRef::parse("RECURSIVE_TEST_CRED_ENV_EMPTY").unwrap())
                .unwrap(),
            None
        );
        std::env::remove_var("RECURSIVE_TEST_CRED_ENV_EMPTY");
    }

    #[test]
    fn rotation_is_visible_on_the_next_resolve_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_ROTATE").unwrap();

        write_credentials(&store, "RECURSIVE_TEST_CRED_ROTATE: sk-old\n");
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-old")
        );

        // Same store instance, same process — only the file changed.
        write_credentials(&store, "RECURSIVE_TEST_CRED_ROTATE: sk-new\n");
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-new")
        );
    }

    #[test]
    fn set_commits_a_value_that_resolve_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_SET").unwrap();

        assert!(store.resolve(&reference).unwrap().is_none());
        store.set(&reference, "sk-committed").unwrap();
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-committed")
        );

        // Overwriting preserves any other entries.
        let other = CredentialRef::parse("RECURSIVE_TEST_CRED_SET_OTHER").unwrap();
        store.set(&other, "sk-other").unwrap();
        store.set(&reference, "sk-rotated").unwrap();
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-rotated")
        );
        assert_eq!(store.resolve(&other).unwrap().as_deref(), Some("sk-other"));
    }

    #[cfg(unix)]
    #[test]
    fn set_writes_the_credentials_file_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store
            .set(
                &CredentialRef::parse("RECURSIVE_TEST_CRED_MODE").unwrap(),
                "sk-x",
            )
            .unwrap();
        assert_eq!(mode_of(&store.credentials_file()), 0o600);
    }

    #[test]
    fn set_refuses_to_store_an_empty_value() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_NOEMPTY").unwrap();
        for empty in ["", "  ", "\n"] {
            let err = store.set(&reference, empty).unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::Credential {
                        code: CredentialErrorCode::Invalid,
                        ..
                    }
                ),
                "empty value must be refused, got {err:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_readable_credentials_file_is_refused_with_a_fix_hint() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_INSECURE").unwrap();
        write_credentials(&store, "RECURSIVE_TEST_CRED_INSECURE: sk-exposed\n");
        set_mode(&store.credentials_file(), 0o644);

        let err = store.resolve(&reference).unwrap_err();
        match &err {
            Error::Credential {
                code: CredentialErrorCode::InsecureFile,
                message,
            } => {
                assert!(
                    message.contains("chmod 600"),
                    "the refusal must tell the operator how to fix it: {message}"
                );
                assert!(
                    message.contains(".credentials.yaml"),
                    "the refusal must name the offending file: {message}"
                );
                assert!(
                    !message.contains("sk-exposed"),
                    "the refusal must not leak the value: {message}"
                );
            }
            other => panic!("expected an insecure-file refusal, got {other:?}"),
        }

        // Group-only exposure is refused too (0o077 mask), not just world.
        set_mode(&store.credentials_file(), 0o640);
        assert!(matches!(
            store.resolve(&reference).unwrap_err(),
            Error::Credential {
                code: CredentialErrorCode::InsecureFile,
                ..
            }
        ));

        // The documented fix makes it readable again.
        set_mode(&store.credentials_file(), 0o600);
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-exposed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn set_refuses_to_rewrite_a_group_readable_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_SET_INSECURE").unwrap();
        write_credentials(&store, "RECURSIVE_TEST_CRED_SET_INSECURE: sk-old\n");
        set_mode(&store.credentials_file(), 0o640);

        let err = store.set(&reference, "sk-new").unwrap_err();
        assert!(
            matches!(
                err,
                Error::Credential {
                    code: CredentialErrorCode::InsecureFile,
                    ..
                }
            ),
            "an insecure file must be refused on write too, got {err:?}"
        );

        // The refused write must not have landed.
        set_mode(&store.credentials_file(), 0o600);
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-old")
        );
    }

    #[test]
    fn parse_errors_report_a_location_and_never_echo_the_source_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_PARSE").unwrap();

        // A malformed mapping whose text contains a secret-shaped token.
        write_credentials(&store, "RECURSIVE_TEST_CRED_PARSE: [sk-live-secret\n");
        let err = store.resolve(&reference).unwrap_err();
        match &err {
            Error::Credential {
                code: CredentialErrorCode::Parse,
                message,
            } => {
                assert!(
                    message.contains("line ") && message.contains("column "),
                    "the parse error must carry a line/column location: {message}"
                );
                assert!(
                    !message.contains("sk-live-secret"),
                    "the parse error must not echo the source line: {message}"
                );
            }
            other => panic!("expected a parse error, got {other:?}"),
        }

        // A *type* error is the dangerous one: serde's own message quotes the
        // offending value, so our sanitized message must not.
        write_credentials(&store, "RECURSIVE_TEST_CRED_PARSE:\n  - sk-live-secret\n");
        let err = store.resolve(&reference).unwrap_err();
        assert_eq!(err.credential_code(), Some(CredentialErrorCode::Parse));
        let message = err.to_string();
        assert!(
            !message.contains("sk-live-secret"),
            "a type error must not leak the value either: {message}"
        );
    }

    #[test]
    fn env_parser_handles_quotes_comments_and_export() {
        let map = parse_env_file(
            "# comment\n\
             export QUOTED=\"a b\"\n\
             SINGLE='c d'\n\
             PLAIN=e\n\
             \n\
             NO_EQUALS\n\
             =skipped-key\n",
        );
        assert_eq!(map.get("QUOTED").unwrap(), "a b");
        assert_eq!(map.get("SINGLE").unwrap(), "c d");
        assert_eq!(map.get("PLAIN").unwrap(), "e");
        assert!(!map.contains_key("NO_EQUALS"));
        assert!(!map.contains_key(""));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn info_reports_provenance_without_touching_the_value() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = "RECURSIVE_TEST_CRED_INFO";
        std::env::remove_var(name);
        let reference = CredentialRef::parse(name).unwrap();
        let key = CredentialKey::new("provider", "openai").unwrap();

        assert!(!store.info(&key, &reference).unwrap().configured());

        write_env(&store, true, &format!("{name}=sk-cwd\n"));
        let info = store.info(&key, &reference).unwrap();
        assert!(info.configured());
        assert_eq!(info.source(), Some(CredentialSource::CwdEnvFile));
        assert!(info.writable());
        assert_eq!(info.key(), &key);

        std::env::set_var(name, "sk-env");
        let info = store.info(&key, &reference).unwrap();
        assert_eq!(info.source(), Some(CredentialSource::InheritedEnv));
        assert!(!info.writable(), "an inherited env var shadows the file");
        std::env::remove_var(name);
    }

    #[test]
    fn file_paths_hang_off_the_configured_roots() {
        let store = CredentialStore::new("/data/recursive", "/work/project");
        assert_eq!(
            store.credentials_file(),
            PathBuf::from("/data/recursive/.credentials.yaml")
        );
        assert_eq!(store.cwd_env_file(), PathBuf::from("/work/project/.env"));
        assert_eq!(store.home_env_file(), PathBuf::from("/data/recursive/.env"));
    }

    #[test]
    fn discover_uses_recursive_home_and_the_process_cwd() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::PinnedRecursiveHome::new(home.path());
        let store = CredentialStore::discover();
        assert_eq!(
            store.credentials_file(),
            home.path().join(".credentials.yaml")
        );
        assert_eq!(
            store.cwd_env_file(),
            std::env::current_dir().unwrap().join(".env")
        );
    }
}
