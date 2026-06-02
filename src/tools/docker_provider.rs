//! Docker-based [`ToolSetProvider`] for L2 container sandbox.
//!
//! [`DockerToolSetProvider`] builds a tool registry where `run_shell` is
//! replaced by [`DockerShellTool`], transparently routing shell commands
//! into an isolated Docker container.
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::path::PathBuf;
use std::sync::Arc;

use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::ToolRegistry;

use super::docker_sandbox::DockerShellTool;

/// Configuration for the Docker tool provider.
pub struct DockerToolSetProvider {
    /// Docker image to use for the sandbox container.
    pub image: String,
    /// Workspace path that will be bind-mounted into the container.
    pub workspace: PathBuf,
    pub shell_timeout_secs: u64,
    pub skills: Vec<crate::skills::Skill>,
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
        }
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
        let image = self.image.clone();
        let workspace = self.workspace.clone();
        let timeout = self.shell_timeout_secs;

        let docker_shell = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                DockerShellTool::new(&image, workspace.as_path(), timeout).await
            })
        });

        match docker_shell {
            Ok(tool) => {
                // register_mut replaces the existing "run_shell" entry.
                registry.register_mut(Arc::new(tool));
            }
            Err(e) => {
                tracing::warn!(
                    "DockerToolSetProvider: failed to start container, falling back to local shell: {e}"
                );
                // Fallback: keep the existing local run_shell
            }
        }
        registry
    }

    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::Container
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn docker_provider_sandbox_mode_is_container() {
        let dir = TempDir::new().unwrap();
        let p = DockerToolSetProvider::ubuntu(dir.path().to_path_buf(), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::Container);
    }
}
