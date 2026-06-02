//! Tool set provider trait for pluggable, swappable tool implementations.
//!
//! The [`ToolSetProvider`] trait decouples `AgentKernel` from the concrete
//! tool set. The local default builds the standard registry with no sandboxing;
//! cloud deployments can substitute a sandboxed registry without touching the
//! kernel loop.

use crate::tools::ToolRegistry;

/// Controls how aggressively the runtime restricts tool side-effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// No sandbox — tools execute directly in the agent process (local default).
    #[default]
    None,
    /// Policy-based: filesystem path and network access is validated at the
    /// Rust layer before the tool executes.
    Policy,
    /// Container-based: tools execute inside an isolated Docker/gVisor container.
    Container,
    /// MicroVM-based: tools execute inside a hardware-virtualised VM
    /// (e.g. Firecracker or E2B).
    MicroVm,
}

/// Provides the [`ToolRegistry`] for a given runtime mode.
///
/// The local implementation returns the standard registry with direct execution.
/// Cloud implementations wrap tools with sandbox adapters, injecting
/// container/microVM transport layers transparently.
pub trait ToolSetProvider: Send + Sync + 'static {
    /// Build and return the tool registry for this provider.
    fn build_registry(&self) -> ToolRegistry;
    /// Report the sandbox level this provider operates at.
    fn sandbox_mode(&self) -> SandboxMode;
}

/// Standard local tool set with no sandboxing.
///
/// Calls [`crate::tools::build_standard_tools`] with default workspace
/// and shell timeout settings. AgentKernelBuilder overrides these at build
/// time if more context is available.
pub struct LocalToolSetProvider {
    workspace: std::path::PathBuf,
    shell_timeout_secs: u64,
    skills: Vec<crate::skills::Skill>,
}

impl LocalToolSetProvider {
    /// Create a provider for `workspace` with the given shell timeout.
    pub fn new(
        workspace: std::path::PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self {
            workspace,
            shell_timeout_secs,
            skills,
        }
    }
}

impl ToolSetProvider for LocalToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        crate::tools::build_standard_tools(&self.workspace, &self.skills, self.shell_timeout_secs)
    }

    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn provider() -> (LocalToolSetProvider, TempDir) {
        let dir = TempDir::new().unwrap();
        let p = LocalToolSetProvider::new(dir.path().to_path_buf(), 30, vec![]);
        (p, dir)
    }

    #[test]
    fn local_provider_sandbox_mode_is_none() {
        let (p, _dir) = provider();
        assert_eq!(p.sandbox_mode(), SandboxMode::None);
    }

    #[test]
    fn local_provider_registry_non_empty() {
        let (p, _dir) = provider();
        let reg = p.build_registry();
        let names = reg.names();
        assert!(
            names.iter().any(|n| n == "run_shell"),
            "expected run_shell in registry, got: {names:?}"
        );
    }
}
