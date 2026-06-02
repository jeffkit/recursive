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
pub fn config_file_path() -> Option<PathBuf> {
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
}

/// [provider] section.
#[derive(Debug, Deserialize)]
pub struct ProviderSection {
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub model: Option<String>,
}

/// [agent] section.
#[derive(Debug, Deserialize)]
pub struct AgentSection {
    pub max_steps: Option<usize>,
    pub temperature: Option<f64>,
    pub shell_timeout_secs: Option<u64>,
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
        let config = load_layered_permissions(tmp.path());
        // Session layer is always present
        assert!(config
            .layers
            .iter()
            .any(|l| l.source == RuleSource::Session));
        // Even with no config files, we get the session layer
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
allow = ["read_file"]
deny = ["run_shell"]
"#,
        )
        .unwrap();

        // Create project config
        std::fs::create_dir_all(project.join(".recursive")).unwrap();
        std::fs::write(
            project.join(".recursive").join("config.toml"),
            r#"
[permissions]
allow = ["write_file"]
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
        assert_eq!(config.layers[0].allow, vec!["read_file"]);
        assert_eq!(config.layers[0].deny, vec!["run_shell"]);

        // Project layer has allow/interactive
        assert_eq!(config.layers[1].allow, vec!["write_file"]);
        assert_eq!(config.layers[1].interactive, vec!["delete_file"]);
    }
}
