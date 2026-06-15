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

    fn is_deferred(&self) -> bool {
        true
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

        // Parse args with shell-words: each arg is passed as a discrete argv
        // element — no shell injection, no glob expansion, no command
        // substitution.  The script's own shebang determines the interpreter.
        let args_raw = arguments["args"].as_str().unwrap_or("");
        let args_vec: Vec<String> = if args_raw.is_empty() {
            Vec::new()
        } else {
            shell_words::split(args_raw).map_err(|e| Error::Tool {
                name: "run_skill_script".into(),
                message: format!("failed to parse args: {e}"),
            })?
        };

        let mut cmd = Command::new(&script.path);
        cmd.args(&args_vec)
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
#[cfg(not(target_os = "windows"))]
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
            hint: String::new(),
            depends_on: vec![],
            sections: vec![],
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

    // ── Goal 283: shell-words arg safety ──────────────────────────────

    /// Shell metacharacters in args are passed as discrete argv elements
    /// — no shell injection.  The test script prints each argv element on
    /// its own line.  `shell_words::split` strips shell quoting, so the
    /// inner text `; rm -rf /; echo pwned` (without quotes) is the single
    /// arg the script receives.  We assert the sentinel file was NOT created.
    #[tokio::test]
    async fn args_with_shell_metachars_are_passed_verbatim() {
        let tmp = TempDir::new().unwrap();
        // Sentinel file to prove no command execution happened
        let sentinel = tmp.path().join("PWNED");
        let skills = make_skill_with_scripts(
            &tmp,
            &[(
                "argv.sh",
                "#!/bin/sh\nprintf '%s\\n' \"$@\"",
                "Print each arg",
            )],
        );
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        // Malicious args that would trigger command execution under sh -c.
        // shell_words strips the outer quotes; the script receives the
        // literal text `; rm -rf /; echo pwned` as a single argv element.
        let result = tool
            .execute(json!({
                "skill": "test-skill",
                "script": "argv.sh",
                "args": "\"; rm -rf /; echo pwned\""
            }))
            .await
            .unwrap();

        assert!(result.contains("exit: 0"));
        // The literal inner text (quotes stripped by shell_words) must
        // appear verbatim — the `;` was NOT interpreted as a shell separator.
        assert!(
            result.contains("; rm -rf /; echo pwned"),
            "malicious args should appear verbatim, not be executed. Got: {result}"
        );
        // Sentinel must NOT exist — no shell execution happened
        assert!(
            !sentinel.exists(),
            "sentinel file was created — shell injection succeeded!"
        );
    }

    /// Command substitution syntax in args is passed as literal text —
    /// no `$(...)` expansion occurs because we exec the script directly
    /// (no sh -c wrapper).  `shell_words::split` treats the unquoted
    /// space inside `$(...)` as a word boundary, so the script receives
    /// two args: `$(touch` and `/tmp/pwned_by_subshell)`.
    #[tokio::test]
    async fn args_with_command_substitution_are_passed_verbatim() {
        let tmp = TempDir::new().unwrap();
        let sentinel = tmp.path().join("pwned_by_subshell");
        let skills = make_skill_with_scripts(
            &tmp,
            &[(
                "argv.sh",
                "#!/bin/sh\nprintf '%s\\n' \"$@\"",
                "Print each arg",
            )],
        );
        let tool = RunSkillScript::new(skills, tmp.path().to_path_buf(), Duration::from_secs(30));

        let result = tool
            .execute(json!({
                "skill": "test-skill",
                "script": "argv.sh",
                "args": "$(touch /tmp/pwned_by_subshell)"
            }))
            .await
            .unwrap();

        assert!(result.contains("exit: 0"));
        // The text appears but split as separate argv elements —
        // the `$(...)` was NOT expanded.
        assert!(
            result.contains("$(touch"),
            "command substitution fragment should appear verbatim. Got: {result}"
        );
        assert!(
            result.contains("/tmp/pwned_by_subshell)"),
            "command substitution fragment should appear verbatim. Got: {result}"
        );
        assert!(
            !sentinel.exists(),
            "sentinel file was created — command substitution executed!"
        );
    }

    /// When a deny rule for "run_skill_script" is configured, calling
    /// through the ToolRegistry (and thus the PermissionPipeline) must
    /// return Error::PermissionDenied.
    #[tokio::test]
    async fn run_skill_script_respects_permission_pipeline() {
        let tmp = TempDir::new().unwrap();
        let skills = make_skill_with_scripts(&tmp, &[("hello.sh", "#!/bin/sh\necho hi", "Say hi")]);

        let config = crate::permissions::LayeredPermissionsConfig {
            mode: crate::permissions::PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                deny: vec!["run_skill_script".into()],
                ..Default::default()
            }],
        };

        let reg = crate::tools::ToolRegistry::local()
            .with_permissions(config)
            .register(Arc::new(RunSkillScript::new(
                skills,
                tmp.path().to_path_buf(),
                Duration::from_secs(30),
            )));

        let dispatch = reg
            .invoke_with_audit(
                "run_skill_script",
                json!({"skill": "test-skill", "script": "hello.sh"}),
            )
            .await;

        assert!(
            matches!(dispatch.result, Err(Error::PermissionDenied { .. })),
            "run_skill_script with deny rule should be PermissionDenied, got: {:?}",
            dispatch.result
        );
    }

    /// When the script file lacks execute permission, the OS refuses to
    /// exec it and the tool returns Error::Tool with a "spawn failed"
    /// message (not a silent success or crash).
    #[tokio::test]
    async fn run_skill_script_fails_on_non_executable_script() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("bad-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: bad-skill\ndescription: Bad\n---\n\nBody",
        )
        .unwrap();

        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir(&scripts_dir).unwrap();
        let script_path = scripts_dir.join("noexec.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho 'should not run'").unwrap();

        // Explicitly remove execute permission — mode 0o644
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&script_path).unwrap().permissions();
            let mut new_perms = perms;
            new_perms.set_mode(0o644);
            std::fs::set_permissions(&script_path, new_perms).unwrap();
        }

        let skill = Skill {
            name: "bad-skill".to_string(),
            description: "Bad".to_string(),
            path: skill_dir.join("SKILL.md"),
            refs: vec![],
            params: vec![],
            scripts: vec![SkillScript {
                name: "noexec.sh".to_string(),
                path: script_path,
                description: "not executable".to_string(),
            }],
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            sections: vec![],
        };

        let tool = RunSkillScript::new(
            vec![skill],
            tmp.path().to_path_buf(),
            Duration::from_secs(30),
        );

        let result = tool
            .execute(json!({"skill": "bad-skill", "script": "noexec.sh"}))
            .await;

        assert!(result.is_err(), "non-executable script should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("spawn failed") || err.contains("PermissionDenied"),
            "should get spawn-failed or permission-denied error, got: {err}"
        );
    }
}
