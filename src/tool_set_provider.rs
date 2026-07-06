//! Tool set provider trait for pluggable, swappable tool implementations.
//!
//! Implementations:
//! - [`LocalToolSetProvider`]: standard registry, no sandboxing
//! - [`PolicyToolSetProvider`]: standard registry + L1 policy checks
//!
//! The [`ToolSetProvider`] trait decouples `AgentKernel` from the concrete
//! tool set. The local default builds the standard registry with no sandboxing;
//! cloud deployments can substitute a sandboxed registry without touching the
//! kernel loop.

use crate::tools::{policy_sandbox::PolicyConfig, ToolRegistry};

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

/// [`ToolSetProvider`] that wraps the default registry with an L1 policy config.
///
/// The policy is stored in the registry for tools to query at call time via
/// `registry.policy()`. It does **not** automatically enforce anything by itself;
/// individual tools (e.g. `run_shell`) are responsible for calling
/// `registry.policy().check_shell(...)` before executing.
pub struct PolicyToolSetProvider {
    workspace: std::path::PathBuf,
    shell_timeout_secs: u64,
    skills: Vec<crate::skills::Skill>,
    /// The L1 policy to attach to the built registry.
    pub policy: PolicyConfig,
}

impl PolicyToolSetProvider {
    pub fn new(
        workspace: std::path::PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
        policy: PolicyConfig,
    ) -> Self {
        Self {
            workspace,
            shell_timeout_secs,
            skills,
            policy,
        }
    }

    /// Create with [`PolicyConfig::default_restrictive`] policy.
    pub fn restrictive(
        workspace: std::path::PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self::new(
            workspace,
            shell_timeout_secs,
            skills,
            PolicyConfig::default_restrictive(),
        )
    }
}

impl ToolSetProvider for PolicyToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        crate::tools::build_standard_tools(&self.workspace, &self.skills, self.shell_timeout_secs)
            .with_policy(self.policy.clone())
    }

    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::Policy
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
            names.iter().any(|n| n == "Bash"),
            "expected Bash in registry, got: {names:?}"
        );
    }

    #[test]
    fn policy_provider_sandbox_mode_is_policy() {
        let dir = TempDir::new().unwrap();
        let p = PolicyToolSetProvider::restrictive(dir.path().to_path_buf(), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::Policy);
    }

    #[test]
    fn policy_provider_attaches_policy_to_registry() {
        let dir = TempDir::new().unwrap();
        let p = PolicyToolSetProvider::restrictive(dir.path().to_path_buf(), 30, vec![]);
        let reg = p.build_registry();
        // The policy is attached and check_shell works through it.
        let policy = reg.policy().expect("policy should be attached");
        assert!(policy.check_shell("ls").is_ok());
        assert!(policy.check_shell("rm -rf /").is_err());
    }

    #[test]
    fn policy_provider_new_uses_given_policy() {
        // kills mutation that replaces PolicyToolSetProvider::new() field assignments
        use crate::tools::policy_sandbox::PolicyConfig;
        let dir = TempDir::new().unwrap();
        let policy = PolicyConfig::default();
        let p = PolicyToolSetProvider::new(dir.path().to_path_buf(), 30, vec![], policy);
        assert_eq!(p.sandbox_mode(), SandboxMode::Policy);
        let reg = p.build_registry();
        // With a permissive policy, rm -rf should be allowed (no deny patterns)
        let attached_policy = reg.policy().expect("policy should be attached");
        assert!(
            attached_policy.check_shell("rm -rf /").is_ok(),
            "permissive policy should allow all shell commands"
        );
    }

    #[test]
    fn sandbox_mode_default_is_none() {
        // kills `#[default]` annotation removal mutation on SandboxMode::None
        assert_eq!(
            SandboxMode::default(),
            SandboxMode::None,
            "SandboxMode default must be None"
        );
    }

    #[test]
    fn local_provider_build_registry_has_read_tool() {
        // kills `build_registry` function-level replacement mutation
        let (p, _dir) = provider();
        let reg = p.build_registry();
        let names = reg.names();
        assert!(
            names.iter().any(|n| n == "Read"),
            "local registry must contain 'Read' tool; found: {names:?}"
        );
    }
}
