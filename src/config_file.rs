//! Config file support: ~/.recursive/config.toml
//!
//! Priority chain: CLI flag > env var > config file > hardcoded default.
//! The config file is optional — if absent, we gracefully fall back.

use crate::error::{Error, Result};
use crate::permissions::PermissionMode;
use crate::permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Return the path to the global config file: ~/.recursive/config.toml.
/// Returns None if the home directory cannot be determined.
///
/// Honours `RECURSIVE_HOME` for tests (matching `paths::user_data_dir`)
/// before falling back to `dirs::home_dir()`. Without the
/// `RECURSIVE_HOME` short-circuit, tests that pin HOME via
/// `PinnedHome` still race with tests that mutate `HOME` directly
/// (without holding the env lock) on macOS, where `dirs::home_dir`
/// can re-resolve through `getpwuid_r` mid-test.
pub fn config_file_path() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("RECURSIVE_HOME") {
        return Some(PathBuf::from(custom).join(".recursive").join("config.toml"));
    }
    dirs::home_dir().map(|h| h.join(".recursive").join("config.toml"))
}

/// Top-level deserialized structure of config.toml.
#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    pub provider: Option<ProviderSection>,
    pub agent: Option<AgentSection>,
    /// Optional `[permissions]` section. When present, restricts which
    /// tools the agent may invoke. Schema mirrors
    /// [`crate::permissions::PermissionsConfig`]. See g140.
    pub permissions: Option<PermissionsSection>,
    /// Optional `[sandbox]` section. Expands the filesystem sandbox beyond
    /// the primary workspace so the agent can read (and, if declared,
    /// write) files in additional directories.
    #[serde(default)]
    pub sandbox: Option<SandboxSection>,
}

/// [provider] section.
#[derive(Debug, Deserialize)]
pub struct ProviderSection {
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub model: Option<String>,
    /// Preset id from the bundled `providers.toml` (e.g. `"deepseek"`).
    /// When set, `Config::from_env` uses it as the base for `type` / `api_base` /
    /// `model` / `api_key`; explicit fields above still win. Resolved at load
    /// time — not persisted back via `set_value`.
    #[serde(default)]
    pub preset: Option<String>,
}

/// [agent] section.
#[derive(Debug, Deserialize)]
pub struct AgentSection {
    pub max_steps: Option<usize>,
    pub temperature: Option<f64>,
    pub shell_timeout_secs: Option<u64>,
}

/// `[sandbox]` section. Lists additional directories the agent may reach
/// beyond the primary workspace.
///
/// `extra_dirs` are read-write roots (the agent can `Read`, `Write`, `Edit`
/// inside them). `extra_readonly_dirs` are read-only roots (the agent can
/// `Read` / `Glob` / `Grep` but not `Write` / `Edit`). Paths may be absolute
/// or relative to the current working directory at agent start.
///
/// Example:
/// ```toml
/// [sandbox]
/// extra_dirs = ["/opt/shared-writable"]
/// extra_readonly_dirs = ["/etc/project-ref", "../neighbour-repo"]
/// ```
#[derive(Debug, Default, Deserialize, Clone)]
pub struct SandboxSection {
    #[serde(default)]
    pub extra_dirs: Vec<String>,
    #[serde(default)]
    pub extra_readonly_dirs: Vec<String>,
}

/// [permissions] section. Wire-compatible with
/// [`crate::permissions::PermissionsConfig`] but lives here so config
/// loading does not couple to that crate.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct PermissionsSection {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub interactive: Vec<String>,
    /// Tools that require plan mode before use.
    #[serde(default)]
    pub plan: Vec<String>,
    /// Default permission mode. Accepts both old and new format names.
    /// New: "default", "acceptEdits", "bypassPermissions", "dontAsk", "plan".
    /// Old: "allow" (→ default), "deny" (→ dontAsk), "interactive" (→ dontAsk).
    /// The "plan" variant can also be an object `{prePlanMode, bypassAvailable}`.
    #[serde(default)]
    pub mode: Option<PermissionMode>,
}

impl FileConfig {
    /// Load from the default path (~/.recursive/config.toml).
    /// Returns Ok(None) if the file doesn't exist.
    /// Returns Err if the file exists but is malformed.
    pub fn load() -> Result<Option<Self>> {
        let path = match config_file_path() {
            Some(p) => p,
            None => return Ok(None),
        };
        Self::load_from(&path)
    }

    /// Load from an explicit path.
    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path).map_err(Error::Io)?;
        let config: FileConfig = toml::from_str(&content).map_err(|e| Error::Config {
            message: format!("failed to parse config file {}: {}", path.display(), e),
        })?;
        Ok(Some(config))
    }
}

/// Write a dotted key=value to ~/.recursive/config.toml.
/// Supports dotted keys like "provider.model", "agent.max_steps".
/// Creates the file and parent directory if needed.
pub fn set_value(key: &str, value: &str) -> Result<()> {
    // L1 (Goal 267 follow-up): refuse to persist provider.api_key (or any
    // dotted variant) to disk. The init wizard and `recursive config
    // set-secret` route secrets through [`set_secret`], which writes a
    // 0600 shell-sourceable env file the binary never reads. A key
    // never written to ~/.recursive/config.toml cannot be exfiltrated by
    // an agent with a `run_shell` tool that cats the config file (this
    // is the leak class that produced the .dev/journal key disclosure
    // in run-20260602T090748Z-34743.md).
    if key == "provider.api_key" || key.starts_with("provider.api_key.") {
        return Err(Error::Config {
            message: "refusing to persist provider.api_key to ~/.recursive/config.toml.\n\
                      \n\
                      The binary reads API keys from the process env at runtime,\n\
                      never from the config file. Use one of:\n\
                      \n  \
                      • export DEEPSEEK_API_KEY='<key>'  in your shell rc, or\n  \
                      • `recursive config set-secret <ENV_NAME> <KEY>` to write\n    \
                      a 0600 shell-sourceable file at ~/.recursive/secrets.env\n\
                      \n\
                      (set the env var, or `source ~/.recursive/secrets.env`\n\
                      from your shell rc, before running `recursive`.)"
                .into(),
        });
    }

    let path = config_file_path().ok_or_else(|| Error::Config {
        message: "cannot determine home directory".into(),
    })?;

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }

    // Read existing or start fresh
    let content = if path.exists() {
        std::fs::read_to_string(&path).map_err(Error::Io)?
    } else {
        String::new()
    };

    let mut doc: toml::Table = content.parse::<toml::Table>().unwrap_or_default();

    // Parse dotted key "provider.model" → table["provider"]["model"]
    let parts: Vec<&str> = key.splitn(2, '.').collect();
    match parts.as_slice() {
        [section, field] => {
            let table = doc
                .entry(*section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(t) = table {
                t.insert(field.to_string(), smart_value(value));
            }
        }
        [field] => {
            doc.insert(field.to_string(), smart_value(value));
        }
        _ => {
            return Err(Error::Config {
                message: format!("invalid key format: {key}"),
            })
        }
    }

    let toml_str = toml::to_string_pretty(&doc).map_err(|e| Error::Config {
        message: format!("failed to serialize config: {}", e),
    })?;
    std::fs::write(&path, toml_str).map_err(Error::Io)?;
    Ok(())
}

/// Path of the shell-sourceable secrets file. Mirrors the home-dir
/// resolution in [`config_file_path`] (honours `RECURSIVE_HOME` for tests).
pub fn secrets_env_path() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("RECURSIVE_HOME") {
        return Some(PathBuf::from(custom).join(".recursive").join("secrets.env"));
    }
    dirs::home_dir().map(|h| h.join(".recursive").join("secrets.env"))
}

/// Persist a secret (typically an API key) to a 0600 shell-sourceable
/// env file. The binary does NOT read this file at runtime; the user is
/// expected to `source` it from their shell rc so the secret is in the
/// process env when `recursive` runs.
///
/// This is the L1 half of the .dev/journal key-leak fix: a key that
/// never lands in `~/.recursive/config.toml` cannot be `cat`'d by an
/// agent's `run_shell` tool and end up in a tracked journal.
pub fn set_secret(env_name: &str, value: &str) -> Result<()> {
    let path = secrets_env_path().ok_or_else(|| Error::Config {
        message: "cannot determine home directory".into(),
    })?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }

    // Read existing lines (preserve any other env vars the user previously
    // set via the same file). Idempotent: re-running with the same
    // (env_name, value) yields the same file.
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("export {env_name}=");
    let new_line = format!("{prefix}'{}'", shell_single_quote(value));

    let mut lines: Vec<String> = existing.lines().map(String::from).collect();
    let mut found = false;
    for line in lines.iter_mut() {
        if line.trim_start().starts_with(&prefix) {
            *line = new_line.clone();
            found = true;
            break;
        }
    }
    if !found {
        if !existing.is_empty() && !existing.ends_with('\n') {
            // Existing file has no trailing newline; add one before appending.
            lines.push(String::new());
        }
        lines.push(new_line);
    }

    let mut content = String::new();
    if existing.is_empty() {
        content.push_str(
            "# Generated by `recursive init` / `recursive config set-secret`.\n\
             # Source this from your shell rc to load API keys into the env:\n\
             #   source ~/.recursive/secrets.env\n\
             # File mode is 0600 (owner read/write only).\n",
        );
    }
    content.push_str(&lines.join("\n"));
    content.push('\n');

    std::fs::write(&path, content).map_err(Error::Io)?;

    // Restrict permissions to owner. On Windows the chmod is a no-op, but
    // the file is still per-user in the user's profile directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms).map_err(Error::Io)?;
    }

    Ok(())
}

/// Shell-escape a value for inclusion in a single-quoted `export FOO='…'`
/// line. Replaces `'` with the standard `'\''` close-quote / escape /
/// reopen-quote sequence.
fn shell_single_quote(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Convert a string to the appropriate TOML value type.
fn smart_value(s: &str) -> toml::Value {
    if let Ok(i) = s.parse::<i64>() {
        toml::Value::Integer(i)
    } else if let Ok(f) = s.parse::<f64>() {
        toml::Value::Float(f)
    } else if s == "true" || s == "false" {
        toml::Value::Boolean(s == "true")
    } else {
        toml::Value::String(s.to_string())
    }
}

/// Load layered permissions from user config and project config.
///
/// Resolution order (highest priority first):
/// 1. Session layer (empty, filled at runtime via Goal 196)
/// 2. Project layer (`.recursive/config.toml` in the workspace)
/// 3. User layer (`~/.recursive/config.toml`)
///
/// The Session layer is always present (even if empty) so that runtime
/// rule updates can be written to it.
pub fn load_layered_permissions(workspace: &Path) -> LayeredPermissionsConfig {
    let mut config = LayeredPermissionsConfig::default();

    // User layer (lowest priority)
    if let Some(home) = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
    {
        let path = home.join(".recursive").join("config.toml");
        if let Some(layer) = load_permission_layer(&path, RuleSource::User) {
            config.layers.push(layer);
        }
    }

    // Project layer (medium priority)
    let project_path = workspace.join(".recursive").join("config.toml");
    if let Some(layer) = load_permission_layer(&project_path, RuleSource::Project) {
        config.layers.push(layer);
    }

    // Session layer (highest priority) — always present, empty by default
    config.layers.push(PermissionLayer {
        source: RuleSource::Session,
        ..Default::default()
    });

    config
}

/// Load a single permission layer from a TOML file, if it exists.
///
/// Returns `None` if the file doesn't exist or can't be parsed.
fn load_permission_layer(path: &Path, source: RuleSource) -> Option<PermissionLayer> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    // Parse as FileConfig first so we only extract the [permissions] section.
    // This avoids picking up unrelated sections (e.g. [provider]) as empty defaults.
    let file_config: FileConfig = toml::from_str(&content).ok()?;
    let section = file_config.permissions?;
    Some(PermissionLayer {
        source,
        allow: section.allow,
        deny: section.deny,
        interactive: section.interactive,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_none_when_missing() {
        let result = FileConfig::load_from(Path::new("/nonexistent/path.toml")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_parses_valid_toml() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"
[provider]
type = "openai"
api_key = "sk-test"
api_base = "https://api.deepseek.com"
model = "deepseek-chat"

[agent]
max_steps = 64
temperature = 0.5
"#,
        )
        .unwrap();

        let config = FileConfig::load_from(tmp.path()).unwrap();
        assert!(config.is_some());
        let c = config.unwrap();
        let p = c.provider.unwrap();
        assert_eq!(p.provider_type.as_deref(), Some("openai"));
        assert_eq!(p.api_key.as_deref(), Some("sk-test"));
        assert_eq!(p.api_base.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(p.model.as_deref(), Some("deepseek-chat"));
        let a = c.agent.unwrap();
        assert_eq!(a.max_steps, Some(64));
        assert_eq!(a.temperature, Some(0.5));
    }

    #[test]
    fn load_errors_on_malformed() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "this is [[[not valid toml").unwrap();
        let result = FileConfig::load_from(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn smart_value_parses_types() {
        assert_eq!(smart_value("42"), toml::Value::Integer(42));
        assert_eq!(smart_value("0.5"), toml::Value::Float(0.5));
        assert_eq!(smart_value("true"), toml::Value::Boolean(true));
        assert_eq!(smart_value("hello"), toml::Value::String("hello".into()));
    }

    #[test]
    fn parse_provider_section_with_preset() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"
[provider]
preset = "deepseek"
"#,
        )
        .unwrap();

        let config = FileConfig::load_from(tmp.path()).unwrap().unwrap();
        let p = config.provider.unwrap();
        assert_eq!(p.preset.as_deref(), Some("deepseek"));
        // Other fields are absent — preset resolution happens in Config::from_env.
        assert!(p.provider_type.is_none());
        assert!(p.api_base.is_none());
        assert!(p.model.is_none());
        assert!(p.api_key.is_none());
    }

    #[test]
    fn set_value_preset_round_trips() {
        // The dotted-key writer must handle the new `provider.preset` field.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(&path, "[provider]\nmodel = \"x\"\n").unwrap();

        // Manually invoke the same dotted-key logic the public set_value uses,
        // since set_value writes to ~/.recursive/config.toml via HOME.
        let content = std::fs::read_to_string(&path).unwrap();
        let mut doc: toml::Table = content.parse().unwrap();
        let parts: Vec<&str> = "provider.preset".splitn(2, '.').collect();
        if let [section, field] = parts.as_slice() {
            let table = doc
                .entry(*section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(t) = table {
                t.insert(field.to_string(), smart_value("anthropic"));
            }
        }
        std::fs::write(&path, toml::to_string_pretty(&doc).unwrap()).unwrap();

        let loaded = FileConfig::load_from(&path).unwrap().unwrap();
        assert_eq!(
            loaded.provider.unwrap().preset.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn set_value_creates_file_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        // Temporarily override the path resolution by writing directly
        std::fs::create_dir_all(tmp.path()).unwrap();
        // We'll test the write logic manually since config_file_path() uses HOME
        let content = String::new();
        let mut doc: toml::Table = content.parse::<toml::Table>().unwrap_or_default();

        let parts: Vec<&str> = "provider.model".splitn(2, '.').collect();
        if let [section, field] = parts.as_slice() {
            let table = doc
                .entry(*section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(t) = table {
                t.insert(field.to_string(), smart_value("deepseek-chat"));
            }
        }

        let output = toml::to_string_pretty(&doc).unwrap();
        std::fs::write(&path, &output).unwrap();

        // Verify
        let loaded = FileConfig::load_from(&path).unwrap().unwrap();
        assert_eq!(
            loaded.provider.unwrap().model.as_deref(),
            Some("deepseek-chat")
        );
    }

    #[test]
    fn test_load_layered_permissions_session_layer_always_present() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let fake_home = tmp.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();

        // Pin HOME (under env_lock) so a concurrent test can't interleave
        // its own HOME mutation and have its User layer leak into the
        // "exactly one Session layer" assertion.
        let _pin = crate::test_util::PinnedHome::new(&fake_home);
        let config = load_layered_permissions(&workspace);

        // Session layer is always present
        assert!(config
            .layers
            .iter()
            .any(|l| l.source == RuleSource::Session));
        // Even with no config files, we get only the session layer
        assert_eq!(config.layers.len(), 1);
        assert_eq!(config.layers[0].source, RuleSource::Session);
    }

    #[test]
    fn test_load_layered_permissions_loads_user_and_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");

        // Create user config
        std::fs::create_dir_all(home.join(".recursive")).unwrap();
        std::fs::write(
            home.join(".recursive").join("config.toml"),
            r#"
[permissions]
allow = ["Read"]
deny = ["Bash"]
"#,
        )
        .unwrap();

        // Create project config
        std::fs::create_dir_all(project.join(".recursive")).unwrap();
        std::fs::write(
            project.join(".recursive").join("config.toml"),
            r#"
[permissions]
allow = ["Write"]
interactive = ["delete_file"]
"#,
        )
        .unwrap();

        // Override home dir for the test
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        let config = load_layered_permissions(&project);

        // Restore home
        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        // Should have 3 layers: User, Project, Session
        assert_eq!(config.layers.len(), 3);
        assert_eq!(config.layers[0].source, RuleSource::User);
        assert_eq!(config.layers[1].source, RuleSource::Project);
        assert_eq!(config.layers[2].source, RuleSource::Session);

        // User layer has allow/deny
        assert_eq!(config.layers[0].allow, vec!["Read"]);
        assert_eq!(config.layers[0].deny, vec!["Bash"]);

        // Project layer has allow/interactive
        assert_eq!(config.layers[1].allow, vec!["Write"]);
        assert_eq!(config.layers[1].interactive, vec!["delete_file"]);
    }

    // ---- L1: provider.api_key must NEVER land in ~/.recursive/config.toml ----
    //
    // See set_value() and set_secret() doc comments for the threat model.
    // The agent's `run_shell` tool can `cat` any file the binary can read,
    // so a key on disk in config.toml can be exfiltrated into a tracked
    // journal. The fix is to refuse the write entirely and route the
    // secret through set_secret() to a 0600 file the binary never reads.

    #[test]
    fn set_value_refuses_provider_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        let err = set_value(
            "provider.api_key",
            "sk-fixture-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect_err("set_value must refuse provider.api_key");
        let msg = format!("{err}");
        assert!(
            msg.contains("refusing to persist"),
            "error should mention refusal; got: {msg}"
        );
        assert!(
            msg.contains("DEEPSEEK_API_KEY") || msg.contains("set-secret"),
            "error should steer the user to env var or set-secret; got: {msg}"
        );

        // Belt: the config file must not exist on disk at all.
        let path = config_file_path().unwrap();
        assert!(
            !path.exists(),
            "config.toml must not be created when refusing the write (path={})",
            path.display()
        );
    }

    #[test]
    fn set_value_refuses_provider_api_key_dotted_subkey() {
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        let err = set_value("provider.api_key.something", "sk-abc")
            .expect_err("set_value must refuse dotted variants of provider.api_key");
        let msg = format!("{err}");
        assert!(msg.contains("refusing to persist"), "got: {msg}");
    }

    #[test]
    fn set_value_allows_non_secret_provider_keys() {
        // Regression guard: only provider.api_key is refused, not
        // provider.model, provider.api_base, provider.preset, etc.
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        set_value("provider.model", "deepseek-chat").expect("model must write");
        set_value("provider.api_base", "https://api.deepseek.com").expect("api_base must write");
        set_value("provider.preset", "deepseek").expect("preset must write");

        let path = config_file_path().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("deepseek-chat"));
        assert!(content.contains("https://api.deepseek.com"));
        assert!(content.contains("deepseek"));
        // Belt: no api_key line.
        assert!(
            !content.contains("api_key"),
            "no api_key line should be present, got:\n{content}"
        );
    }

    #[test]
    fn set_secret_writes_to_secrets_env_with_0600_perms() {
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        set_secret(
            "DEEPSEEK_API_KEY",
            "sk-fixture-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("set_secret should succeed");

        let path = secrets_env_path().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("export DEEPSEEK_API_KEY='sk-fixture-aaaaaaaaaaaaaaaaaaaaaaaaaaaa'"),
            "expected shell-sourceable line, got:\n{content}"
        );

        // 0600 on unix; on Windows, the chmod is a no-op and the file is
        // already in the per-user profile directory, so the test only
        // enforces the perms on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0600 perms, got {mode:o}");
        }

        // Belt: ~/.recursive/config.toml must NOT have been created.
        let cfg_path = config_file_path().unwrap();
        assert!(
            !cfg_path.exists(),
            "config.toml must not be created as a side effect of set_secret"
        );
    }

    #[test]
    fn set_secret_updates_existing_line_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        set_secret(
            "DEEPSEEK_API_KEY",
            "sk-old-key-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        set_secret(
            "DEEPSEEK_API_KEY",
            "sk-new-key-bbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();

        let content = std::fs::read_to_string(secrets_env_path().unwrap()).unwrap();
        assert!(
            !content.contains("sk-old-key"),
            "old value must be replaced, not duplicated; got:\n{content}"
        );
        assert!(
            content.contains("sk-new-key"),
            "new value must be present; got:\n{content}"
        );
        // Exactly one DEEPSEEK_API_KEY line.
        let count = content
            .lines()
            .filter(|l| l.contains("DEEPSEEK_API_KEY"))
            .count();
        assert_eq!(count, 1, "expected exactly one DEEPSEEK_API_KEY line");
    }

    #[test]
    fn set_secret_preserves_other_env_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        set_secret(
            "DEEPSEEK_API_KEY",
            "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        set_secret("OPENAI_API_KEY", "sk-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();

        let content = std::fs::read_to_string(secrets_env_path().unwrap()).unwrap();
        assert!(content.contains("DEEPSEEK_API_KEY="));
        assert!(content.contains("OPENAI_API_KEY="));
    }

    #[test]
    fn set_secret_escapes_single_quotes() {
        // A pathological key value containing a single quote must not
        // break out of the single-quoted shell context. The fix is
        // standard `'\''` escaping.
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());

        let weird = "sk-foo'; rm -rf ~; 'bar";
        set_secret("DEEPSEEK_API_KEY", weird).unwrap();
        let content = std::fs::read_to_string(secrets_env_path().unwrap()).unwrap();
        // The exported value, when re-evaluated by sh, must equal the
        // original `weird` string. We don't have sh here, but we can
        // assert the escaping is present and the file is syntactically
        // a single export line.
        assert!(
            content.contains("'\\''"),
            "single quote should be escaped as '\\''; got:\n{content}"
        );
    }
}
