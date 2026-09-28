//! Container-tier [`ToolSetProvider`] (Goal 403).
//!
//! [`ContainerToolSetProvider`] builds the full standard tool set (via
//! [`crate::tools::build_standard_tools_with_transport`]) with the single
//! shared transport (Goal 401) swapped for a [`ContainerTransport`], so
//! **every** I/O tool (Read / Write / Edit / Glob / Grep / count_lines /
//! Bash) executes inside a hardened, long-lived Docker container with the
//! workspace bind-mounted at `/workspace`.
//!
//! Unlike [`super::docker_provider::DockerToolSetProvider`] (which only
//! replaces the Bash tool), this rebinds the whole transport layer — and,
//! because it reuses the standard builder, keeps every non-transport tool
//! (memory / facts / scratchpad / TodoWrite / RunBackground / web tools /
//! LoadSkill / aliases / extra+session roots) intact.
//!
//! Container creation failure (image missing, daemon unreachable) is a hard
//! error — the provider never silently degrades to local execution. The
//! [`ToolSetProvider`] trait has no `Result` exit, so failure aborts the
//! process with a clear message (the CLI mirrors this contract for
//! non-`cloud-runtime` builds).
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::path::PathBuf;
use std::sync::Arc;

use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::ToolRegistry;

use super::container_transport::ContainerTransport;

/// Error returned when the sandbox container cannot be created. The CLI
/// surfaces it as a fatal error — never a silent local fallback.
#[derive(Debug)]
pub struct ContainerSetupError(pub String);

impl std::fmt::Display for ContainerSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "container sandbox creation failed: {} (refusing to fall back to local execution)",
            self.0
        )
    }
}

impl std::error::Error for ContainerSetupError {}

/// [`ToolSetProvider`] whose tools execute inside a container.
pub struct ContainerToolSetProvider {
    /// Host-side workspace directory (the bind source).
    pub workspace: PathBuf,
    pub shell_timeout_secs: u64,
    pub skills: Vec<crate::skills::Skill>,
}

impl ContainerToolSetProvider {
    pub fn new(
        workspace: PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self {
            workspace,
            shell_timeout_secs,
            skills,
        }
    }

    /// Create the container transport. Public so callers that need a
    /// `Result` (tests, alternative wiring) can create it directly.
    pub async fn create_transport(
        workspace: &std::path::Path,
    ) -> Result<ContainerTransport, ContainerSetupError> {
        ContainerTransport::new(workspace)
            .await
            .map_err(|e| ContainerSetupError(format!("docker: {e}")))
    }
}

impl ToolSetProvider for ContainerToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        // `build_registry` is sync but container creation is async; same
        // pattern as `docker_provider.rs` (requires a multi-thread
        // runtime — the CLI already runs one). Creation failure aborts
        // with a clear message: silently degrading to host execution
        // would defeat the sandbox.
        let workspace = self.workspace.clone();
        let transport = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { Self::create_transport(&workspace).await })
        })
        .unwrap_or_else(|e| {
            eprintln!("recursive: {e}");
            std::process::exit(2);
        });
        // Issue §5: capabilities() must reflect the environment — probe
        // the toolchain once so the report is not permanently empty.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(transport.prime_toolchain())
        });
        let shared: Arc<dyn crate::tools::ToolTransport> = Arc::new(transport);

        // Plan B (issue §1): reuse the full standard builder so memory /
        // facts / scratchpad / TodoWrite / RunBackground / WatchFile /
        // EstimateTokens / web tools / LoadSkill / aliases / extra+session
        // roots all survive the container rebind — only the shared
        // transport differs from the local default.
        crate::tools::build_standard_tools_with_transport_opt(
            shared,
            &self.workspace,
            &[],
            None,
            &self.skills,
            self.shell_timeout_secs,
            None,
            None,
            None,
            None,
            // `disable_host_exec=true`: run_background / check_background /
            // watch_file / stop_loop execute via the HOST `/bin/sh` and read
            // the HOST filesystem, which would bypass the container sandbox
            // (issue #30: "container runs the commands, host executes them"
            // split). Until they are re-bound to the transport, the
            // container tier honestly omits them instead of silently
            // exposing host execution.
            true,
        )
    }

    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::Container
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_provider_sandbox_mode_is_container() {
        let p = ContainerToolSetProvider::new(PathBuf::from("/tmp"), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::Container);
    }

    #[test]
    fn container_setup_error_message_mentions_no_fallback() {
        let e = ContainerSetupError("no daemon".into());
        let msg = format!("{e}");
        assert!(msg.contains("no daemon"));
        assert!(msg.contains("refusing to fall back"));
    }
}
