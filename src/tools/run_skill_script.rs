//! Tool to run a script from a skill's `scripts/` directory.
//!
//! Finds the named script within a named skill, executes it with optional
//! arguments, and returns the combined stdout+stderr output.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::skills::Skill;
use crate::tools::Tool;

/// Tool to run a script from a skill's scripts/ directory.
pub struct RunSkillScript {
    skills: Arc<Vec<Skill>>,
    workspace: PathBuf,
    timeout: Duration,
}

impl RunSkillScript {
    pub fn new(skills: Vec<Skill>, workspace: PathBuf, timeout: Duration) -> Self {
        Self {
            skills: Arc::new(skills),
            workspace,
            timeout,
        }
    }
}

#[async_trait]
impl Tool for RunSkillScript {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_skill_script".into(),
            description: "Run a script from a skill's scripts/ directory by skill and script name. The script executes in the workspace root.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Name of the skill (case-insensitive)"
                    },
                    "script": {
                        "type": "string",
                        "description": "Name of the script within the skill (case-insensitive)"
                    },
                    "args": {
                        "type": "string",
                        "description": "Optional arguments to pass to the script"
                    }
                },
                "required": ["skill", "script"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let skill_name = arguments["skill"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "run_skill_script".into(),
                message: "missing required parameter: skill".to_string(),
            })?;

        let script_name = arguments["script"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "run_skill_script".into(),
                message: "missing required parameter: script".to_string(),
            })?;

        // Find the skill (case-insensitive)
        let skill = self
            .skills
            .iter()
            .find(|s| s.name.to_lowercase() == skill_name.to_lowercase())
            .ok_or_else(|| Error::Tool {
                name: "run_skill_script".into(),
                message: format!("skill not found: {skill_name}"),
            })?;

        // Find the script within the skill (case-insensitive)
        let script = skill
            .scripts
            .iter()
            .find(|s| s.name.to_lowercase() == script_name.to_lowercase())
            .ok_or_else(|| {
                let available: Vec<&str> = skill.scripts.iter().map(|s| s.name.as_str()).collect();
                let available_list = if available.is_empty() {
                    "no scripts available for this skill".to_string()
                } else {
                    format!("available scripts: {}", available.join(", "))
                };
                Error::Tool {
                    name: "run_skill_script".into(),
                    message: format!("script not found: '{script_name}'. {available_list}"),
                }
            })?;

        // Security: script path must be within the skill directory
        let skill_dir = skill.path.parent().ok_or_else(|| Error::Tool {
            name: "run_skill_script".into(),
            message: "cannot determine skill directory".to_string(),
        })?;

        let canonical_script = std::fs::canonicalize(&script.path).map_err(|e| Error::Tool {
            name: "run_skill_script".into(),
            message: format!("cannot resolve script path: {e}"),
        })?;

        let canonical_skill_dir = std::fs::canonicalize(skill_dir).map_err(|e| Error::Tool {
            name: "run_skill_script".into(),
            message: format!("cannot resolve skill directory: {e}"),
        })?;

        if !canonical_script.starts_with(&canonical_skill_dir) {
            return Err(Error::Tool {
                name: "run_skill_script".into(),
                message: "script path escapes skill directory".to_string(),
            });
        }

        // Build the command
        let args_str = arguments["args"].as_str().unwrap_or("");
        let shell_command = if args_str.is_empty() {
            script.path.to_string_lossy().to_string()
        } else {
            format!("{} {}", script.path.display(), args_str)
        };

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&shell_command)
            .current_dir(&self.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| Error::Tool {
            name: "run_skill_script".into(),
            message: format!("spawn failed: {e}"),
        })?;

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let max_output: usize = 10000;
        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max_output).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max_output).await });

        let wait = child.wait();
        let status = match tokio::time::timeout(self.timeout, wait).await {
            Ok(s) => s.map_err(|e| Error::Tool {
                name: "run_skill_script".into(),
                message: format!("wait failed: {e}"),
            })?,
            Err(_) => {
                return Err(Error::Tool {
                    name: "run_skill_script".into(),
                    message: format!("script timed out after {:?}", self.timeout),
                });
            }
        };

        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());

        Ok(format!(
            "exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
        ))
    }
}

async fn read_capped<R: AsyncReadExt + Unpin>(reader: &mut R, max: usize) -> String {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max {
                    let take = max.saturating_sub(buf.len());
                    buf.extend_from_slice(&tmp[..take]);
                    buf.extend_from_slice(b"\n... [output truncated]");
                    let _ = tokio::io::copy(reader, &mut tokio::io::sink()).await;
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillMode, SkillScript};
    use tempfile::TempDir;

    fn make_skill_with_scripts(tmp: &TempDir, scripts: &[(&str, &str, &str)]) -> Vec<Skill> {
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: Test\n---\n\nBody",
        )
        .unwrap();

        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir(&scripts_dir).unwrap();

        let mut skill_scripts = Vec::new();
        for (name, content, desc) in scripts {
            let path = scripts_dir.join(name);
            std::fs::write(&path, content).unwrap();
            // Make executable on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::metadata(&path).unwrap().permissions();
                let mut new_perms = perms;
                new_perms.set_mode(0o755);
                std::fs::set_permissions(&path, new_perms).unwrap();
            }
            skill_scripts.push(SkillScript {
                name: name.to_string(),
                path,
                description: desc.to_string(),
            });
        }

        vec![Skill {
            name: "test-skill".to_string(),
            description: "Test".to_string(),
            path: skill_dir.join("SKILL.md"),
            refs: vec![],
            params: vec![],
            scripts: skill_scripts,
            mode: SkillMode::Manual,
            triggers: vec![],
        }]
    }

    #[tokio::test]
    async fn run_skill_script_executes_and_returns_output() {
        let tmp = TempDir::new().unwrap();
        let skills = make_skill_with_scripts(
            &tmp,
            &[("hello.sh", "#!/bin/sh\necho 'Hello, World!'", "Say hello")],
        );
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        let result = tool
            .execute(json!({"skill": "test-skill", "script": "hello.sh"}))
            .await
            .unwrap();

        assert!(result.contains("exit: 0"));
        assert!(result.contains("Hello, World!"));
    }

    #[tokio::test]
    async fn run_skill_script_errors_on_unknown_skill() {
        let tmp = TempDir::new().unwrap();
        let skills = make_skill_with_scripts(&tmp, &[]);
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        let result = tool
            .execute(json!({"skill": "nonexistent", "script": "hello.sh"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("skill not found"));
    }

    #[tokio::test]
    async fn run_skill_script_errors_on_unknown_script() {
        let tmp = TempDir::new().unwrap();
        let skills = make_skill_with_scripts(&tmp, &[("hello.sh", "#!/bin/sh\necho hi", "Say hi")]);
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        let result = tool
            .execute(json!({"skill": "test-skill", "script": "nonexistent"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("script not found"));
        assert!(err.contains("hello.sh"));
    }

    #[tokio::test]
    async fn run_skill_script_passes_args() {
        let tmp = TempDir::new().unwrap();
        let skills = make_skill_with_scripts(
            &tmp,
            &[("echo.sh", "#!/bin/sh\necho \"args: $*\"", "Echo args")],
        );
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        let result = tool
            .execute(
                json!({"skill": "test-skill", "script": "echo.sh", "args": "--verbose --debug"}),
            )
            .await
            .unwrap();

        assert!(result.contains("exit: 0"));
        assert!(result.contains("args: --verbose --debug"));
    }

    #[tokio::test]
    async fn run_skill_script_respects_timeout() {
        let tmp = TempDir::new().unwrap();
        let skills = make_skill_with_scripts(
            &tmp,
            &[("sleep.sh", "#!/bin/sh\nsleep 999", "Sleep forever")],
        );
        let tool =
            RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_millis(100));

        let result = tool
            .execute(json!({"skill": "test-skill", "script": "sleep.sh"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timed out"));
    }

    #[tokio::test]
    async fn run_skill_script_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let skills =
            make_skill_with_scripts(&tmp, &[("Hello.sh", "#!/bin/sh\necho 'hi'", "Say hi")]);
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        // Different case for both skill and script
        let result = tool
            .execute(json!({"skill": "TEST-SKILL", "script": "HELLO.SH"}))
            .await
            .unwrap();

        assert!(result.contains("exit: 0"));
        assert!(result.contains("hi"));
    }
}
