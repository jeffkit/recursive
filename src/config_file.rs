//! Config file support: ~/.recursive/config.toml
//!
//! Priority chain: CLI flag > env var > config file > hardcoded default.
//! The config file is optional — if absent, we gracefully fall back.

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

impl FileConfig {
    /// Load from the default path (~/.recursive/config.toml).
    /// Returns Ok(None) if the file doesn't exist.
    /// Returns Err if the file exists but is malformed.
    pub fn load() -> anyhow::Result<Option<Self>> {
        let path = match config_file_path() {
            Some(p) => p,
            None => return Ok(None),
        };
        Self::load_from(&path)
    }

    /// Load from an explicit path.
    pub fn load_from(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        let config: FileConfig = toml::from_str(&content)?;
        Ok(Some(config))
    }
}

/// Write a dotted key=value to ~/.recursive/config.toml.
/// Supports dotted keys like "provider.model", "agent.max_steps".
/// Creates the file and parent directory if needed.
pub fn set_value(key: &str, value: &str) -> anyhow::Result<()> {
    let path = config_file_path()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Read existing or start fresh
    let content = if path.exists() {
        std::fs::read_to_string(&path)?
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
        _ => anyhow::bail!("invalid key format: {key}"),
    }

    std::fs::write(&path, toml::to_string_pretty(&doc)?)?;
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
        assert_eq!(
            smart_value("hello"),
            toml::Value::String("hello".into())
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
}
