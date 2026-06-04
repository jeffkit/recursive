use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::tools::{LocalTransport, RunShell, ToolTransport};
use crate::ToolRegistry;
use serde_json::json;

use crate::tui::events::UiEvent;

pub fn build_bash_registry(root: &std::path::Path) -> ToolRegistry {
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    ToolRegistry::new(transport).register(Arc::new(
        RunShell::new(root).with_timeout(Duration::from_secs(300)),
    ))
}

pub fn resolve_workspace_root() -> PathBuf {
    std::env::var("RECURSIVE_WORKSPACE")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub async fn run_bash_command(
    registry: &ToolRegistry,
    seq: &AtomicU64,
    cmd: String,
    event_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) {
    let n = seq.fetch_add(1, Ordering::Relaxed);
    let id = format!("ui-bash-{n}");
    let arguments = json!({ "command": cmd });
    let arguments_str = arguments.to_string();

    let _ = event_tx.send(UiEvent::ToolCall {
        id: id.clone(),
        name: "Bash".into(),
        arguments: arguments_str,
    });

    let (output, success) = match registry.invoke("Bash", arguments).await {
        Ok(out) => (out, true),
        Err(e) => (format!("ERROR: {e}"), false),
    };

    let _ = event_tx.send(UiEvent::ToolResult {
        id,
        name: "Bash".into(),
        output,
        success,
    });
}
