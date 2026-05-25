//! Runtime configuration.
//!
//! All of these can be overridden via env vars or CLI flags. Sensible
//! defaults make the binary runnable with just `RECURSIVE_API_KEY` and
//! `RECURSIVE_MODEL` set.

use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub api_base: String,
    pub api_key: Option<String>,
    pub model: String,
    pub max_steps: usize,
    pub temperature: f64,
    pub system_prompt: String,
}

impl Config {
    /// Load from environment. The API key is optional here so commands that
    /// don't need the LLM (e.g. `tools`, future offline ones) still run.
    pub fn from_env() -> Result<Self> {
        let workspace = std::env::var("RECURSIVE_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let api_base = std::env::var("RECURSIVE_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        let api_key = std::env::var("RECURSIVE_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();

        let model = std::env::var("RECURSIVE_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());

        let max_steps = std::env::var("RECURSIVE_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32);

        let temperature = std::env::var("RECURSIVE_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.2);

        let system_prompt = match std::env::var("RECURSIVE_SYSTEM_PROMPT_FILE") {
            Ok(path) => std::fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("read system prompt {path}: {e}")))?,
            Err(_) => default_system_prompt(),
        };

        Ok(Self {
            workspace,
            api_base,
            api_key,
            model,
            max_steps,
            temperature,
            system_prompt,
        })
    }

    /// Return the API key or a descriptive error if none was configured.
    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .ok_or_else(|| Error::Config("set RECURSIVE_API_KEY (or OPENAI_API_KEY)".into()))
    }
}

pub fn default_system_prompt() -> String {
    "You are Recursive, a minimal but capable coding agent. \
    You have these tools: read_file, write_file, list_dir, run_shell. \
    Work inside the workspace; never reach outside. \
    Plan briefly, then act with tool calls. \
    Before writing code, read the relevant existing files. \
    After changes, run the project's tests via run_shell to verify. \
    Stop calling tools and write a final summary once the task is done."
        .to_string()
}
