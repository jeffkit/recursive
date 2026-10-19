//! Docker-based [`ToolSetProvider`] for L2 container sandbox.
//!
//! [`DockerToolSetProvider`] builds a tool registry where `run_shell` is
//! replaced by [`DockerShellTool`], transparently routing shell commands
//! into an isolated Docker container.
//!
//! # Degradation contract (issue #167)
//!
//! Container start can fail (no daemon, missing image). This provider keeps
//! the host Bash so a session still runs, but the downgrade is **never
//! silent**: it emits a structured `WARN` (`expected_tier` / `actual_tier` /
//! `reason`) and [`ToolSetProvider::sandbox_mode`] afterwards reports the
//! real tier ([`SandboxMode::None`]) instead of continuing to claim
//! [`SandboxMode::Container`]. A provider reporting `Container` is
//! guaranteed to execute Bash through [`DockerShellTool`].
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::{Tool, ToolRegistry};

use super::docker_sandbox::DockerShellTool;

/// Boxed future returned by [`DockerShellStarter::start`].
pub type StartFuture = Pin<Box<dyn Future<Output = Result<Arc<dyn Tool>, String>> + Send>>;

/// Starts the container-backed Bash replacement.
///
/// Production uses [`DockerShellTool`]; tests inject a failing or fake
/// starter to exercise the degradation contract without a Docker daemon.
pub trait DockerShellStarter: Send + Sync {
    fn start(&self, image: String, workspace: PathBuf, timeout_secs: u64) -> StartFuture;
}

/// Default starter: spin up a [`DockerShellTool`] for the given image.
struct DockerShellToolStarter;

impl DockerShellStarter for DockerShellToolStarter {
    fn start(&self, image: String, workspace: PathBuf, timeout_secs: u64) -> StartFuture {
        Box::pin(async move {
            let tool = DockerShellTool::new(&image, workspace.as_path(), timeout_secs)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Arc::new(tool) as Arc<dyn Tool>)
        })
    }
}

/// Configuration for the Docker tool provider.
pub struct DockerToolSetProvider {
    /// Docker image to use for the sandbox container.
    pub image: String,
    /// Workspace path that will be bind-mounted into the container.
    pub workspace: PathBuf,
    pub shell_timeout_secs: u64,
    pub skills: Vec<crate::skills::Skill>,
    starter: Arc<dyn DockerShellStarter>,
    /// Tier the most recent [`ToolSetProvider::build_registry`] actually
    /// produced. Starts at the declared tier; a failed container start
    /// downgrades it to [`SandboxMode::None`].
    actual_mode: RwLock<SandboxMode>,
}

impl DockerToolSetProvider {
    pub fn new(
        image: impl Into<String>,
        workspace: PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self {
            image: image.into(),
            workspace,
            shell_timeout_secs,
            skills,
            starter: Arc::new(DockerShellToolStarter),
            actual_mode: RwLock::new(SandboxMode::Container),
        }
    }

    /// Override the container start hook. Production uses the default
    /// ([`DockerShellTool`]); tests inject a fake or failing starter.
    pub fn with_starter(mut self, starter: Arc<dyn DockerShellStarter>) -> Self {
        self.starter = starter;
        self
    }

    /// Use the default `ubuntu:22.04` image.
    pub fn ubuntu(
        workspace: PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self::new("ubuntu:22.04", workspace, shell_timeout_secs, skills)
    }
}

impl ToolSetProvider for DockerToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        // Build the standard registry (which includes a local run_shell).
        let mut registry = crate::tools::build_standard_tools(
            &self.workspace,
            &self.skills,
            self.shell_timeout_secs,
        );

        // Create a blocking runtime-local future to spin up the container.
        // Note: `build_registry` is sync; we use `tokio::task::block_in_place`
        // when called from async context, or fall back to a new runtime.
        let started = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.starter.start(
                self.image.clone(),
                self.workspace.clone(),
                self.shell_timeout_secs,
            ))
        });

        match started {
            Ok(tool) => {
                // register_mut replaces the existing "Bash" entry.
                registry.register_mut(tool);
                self.record_actual_mode(SandboxMode::Container);
            }
            Err(reason) => {
                // Keep the local Bash, but never let the downgrade be
                // silent (owner ruling: any degradation must be
                // observable) — and never keep claiming Container.
                self.record_actual_mode(SandboxMode::None);
                tracing::warn!(
                    target: "recursive::sandbox",
                    expected_tier = "container",
                    actual_tier = "none",
                    reason = %reason,
                    "sandbox tier degraded: container sandbox unavailable; \
                     Bash executes on the host"
                );
            }
        }
        registry
    }

    fn sandbox_mode(&self) -> SandboxMode {
        *self
            .actual_mode
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DockerToolSetProvider {
    /// Record the tier the registry actually got, so [`sandbox_mode`] can
    /// never keep advertising `Container` after a host fallback.
    ///
    /// [`sandbox_mode`]: ToolSetProvider::sandbox_mode
    fn record_actual_mode(&self, mode: SandboxMode) {
        *self
            .actual_mode
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use tempfile::TempDir;

    use crate::llm::ToolSpec;

    /// Starter that fails the way a missing Docker daemon does.
    struct FailingStarter;

    impl DockerShellStarter for FailingStarter {
        fn start(&self, _image: String, _workspace: PathBuf, _timeout_secs: u64) -> StartFuture {
            Box::pin(async { Err("docker daemon unreachable (mock)".to_string()) })
        }
    }

    /// Host-side fake standing in for `DockerShellTool`.
    struct FakeContainerBash;

    #[async_trait]
    impl Tool for FakeContainerBash {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "Bash".into(),
                description: "mock container shell".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, _arguments: Value) -> crate::error::Result<String> {
            Ok("container-ok".into())
        }
    }

    struct SucceedingStarter;

    impl DockerShellStarter for SucceedingStarter {
        fn start(&self, _image: String, _workspace: PathBuf, _timeout_secs: u64) -> StartFuture {
            Box::pin(async { Ok(Arc::new(FakeContainerBash) as Arc<dyn Tool>) })
        }
    }

    #[test]
    fn docker_provider_sandbox_mode_is_container() {
        let dir = TempDir::new().unwrap();
        let p = DockerToolSetProvider::ubuntu(dir.path().to_path_buf(), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::Container);
    }

    /// Regression for issue #167: a failed container start must (a) emit a
    /// structured WARNING (expected/actual tier + reason) and (b) report the
    /// real host tier instead of still claiming `Container`.
    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn container_start_failure_warns_and_reports_actual_tier() {
        let dir = TempDir::new().unwrap();
        let provider = DockerToolSetProvider::ubuntu(dir.path().to_path_buf(), 30, vec![])
            .with_starter(Arc::new(FailingStarter));

        let registry = provider.build_registry();

        // The fallback keeps the local Bash so the session still works...
        assert!(
            registry.names().iter().any(|n| n == "Bash"),
            "host fallback must still register Bash"
        );
        // ...but the provider must stop claiming the container tier.
        assert_eq!(
            provider.sandbox_mode(),
            SandboxMode::None,
            "a degraded provider must report the real (host) tier, not Container"
        );

        logs_assert(|lines: &[&str]| {
            let line = lines
                .iter()
                .find(|l| l.contains("sandbox tier degraded"))
                .ok_or_else(|| "expected a WARNING for the degraded sandbox tier".to_string())?;
            if !line.contains("WARN") {
                return Err(format!("degradation must be logged at WARN, got: {line}"));
            }
            for field in [
                "expected_tier=\"container\"",
                "actual_tier=\"none\"",
                "reason=docker daemon unreachable (mock)",
            ] {
                if !line.contains(field) {
                    return Err(format!("missing `{field}` in: {line}"));
                }
            }
            Ok(())
        });
    }

    /// The success path must keep reporting `Container` and actually route
    /// Bash through the started container tool.
    #[tokio::test(flavor = "multi_thread")]
    async fn container_start_success_reports_container_and_uses_container_bash() {
        let dir = TempDir::new().unwrap();
        let provider = DockerToolSetProvider::ubuntu(dir.path().to_path_buf(), 30, vec![])
            .with_starter(Arc::new(SucceedingStarter));

        let registry = provider.build_registry();

        assert_eq!(provider.sandbox_mode(), SandboxMode::Container);
        let bash = registry.get("Bash").expect("Bash must be registered");
        let out = bash
            .execute(json!({"command": "echo hi"}))
            .await
            .expect("mock container Bash must run");
        assert_eq!(out, "container-ok");
    }
}
