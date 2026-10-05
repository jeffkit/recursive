//! Build helpers: tool registry, agent runtime, MCP registration, skill discovery.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use recursive::config::Config;
use recursive::coordinator;
use recursive::mcp::{discover_mcp_servers, load_mcp_config, McpClient, McpServer, McpTool};
use recursive::skills::{discover_skills, skills_for_injection, Skill};
#[cfg(feature = "web_search")]
use recursive::tools::WebSearch;
use recursive::{
    assemble_system_prompt,
    llm::{AnthropicProvider, ChatProvider, OpenAiProvider},
    register_subagent_if_enabled,
    tools::fs::ReadFileState,
    tools::EpisodicRecall,
    tools::{
        BackgroundJobManager, CheckBackground, CountLines, EditTool, EstimateTokens, Forget,
        GlobTool, LoadSkill, LocalTransport, ReadFile, Recall, Remember, RunBackground, RunShell,
        ScratchpadDelete, ScratchpadGet, ScratchpadList, SearchFiles, TodoWriteTool, ToolTransport,
        WebFetch, WorkingMemoryTool, WriteFile,
    },
    tools::{ForgetFact, RecallFact, RememberFact, UpdateFact},
    AgentRuntime, AgentRuntimeBuilder, EventSink, NullSink, RetryPolicy, ToolRegistry,
};

/// Build the tool registry, optionally registering MCP tools from a config file.
///
/// When `read_state` is `Some`, the given shared state is used instead of
/// creating a new one. Returns both the registry and the read_state so
/// callers can share it with a [`FileReinjector`](recursive::compact::FileReinjector).
pub(crate) async fn build_tools(
    config: &Config,
    read_state: Option<Arc<Mutex<ReadFileState>>>,
) -> (ToolRegistry, Arc<Mutex<ReadFileState>>) {
    let root = &config.workspace;
    // Goal 403: container sandbox tier via RECURSIVE_SANDBOX=container.
    // The default (env unset) path below stays byte-identical; other tier
    // values are validated but handled by their own providers/goals.
    let sandbox = recursive::SandboxMode::from_env().unwrap_or_else(|e| {
        eprintln!("recursive: RECURSIVE_SANDBOX: {e}");
        std::process::exit(2);
    });
    match sandbox {
        Some(recursive::SandboxMode::Container) => {
            #[cfg(feature = "cloud-runtime")]
            {
                let skills = discover_loaded_skills(config);
                let provider = recursive::tools::ContainerToolSetProvider::new(
                    root.clone(),
                    config.shell_timeout_secs,
                    skills,
                );
                let registry = recursive::ToolSetProvider::build_registry(&provider);
                let read_state = registry
                    .read_file_state()
                    .unwrap_or_else(|| Arc::new(Mutex::new(ReadFileState::new())));
                return (registry, read_state);
            }
            #[cfg(not(feature = "cloud-runtime"))]
            {
                eprintln!(
                    "recursive: RECURSIVE_SANDBOX=container requires a build with the \
                     `cloud-runtime` feature; refusing to fall back to local execution"
                );
                std::process::exit(2);
            }
        }
        // Issue §3: policy tier gets the L1 policy attached via its
        // provider (previously the value was parsed and silently ignored).
        Some(recursive::SandboxMode::Policy) => {
            let skills = discover_loaded_skills(config);
            let provider = recursive::PolicyToolSetProvider::restrictive(
                root.clone(),
                config.shell_timeout_secs,
                skills,
            );
            let registry = recursive::ToolSetProvider::build_registry(&provider);
            let read_state = registry
                .read_file_state()
                .unwrap_or_else(|| Arc::new(Mutex::new(ReadFileState::new())));
            return (registry, read_state);
        }
        Some(recursive::SandboxMode::MicroVm) => {
            // Goal 405: microVM tier via E2B — same fatal-on-failure
            // contract as the container tier (never a silent local
            // fallback).
            #[cfg(feature = "e2b-sandbox")]
            {
                let skills = discover_loaded_skills(config);
                let provider = match recursive::tools::E2bToolSetProvider::from_env_provider(
                    root.clone(),
                    config.shell_timeout_secs,
                    skills,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!(
                            "recursive: RECURSIVE_SANDBOX=microvm requires the E2B \
                                 provider: {e} (refusing to fall back to local execution)"
                        );
                        std::process::exit(2);
                    }
                };
                let registry = recursive::ToolSetProvider::build_registry(&provider);
                let read_state = registry
                    .read_file_state()
                    .unwrap_or_else(|| Arc::new(Mutex::new(ReadFileState::new())));
                return (registry, read_state);
            }
            #[cfg(not(feature = "e2b-sandbox"))]
            {
                eprintln!(
                    "recursive: RECURSIVE_SANDBOX=microvm requires a build with the \
                     `e2b-sandbox` feature; refusing to fall back to local execution"
                );
                std::process::exit(2);
            }
        }
        Some(recursive::SandboxMode::None) | None => {}
    }
    // One shared transport instance for the registry AND the fs tools
    // (Goal 401): swap this single Arc to a container transport and the
    // whole toolset follows. Never construct a second transport for tools.
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let bg_manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
    let read_state = read_state.unwrap_or_else(|| Arc::new(Mutex::new(ReadFileState::new())));
    // Sandbox expansion: extra read-write roots (CLI `--add-dir` +
    // `[sandbox] extra_dirs`) and read-only roots (`[sandbox]
    // extra_readonly_dirs`). Each structured fs tool receives these so the
    // agent can reach out-of-workspace files without weakening the sandbox
    // for shell/checkpoint/etc.
    let extra_roots: Vec<(PathBuf, recursive::AccessTier)> = config
        .extra_dirs
        .iter()
        .cloned()
        .map(|p| (p, recursive::AccessTier::ReadWrite))
        .chain(
            config
                .extra_readonly_dirs
                .iter()
                .cloned()
                .map(|p| (p, recursive::AccessTier::ReadOnly)),
        )
        .collect();
    // Shared mutable sandbox roots so control `register_repo_root` can expand
    // the sandbox mid-run (Claude Code parity).
    let session_roots = recursive::new_shared_sandbox_roots();
    let mut registry = ToolRegistry::new(transport.clone())
        .with_read_file_state(read_state.clone())
        .register_with_aliases(
            Arc::new(
                ReadFile::new(root)
                    .with_extra_roots(extra_roots.clone())
                    .with_session_roots(session_roots.clone())
                    .with_read_state(read_state.clone())
                    .with_transport(transport.clone()),
            ),
            &["read_file"],
        )
        .register_with_aliases(
            Arc::new(
                WriteFile::new(root)
                    .with_extra_roots(extra_roots.clone())
                    .with_session_roots(session_roots.clone())
                    .with_read_state(read_state.clone())
                    .with_transport(transport.clone()),
            ),
            &["write_file"],
        )
        .register(Arc::new(
            EditTool::new(root)
                .with_extra_roots(extra_roots.clone())
                .with_session_roots(session_roots.clone())
                .with_read_state(read_state.clone())
                .with_transport(transport.clone()),
        ))
        .register_with_aliases(
            Arc::new(
                GlobTool::new(root)
                    .with_extra_roots(extra_roots.clone())
                    .with_session_roots(session_roots.clone())
                    .with_transport(transport.clone()),
            ),
            &["list_dir", "glob"],
        )
        .register(Arc::new(
            RunShell::new(root).with_timeout(Duration::from_secs(config.shell_timeout_secs)),
        ))
        .register(Arc::new(
            SearchFiles::new(root)
                .with_extra_roots(extra_roots.clone())
                .with_session_roots(session_roots.clone())
                .with_transport(transport.clone()),
        ))
        .register(Arc::new(WebFetch::new()))
        .register(Arc::new(RunBackground::new(root, bg_manager.clone())))
        .register(Arc::new(CheckBackground::new(bg_manager.clone())));
    #[cfg(feature = "web_search")]
    {
        let search = WebSearch::new().with_search_config(
            config.web_search_provider.clone(),
            config.web_search_api_key.clone(),
            config.web_search_jina_key.clone(),
        );
        registry = registry.register(Arc::new(search));
    }
    registry = registry.register(Arc::new(
        EstimateTokens::new(root)
            .with_extra_roots(extra_roots.clone())
            .with_session_roots(session_roots.clone()),
    ));
    registry = registry.register(Arc::new(
        CountLines::new(root)
            .with_extra_roots(extra_roots)
            .with_session_roots(session_roots.clone())
            .with_transport(transport),
    ));
    registry = registry.with_session_roots(session_roots);
    // Issue #93: share one vector-memory backend across remember/recall/forget
    // so the index write, semantic read and delete paths stay in sync.
    let (memory_store, memory_embedding) = recursive::memory::default_backends(root);
    registry = registry
        .register(Arc::new(Remember::new(root).with_vector_store(
            memory_store.clone(),
            memory_embedding.clone(),
        )))
        .register(Arc::new(Recall::new(root).with_vector_store(
            memory_store.clone(),
            memory_embedding.clone(),
        )))
        .register(Arc::new(
            Forget::new(root).with_vector_store(memory_store.clone()),
        ));
    registry = registry
        .register(Arc::new(RememberFact::new(root)))
        .register(Arc::new(RecallFact::new(root)))
        .register(Arc::new(ForgetFact::new(root)))
        .register(Arc::new(UpdateFact::new(root)));
    registry = registry.register(Arc::new(EpisodicRecall::new(root)));
    registry = registry
        .register(Arc::new(WorkingMemoryTool::new(root)))
        .register(Arc::new(ScratchpadGet::new(root)))
        .register(Arc::new(ScratchpadDelete::new(root)))
        .register(Arc::new(ScratchpadList::new(root)));
    // Goal-167: register with a NullSink placeholder; AgentRuntimeBuilder::build()
    // will overwrite this with a properly-wired sink.
    registry = registry.register(Arc::new(TodoWriteTool::new(
        Arc::new(std::sync::RwLock::new(vec![])),
        Arc::new(NullSink),
    )));
    let skills = discover_loaded_skills(config);
    if !skills.is_empty() {
        registry = registry.register(Arc::new(LoadSkill::new(skills)));
    }
    // Issue #63: registry-bound business-API tool. Only registered when an
    // endpoints config exists (`<workspace>/.recursive/endpoints.json` or
    // `RECURSIVE_ENDPOINTS_FILE`) — absent config leaves the tool surface
    // unchanged. The `RECURSIVE_ALLOW_TOOLS` allow-list is applied later as
    // the last assembly step (issue #65), so `HttpCall` narrows like any
    // other tool.
    if let Some(endpoints) = recursive::tools::EndpointRegistry::discover(root) {
        registry = registry.register(Arc::new(recursive::tools::HttpCall::new(endpoints)));
    }
    // Note: read-only checkpoint tools (checkpoint_list / checkpoint_diff)
    // are registered by the runtime when a session id is known, since
    // they must be scoped to the current session's checkpoint chain.
    if let Some(perms) = resolve_tool_permissions() {
        registry = registry.with_permissions(perms);
    }
    // Goal-199: headless mode — configure external hooks.
    {
        let mut hook_dirs: Vec<std::path::PathBuf> = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            hook_dirs.push(
                std::path::PathBuf::from(home)
                    .join(".recursive")
                    .join("hooks"),
            );
        }
        hook_dirs.push(config.workspace.join(".recursive").join("hooks"));
        let hook_runner = recursive::hooks::ExternalHookRunner::discover(&hook_dirs);
        registry = registry
            .with_headless(config.headless)
            .with_hook_runner(hook_runner);
    }
    (registry, read_state)
}

/// Apply the cross-cutting tool-surface wiring every agent-loop channel must
/// layer on top of [`build_tools`], in one place:
///
/// 1. MCP registration (`--mcp-config` / workspace auto-discovery) —
///    issue #70: `Cmd::Http` used to skip this, so AG-UI sessions saw no
///    `mcp__*` business tools at all.
/// 2. The per-registry `TouchedFiles` collector (per-turn checkpoint
///    recording reads it back via `ToolRegistry::touched_files`).
/// 3. Coordinator-mode pruning (`coordinator::filter_registry`; no-op
///    outside coordinator mode).
///
/// `elicitation` follows [`register_mcp_tools`]: interactive channels pass a
/// slot they pump; headless channels (HTTP) pass `None` — an MCP
/// `UrlElicitationRequired` then surfaces as a tool error instead of
/// blocking on a host that does not exist.
///
/// Registration runs before the coordinator prune, so coordinator mode keeps
/// every tool in its allow-set. The operator allow-list is deliberately NOT
/// applied here: sub-agent registration happens after this function (it adds
/// `agent` / `send_message` / `list_workers` post-prune by design), and the
/// operator's list must be the last word — call [`apply_operator_allow_list`]
/// after sub-agent registration (issue #65's "exactly the allowed set").
pub(crate) async fn finish_tool_surface(
    mut tools: ToolRegistry,
    config: &Config,
    mcp_config: Option<PathBuf>,
    elicitation: Option<recursive::mcp::SharedElicitationHandler>,
) -> ToolRegistry {
    if let Some(slot) = elicitation.clone() {
        tools = tools.with_elicitation_slot(slot);
    }
    register_mcp_tools(&mut tools, &config.workspace, mcp_config, elicitation).await;
    // Always attach a TouchedFiles collector so AgentRuntime can record
    // per-turn file touches when checkpoints are enabled later via
    // enable_checkpoints(). When checkpoints are disabled this is a
    // no-op observer.
    tools = tools.with_touched_files(Arc::new(std::sync::Mutex::new(
        recursive::TouchedFiles::new(),
    )));
    coordinator::filter_registry(&mut tools);
    tools
}

/// Apply the operator allow-list (`--allow-tools` / `RECURSIVE_ALLOW_TOOLS`)
/// — issue #65. Must run as the LAST step of a channel's tool assembly,
/// after sub-agent registration, so the advertised surface is exactly the
/// allowed set. No-op when no list is configured.
pub(crate) fn apply_operator_allow_list(tools: &mut ToolRegistry, config: &Config) {
    if !config.allow_tools.is_empty() {
        tools.retain_tools(&config.allow_tools);
    }
}

/// Resolve the active tool-permission configuration.
///
/// Resolution order:
///   1. `RECURSIVE_TOOL_PERMISSIONS_FILE=<path>` env — TOML file
///      whose top-level keys are `allow`, `deny`, `interactive`
///      (matches [`recursive::permissions::OldPermissionsConfig`] verbatim).
///   2. `~/.recursive/config.toml`'s `[permissions]` section.
///   3. None — every tool allowed (back-compat default).
///
/// Errors during file read or TOML parse are logged to stderr and
/// treated as "no permissions config" — a malformed file should not
/// brick the CLI for unrelated commands.
fn resolve_tool_permissions() -> Option<recursive::permissions::PermissionsConfig> {
    if let Ok(path) = std::env::var("RECURSIVE_TOOL_PERMISSIONS_FILE") {
        if let Some(perms) = permissions_from_env_path(&path) {
            return Some(perms);
        }
    }
    let file_config = recursive::config_file::FileConfig::load().ok().flatten()?;
    Some(permissions_from_section(file_config.permissions?))
}

/// Parse a `RECURSIVE_TOOL_PERMISSIONS_FILE` value into a permissions config.
///
/// Returns `None` for an empty path, an unreadable file, or malformed TOML
/// (each failure is logged) so `resolve_tool_permissions` can fall through to
/// the `~/.recursive/config.toml` source.
fn permissions_from_env_path(path: &str) -> Option<recursive::permissions::PermissionsConfig> {
    if !path.is_empty() {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                match toml::from_str::<recursive::permissions::OldPermissionsConfig>(&content) {
                    Ok(old) => return Some(old.into()),
                    Err(e) => {
                        eprintln!("permissions: failed to parse {path}: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("permissions: failed to read {path}: {e}");
            }
        }
    }
    None
}

/// Whether a `[permissions]` section carries any user rule list at all.
///
/// A section with no allow/deny/interactive entries contributes no layer
/// (the mode still applies), so callers must not push an empty layer.
fn permissions_section_has_rules(section: &recursive::config_file::PermissionsSection) -> bool {
    !section.allow.is_empty() || !section.deny.is_empty() || !section.interactive.is_empty()
}

/// Turn a `[permissions]` section into a single-layer user config.
fn permissions_from_section(
    section: recursive::config_file::PermissionsSection,
) -> recursive::permissions::PermissionsConfig {
    let has_rules = permissions_section_has_rules(&section);
    let mode = section.mode.unwrap_or_default();
    let mut layers = Vec::new();
    if has_rules {
        layers.push(recursive::permissions::PermissionLayer {
            source: recursive::permissions::RuleSource::User,
            allow: section.allow,
            deny: section.deny,
            interactive: section.interactive,
        });
    }
    recursive::permissions::LayeredPermissionsConfig { mode, layers }
}

/// The operator-facing log line for workspace auto-discovery, or `None` when
/// no servers were found (silence is the documented behaviour for an empty
/// discovery result).
fn auto_discovered_message(servers: &[McpServer]) -> Option<String> {
    if !servers.is_empty() {
        Some(format!(
            "mcp: auto-discovered {} server(s) from workspace",
            servers.len()
        ))
    } else {
        None
    }
}

/// Register MCP tools from a config file into the registry.
pub(crate) async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    workspace: &Path,
    mcp_config_path: Option<PathBuf>,
    elicitation: Option<recursive::mcp::SharedElicitationHandler>,
) {
    let servers: Vec<McpServer> = if let Some(path) = &mcp_config_path {
        // Explicit config file provided
        if !path.exists() {
            eprintln!("warning: MCP config file not found: {}", path.display());
            return;
        }
        match load_mcp_config(path) {
            Ok(s) => {
                eprintln!(
                    "mcp: loaded {} server(s) from explicit config `{}`",
                    s.len(),
                    path.display()
                );
                s
            }
            Err(e) => {
                eprintln!("warning: failed to load MCP config: {e}");
                return;
            }
        }
    } else {
        // Auto-discover from workspace
        match discover_mcp_servers(workspace).await {
            Ok(s) => {
                if let Some(msg) = auto_discovered_message(&s) {
                    eprintln!("{msg}");
                }
                s
            }
            Err(e) => {
                eprintln!("warning: failed to auto-discover MCP servers: {e}");
                return;
            }
        }
    };
    if servers.is_empty() {
        return;
    }
    for server in &servers {
        match register_mcp_server_tools(registry, server, elicitation.clone()).await {
            Ok(count) => {
                eprintln!(
                    "mcp: registered {} tool(s) from server `{}`",
                    count, server.name
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to register MCP server `{}`: {e}",
                    server.name
                );
            }
        }
    }
}

/// Spawn an MCP server, list its tools, and register them in the registry.
async fn register_mcp_server_tools(
    registry: &mut ToolRegistry,
    server: &McpServer,
    elicitation: Option<recursive::mcp::SharedElicitationHandler>,
) -> anyhow::Result<usize> {
    let mut client = McpClient::spawn(server).await?;
    if let Some(slot) = elicitation {
        client = client.with_elicitation(slot);
    }
    let tool_specs = client.list_tools().await?;
    let count = tool_specs.len();
    let client = Arc::new(tokio::sync::Mutex::new(client));
    for spec in tool_specs {
        let tool = McpTool::new(client.clone(), spec, &server.name);
        registry.register_mut(Arc::new(tool));
    }
    Ok(count)
}

/// Discover skills from configured search paths.
/// Defaults: <workspace>/.recursive/skills/, <workspace>/.claude/skills/, ~/.recursive/skills/, ~/.claude/skills/.
/// Override with RECURSIVE_SKILL_PATHS=path1:path2 (colon-separated).
pub(crate) fn discover_loaded_skills(config: &Config) -> Vec<Skill> {
    let paths: Vec<PathBuf> = if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
        // 平台路径分隔符：unix ':' / windows ';'。硬编码 ':' 会把 windows 盘符
        // 路径（C:\...）从盘符处切碎（2026-09-30 windows CI 实证）。
        std::env::split_paths(&env_paths).collect()
    } else {
        let mut defaults = vec![
            config.workspace.join(".recursive").join("skills"),
            config.workspace.join(".claude").join("skills"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            defaults.push(PathBuf::from(&home).join(".recursive").join("skills"));
            defaults.push(PathBuf::from(home).join(".claude").join("skills"));
        }
        defaults
    };
    discover_skills(&paths)
}

/// Load skills from service-level HTTP [`recursive::HttpSkillSource`]s.
///
/// `RECURSIVE_SKILL_SOURCE_URL` holds one or more comma-separated endpoint
/// URLs; each must answer with the JSON skill-index shape
/// (`{"skills": [{"name", "content", "description"?, "version"?, "sha256"?}]}`)
/// enforced by [`recursive::HttpSkillSource`]. The returned skills are
/// content-backed (in-memory `body`, `/virtual/skills/<name>` synthetic path)
/// — nothing is written to or read from the local filesystem.
///
/// Two optional hardening envs:
/// - `RECURSIVE_SKILL_SOURCE_HOSTS` — comma-separated host allowlist (an
///   optional `:port` is honoured). When set, every URL's host must match an
///   entry exactly; unset/blank keeps the historical "any https host"
///   behaviour.
/// - `RECURSIVE_SKILL_SOURCE_SHA256` — comma-separated SHA-256 pins, one per
///   URL positionally, over the exact index body. This is the operator-side
///   trust anchor: a compromised skill host supplies both the content and any
///   per-entry digest, so only an out-of-band body pin detects a swapped
///   index. Unset/blank disables pinning.
///
/// Errors from any URL abort the whole load (`Err`): the caller decides
/// whether that is fatal or a log-and-degrade.
pub(crate) fn skills_from_http_sources() -> Result<Vec<Skill>, String> {
    let raw = std::env::var("RECURSIVE_SKILL_SOURCE_URL").unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let urls: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .collect();
    let pins = skill_source_sha256_pins();
    if !pins.is_empty() && pins.len() != urls.len() {
        return Err(format!(
            "RECURSIVE_SKILL_SOURCE_SHA256 has {} pin(s) for {} URL(s); \
             provide one comma-separated pin per URL (in URL order)",
            pins.len(),
            urls.len()
        ));
    }
    let allowed_hosts = skill_source_allowed_hosts();
    let mut skills = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let mut source = recursive::HttpSkillSource::new(*url, allowed_hosts.clone());
        if let Some(pin) = pins.get(i) {
            source = source.with_pinned_sha256(pin.clone());
        }
        skills.extend(source.load_skills().map_err(|e| e.to_string())?);
    }
    Ok(skills)
}

/// Parse `RECURSIVE_SKILL_SOURCE_HOSTS`; unset/blank disables the allowlist.
fn skill_source_allowed_hosts() -> Option<Vec<String>> {
    let Ok(raw) = std::env::var("RECURSIVE_SKILL_SOURCE_HOSTS") else {
        return None;
    };
    let hosts: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect();
    if hosts.is_empty() {
        None
    } else {
        Some(hosts)
    }
}

/// Parse the positional `RECURSIVE_SKILL_SOURCE_SHA256` pins (may be empty).
fn skill_source_sha256_pins() -> Vec<String> {
    let Ok(raw) = std::env::var("RECURSIVE_SKILL_SOURCE_SHA256") else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Append auto-loaded skill bodies to the assembled system prompt.
///
/// Injects `=== Skill: <name> (auto-loaded) ===` blocks until the running
/// total would exceed a fixed 8192-byte budget; the block that crosses the
/// budget is truncated (or replaced by a bare `[truncated]` marker when
/// fewer than 20 bytes remain) and iteration stops. Extracted from
/// `build_runtime` so the truncation arithmetic is unit-testable.
fn apply_skill_injection(mut base: String, injected: &[(String, String)]) -> String {
    if !injected.is_empty() {
        let mut injection_block = String::new();
        let mut total_chars = 0usize;
        let max_injection_chars = 8192usize;
        for (name, body) in injected {
            let snippet = format!("=== Skill: {name} (auto-loaded) ===\n{body}\n\n");
            if total_chars + snippet.len() > max_injection_chars {
                let remaining = max_injection_chars.saturating_sub(total_chars);
                let truncated = if remaining > 20 {
                    format!(
                        "{}...\n[truncated]\n",
                        &snippet[..remaining.saturating_sub(20)]
                    )
                } else {
                    "[truncated]\n".to_string()
                };
                injection_block.push_str(&truncated);
                break;
            }
            injection_block.push_str(&snippet);
            total_chars += snippet.len();
        }
        base = format!("{}\n\n{}", base, injection_block);
    }
    base
}

/// Construct the LLM provider named by `config.provider_type`.
///
/// Shared by every surface that used to open-code the same `match` (the
/// agent runtime, loop mode, the ACP server and the HTTP server), so the
/// `"anthropic"` arm exists in exactly one place. `max_search_rounds`
/// preserves the per-surface behaviour: agent surfaces forward
/// `config.max_search_rounds`, while the ACP/HTTP servers keep the
/// provider default (`None`) as before this helper existed.
pub(crate) fn build_llm_provider(
    config: &Config,
    api_key: &str,
    retry: RetryPolicy,
    max_search_rounds: Option<usize>,
) -> anyhow::Result<Arc<dyn ChatProvider>> {
    let provider: Arc<dyn ChatProvider> = match config.provider_type.as_str() {
        "anthropic" => {
            let anthropic_retry = recursive::llm::RetryPolicy {
                max_retries: config.retry_max,
                initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
            };
            let mut anthropic = AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_max_tokens(config.max_tokens)
                .with_retry_policy(anthropic_retry);
            if let Some(rounds) = max_search_rounds {
                anthropic = anthropic.with_max_search_rounds(rounds);
            }
            Arc::new(anthropic)
        }
        _ => {
            let mut openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_max_tokens(config.max_tokens)
                .with_retry_policy(retry);
            if let Some(rounds) = max_search_rounds {
                openai = openai.with_max_search_rounds(rounds);
            }
            Arc::new(openai)
        }
    };
    Ok(provider)
}

/// Build an [`AgentRuntime`], optionally registering MCP tools from a config file.
///
/// Cancellation is supplied by the caller in one of two shapes (issue #40 /
/// Goal 407):
///
/// - `shutdown_token`: a static, one-shot token minted once per process (loop
///   mode, HTTP serve, weixin daemon, `resume`). The `agent` tool gets a
///   one-shot filled [`SharedTokenSlot`] carrying it, so parallel workers
///   inherit a child token through the existing child-token tree.
/// - `subagent_token_slot`: a host-owned per-turn slot (REPL). The host
///   refreshes it at every turn start and clears it at every turn end; the
///   `agent` tool reads it at dispatch time. Use this — never a static token —
///   for a long-lived multi-turn runtime, where a static token would poison
///   every turn after the first cancellation.
///
/// When both are given the explicit slot wins for the `agent` tool while the
/// static token still arms the parent kernel. No current caller passes both.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_runtime(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
    stream: bool,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    goal: Option<&str>,
    event_sink: Option<Arc<dyn EventSink>>,
    shutdown_token: Option<tokio_util::sync::CancellationToken>,
    subagent_token_slot: Option<recursive::SharedTokenSlot>,
    // Pass `true` for interactive channels (TUI, CLI) that have a live human
    // to call `confirm_plan()`. Headless/batch callers pass `false`.
    interactive: bool,
) -> anyhow::Result<AgentRuntime> {
    let api_key = config.require_api_key()?;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider = build_llm_provider(config, api_key, retry, Some(config.max_search_rounds))?;
    let (tools, read_state) = build_tools(config, None).await;
    // MCP + touched-files + coordinator pruning all live in one shared tail
    // (issues #70 / #65) so channels cannot drift apart.
    let elicitation = recursive::mcp::new_elicitation_slot();
    let mut tools = finish_tool_surface(tools, config, mcp_config, Some(elicitation)).await;

    // Sub-agent / team coordination is a channel-agnostic capability: every
    // agent-loop surface registers the unified `Agent` tool when
    // `config.subagent_enabled` is set, in lockstep with the coordinator
    // prompt injected by `assemble_system_prompt`.
    //
    // Issue #40: single-turn surfaces mint their shutdown token once, so a
    // one-shot filled slot (never refreshed) carries static-token semantics.
    // Goal 407: multi-turn surfaces (REPL) pass their own per-turn slot
    // instead — it wins over the one-shot fallback, so the host can refresh
    // the token at every turn start without rebuilding the runtime.
    let subagent_token_slot = subagent_token_slot.or_else(|| {
        shutdown_token
            .clone()
            .map(|token| Arc::new(Mutex::new(Some(token))))
    });
    tools = register_subagent_if_enabled(tools, config, provider.clone(), subagent_token_slot);
    // Issue #65: the operator allow-list is the last word — applied after
    // sub-agent registration so the surface is exactly the allowed set.
    apply_operator_allow_list(&mut tools, config);

    let skills = discover_loaded_skills(config);

    // Common system-prompt assembly (project context + base + skill index +
    // coordinator workflow/sub_agent note when enabled) lives in one place.
    // We hold onto the full AssembledPrompt (not just the joined string)
    // so the Goal-328 ContextBreakdown estimator can size the static
    // buckets from the structured segments.
    //
    // CLI-run-only: auto-load matching skill *bodies* based on the goal (the
    // index above only lists skill names). Other channels don't have a goal
    // at prompt-build time, so this stays a CLI-run-specific suffix.
    let mut assembled = assemble_system_prompt(
        &config.system_prompt,
        &config.workspace,
        &skills,
        config.subagent_enabled,
    );
    let injected = skills_for_injection(&skills, goal.unwrap_or(""));
    assembled.full = apply_skill_injection(assembled.full, &injected);
    // Goal-328: forward the structured segments to the runtime so the
    // local ContextBreakdown estimator can size the static buckets.
    // The joined prompt (`assembled.full`) is consumed directly by the
    // builder.
    let prompt_segments = assembled.segments;

    let mut builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&assembled.full)
        .prompt_segments(prompt_segments)
        .max_steps(config.max_steps)
        // Goal 399: `RECURSIVE_WALL_TIMEOUT_SECS` now reaches the agent loop —
        // previously parsed into Config but never consumed anywhere.
        .wall_timeout_secs(config.wall_timeout_secs)
        .streaming(stream)
        .stuck_window(config.stuck_window)
        .stuck_error_rate(config.stuck_error_rate)
        .goal_eval_transcript_tail(config.goal_eval_transcript_tail);
    if let Some(token) = shutdown_token {
        builder = builder.shutdown_token(token);
    }
    if !seed.is_empty() {
        builder = builder.seed_transcript(seed);
    }
    // Goal-393: compactor / microcompactor / transcript cap assembly is
    // shared with the HTTP frontends via the frontend-neutral helper, so the
    // two channels cannot drift apart again. Env semantics
    // (RECURSIVE_COMPACT_THRESHOLD / RECURSIVE_MICROCOMPACT_TRIGGER /
    // RECURSIVE_MICROCOMPACT_KEEP / RECURSIVE_MAX_TRANSCRIPT_CHARS) are
    // documented there.
    builder = recursive::runtime::apply_context_management(builder, config);
    // CLI-specific override: the --max-transcript-chars flag (and its clap
    // env fallback) keeps flag-over-env precedence over the cap the helper
    // just applied.
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    // Goal-334: file reinjector for post-compaction restoration of recently-read files.
    if let Some(r) = recursive::build_file_reinjector_from_env(read_state.clone()) {
        builder = builder.file_reinjector(r);
    }
    // Goal-335: skill reinjector for post-compaction restoration of invoked skills.
    if let Some(r) = recursive::build_skill_reinjector_from_env(skills.clone()) {
        builder = builder.skill_reinjector(r);
    }
    if hook_timing {
        use recursive::hooks::HookRegistry;
        let mut hooks = HookRegistry::new();
        hooks.register(Arc::new(recursive::hooks::ToolTimingHook::new()));
        builder = builder.hooks(hooks);
    }
    if let Some(sink) = event_sink {
        builder = builder.event_sink(sink);
    }
    builder
        .with_plan_mode_tools(interactive)
        .build()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── skills_from_http_sources (issue #74 拆单 3/3) ────────────────────

    /// Unset / blank RECURSIVE_SKILL_SOURCE_URL is a no-op: empty catalog,
    /// no network. Each half is scoped: ENV_LOCK is a non-reentrant
    /// std Mutex, so a second `EnvGuard::set` while the first guard is
    /// still alive self-deadlocks the whole test binary.
    #[test]
    fn skills_from_http_sources_unset_or_blank_is_empty() {
        {
            let _env = EnvGuard::set(&[("RECURSIVE_SKILL_SOURCE_URL", None)]);
            assert!(skills_from_http_sources().unwrap().is_empty());
        }
        {
            let _env = EnvGuard::set(&[("RECURSIVE_SKILL_SOURCE_URL", Some("   "))]);
            assert!(skills_from_http_sources().unwrap().is_empty());
        }
    }

    /// A configured URL failing the https-only gate surfaces a descriptive
    /// error (the http serve entry prints WARN and continues with
    /// directory-discovered skills).
    #[test]
    fn skills_from_http_sources_plain_http_url_errors() {
        let _env = EnvGuard::set(&[(
            "RECURSIVE_SKILL_SOURCE_URL",
            Some("http://skills.example.com/index.json"),
        )]);
        let err = skills_from_http_sources().unwrap_err();
        assert!(
            err.contains("https://"),
            "error must name the https-only policy: {err}"
        );
    }

    /// The production entry wires `RECURSIVE_SKILL_SOURCE_HOSTS` into the
    /// source's allowlist: a URL whose host is not listed is rejected at the
    /// config gate, before any request is issued.
    #[test]
    fn skills_from_http_sources_allowlist_blocks_unlisted_host() {
        let _env = EnvGuard::set(&[
            (
                "RECURSIVE_SKILL_SOURCE_URL",
                Some("https://evil.example.com/index.json"),
            ),
            (
                "RECURSIVE_SKILL_SOURCE_HOSTS",
                Some("skills.corp.example.com"),
            ),
            ("RECURSIVE_SKILL_SOURCE_SHA256", None),
        ]);
        let err = skills_from_http_sources().unwrap_err();
        assert!(
            err.contains("not in the allowlist"),
            "unlisted host must be rejected by the allowlist: {err}"
        );
    }

    /// A listed host clears the allowlist gate — the failure is then the
    /// (dead) network endpoint, not a policy rejection.
    #[tokio::test(flavor = "multi_thread")]
    async fn skills_from_http_sources_allowlist_admits_listed_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let _env = EnvGuard::set(&[
            (
                "RECURSIVE_SKILL_SOURCE_URL",
                Some(format!("https://{addr}/skills.json").as_str()),
            ),
            (
                "RECURSIVE_SKILL_SOURCE_HOSTS",
                Some(format!("127.0.0.1:{}", addr.port()).as_str()),
            ),
            ("RECURSIVE_SKILL_SOURCE_SHA256", None),
        ]);
        let err = skills_from_http_sources().unwrap_err();
        assert!(
            !err.contains("allowlist"),
            "a listed host must clear the allowlist gate: {err}"
        );
    }

    /// Pins are positional (one per URL); a mismatch is caught before any
    /// fetch so a typo can never silently leave a URL unpinned.
    #[test]
    fn skills_from_http_sources_pin_count_must_match_url_count() {
        let _env = EnvGuard::set(&[
            (
                "RECURSIVE_SKILL_SOURCE_URL",
                Some("https://a.example.com/index.json,https://b.example.com/index.json"),
            ),
            ("RECURSIVE_SKILL_SOURCE_HOSTS", None),
            ("RECURSIVE_SKILL_SOURCE_SHA256", Some("deadbeef")),
        ]);
        let err = skills_from_http_sources().unwrap_err();
        assert!(
            err.contains("one comma-separated pin per URL"),
            "a pin count mismatch must be rejected before any fetch: {err}"
        );
    }

    /// End-to-end through the service-level entry: an https endpoint that
    /// refuses the connection surfaces as a propagated `Err` — the fetch
    /// really left the process (loopback, no disk involved), and the http
    /// serve entry's WARN-and-degrade contract has something to act on.
    ///
    /// The full fetch+mapping happy path lives in `src/skills.rs`
    /// (`http_skill_source_fetches_and_parses_the_index`): the https-only
    /// gate deliberately blocks plain-http loopback mocks at the source
    /// constructor, and the lib's test seam (`fetch_remote_index`) is not
    /// reachable from this crate.
    ///
    /// `#[tokio::test(flavor = "multi_thread")]` (not `#[test]`):
    /// `HttpSkillSource::load_skills` re-enters `Handle::current()`, which
    /// panics outside a runtime — and `block_in_place` additionally requires
    /// a multi-thread runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn skills_from_http_sources_propagates_request_failure() {
        // Bind-then-drop: a port nothing listens on. Connect fails fast
        // (ECONNREFUSED), no 10s connect timeout.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let _env = EnvGuard::set(&[(
            "RECURSIVE_SKILL_SOURCE_URL",
            Some(format!("https://{addr}/skills.json").as_str()),
        )]);
        let err = skills_from_http_sources().unwrap_err();
        assert!(
            err.contains("request failed") || err.contains("error"),
            "a dead endpoint must surface as a request error: {err}"
        );
    }

    fn test_config() -> Config {
        Config {
            workspace: PathBuf::from("."),
            api_base: "https://api.deepseek.com/v1".to_string(),
            api_key: Some("sk-test".to_string()),
            model: "deepseek-chat".to_string(),
            provider_type: "openai".to_string(),
            preset: None,
            max_steps: 32,
            max_tokens: 65536,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
            headless: true,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
            wall_timeout_secs: 0,
        }
    }

    /// Goal-334: `build_tools` returns the shared `read_state` so `build_runtime`
    /// can hand it to a `FileReinjector`. Pin two contracts:
    ///   1. passing `None` creates a fresh state and the returned registry's
    ///      `read_file_state()` points at the SAME arc (so the reinjector sees
    ///      what Read just recorded);
    ///   2. passing `Some(custom)` reuses that exact arc (no hidden copy).
    ///
    /// Goal 403 / issue §5: the sandbox tier selection entry point is pinned
    /// at the source level — `build_tools` dispatches on
    /// `SandboxMode::from_env()` and every non-local tier refuses to
    /// degrade (exit(2)) rather than silently running on the host. A
    /// runtime test of the container arm needs a Docker daemon, so the
    /// dispatch + refusal contract is asserted against the builder source.
    #[test]
    fn build_tools_dispatches_on_sandbox_mode_without_silent_fallback() {
        let src = include_str!("builder.rs");
        // Dispatch happens on SandboxMode::from_env().
        assert!(
            src.contains("SandboxMode::from_env()"),
            "build_tools must select the tier via SandboxMode::from_env()"
        );
        // Every non-local tier arm must exit(2) on failure instead of
        // falling through to the local registry below the match.
        let container_arm = src
            .split("Some(recursive::SandboxMode::Container) => {")
            .nth(1)
            .and_then(|r| r.split("Some(recursive::SandboxMode::Policy)").next())
            .expect("Container arm");
        assert!(
            container_arm.contains("ContainerToolSetProvider"),
            "container tier must dispatch to ContainerToolSetProvider"
        );
        let microvm_arm = src
            .split("Some(recursive::SandboxMode::MicroVm) => {")
            .nth(1)
            .and_then(|r| r.split("Some(recursive::SandboxMode::None)").next())
            .expect("MicroVm arm");
        // Goal 405: the microvm arm wires the E2B provider when the
        // feature is on, and still refuses (exit 2) on the no-feature /
        // no-key paths — the arm text contains both the provider dispatch
        // and the exit(2) refusals.
        assert!(
            microvm_arm.contains("E2bToolSetProvider"),
            "microvm tier must dispatch to E2bToolSetProvider (feature build)"
        );
        assert!(
            microvm_arm.contains("std::process::exit(2)"),
            "microvm tier must refuse (exit 2) on missing feature/key, not degrade"
        );
        // The container tier without the cloud-runtime feature must also
        // exit(2) — that is the "no silent downgrade" contract for the
        // default binary.
        let no_feature = src
            .split("#[cfg(not(feature = \"cloud-runtime\"))]")
            .nth(1)
            .and_then(|r| r.split("Some(recursive::SandboxMode::Policy)").next())
            .expect("non-cloud-runtime container arm");
        assert!(
            no_feature.contains("std::process::exit(2)"),
            "container tier on a non-cloud-runtime build must exit(2)"
        );
    }

    #[tokio::test]
    async fn build_tools_returns_shared_read_state_when_none() {
        let cfg = test_config();
        let (tools, read_state) = build_tools(&cfg, None).await;
        let from_registry = tools
            .read_file_state()
            .expect("registry must expose the shared read_file_state");
        // Same strong-counted arc → mutations through one are visible to the other.
        assert!(
            Arc::ptr_eq(&from_registry, &read_state),
            "build_tools must share the SAME read_state arc between the registry and its return value"
        );
    }

    #[tokio::test]
    async fn build_tools_reuses_supplied_read_state_when_some() {
        let cfg = test_config();
        let custom = Arc::new(Mutex::new(ReadFileState::new()));
        let (_tools, read_state) = build_tools(&cfg, Some(custom.clone())).await;
        assert!(
            Arc::ptr_eq(&read_state, &custom),
            "build_tools must reuse the caller-supplied read_state arc verbatim"
        );
    }

    // ── Test harness helpers ─────────────────────────────────────────────

    /// Serialises tests that mutate process-wide env vars. Every env-writing
    /// test holds this lock via [`EnvGuard`] and restores the prior value on
    /// drop, so the developer's real environment (and parallel tests) are
    /// never left modified.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(vars: &[(&str, Option<&str>)]) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = vars
                .iter()
                .map(|(k, _)| ((*k).to_string(), std::env::var_os(k)))
                .collect();
            for (k, v) in vars {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
            EnvGuard { saved, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// A discovered skill lives at `<root>/<dir>/SKILL.md`. Returns `root`.
    fn seed_skill(root: &Path, dir: &str, name: &str) {
        let skill_dir = root.join(dir);
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: demo\nmode: always\n---\n\nBody.\n"),
        )
        .expect("write SKILL.md");
    }

    // ── build_tools: LoadSkill registration (line 216) ───────────────────

    /// `build_tools` registers the `Skill` (LoadSkill) tool *only* when skill
    /// discovery actually found something. Deleting the `!` inverts that, so a
    /// workspace with a discovered skill must surface the tool.
    #[tokio::test]
    async fn build_tools_registers_the_skill_tool_when_skills_are_discovered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills_root = tmp.path().join("skills");
        seed_skill(&skills_root, "demo", "demo");
        let _env = EnvGuard::set(&[(
            "RECURSIVE_SKILL_PATHS",
            Some(skills_root.to_str().expect("utf8 path")),
        )]);
        let mut cfg = test_config();
        cfg.workspace = tmp.path().to_path_buf();
        let (tools, _read_state) = build_tools(&cfg, None).await;
        assert!(
            tools.find_by_name("Skill").is_some(),
            "a discovered skill must make build_tools register the Skill tool"
        );
    }

    // ── resolve_tool_permissions (lines 257/258/278) ─────────────────────

    /// `resolve_tool_permissions` returns `Some(Default::default())` /
    /// `None` mutants unless the env-file branch is genuinely reached with a
    /// non-default payload.
    #[test]
    fn resolve_tool_permissions_prefers_the_env_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("perms.toml");
        std::fs::write(&file, "interactive = [\"write_file\"]\n").expect("write perms");
        let _env = EnvGuard::set(&[(
            "RECURSIVE_TOOL_PERMISSIONS_FILE",
            Some(file.to_str().expect("utf8 path")),
        )]);
        let perms = resolve_tool_permissions().expect("env file must be honoured");
        assert_eq!(
            perms.layers.len(),
            1,
            "the env file's single layer must be present, not a default config"
        );
        assert_eq!(perms.layers[0].interactive, vec!["write_file".to_string()]);
    }

    /// `permissions_from_env_path` must actually read a non-empty path; the
    /// `!path.is_empty()` guard mutant makes a real file look empty.
    #[test]
    fn permissions_from_env_path_parses_a_valid_file_and_ignores_an_empty_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("perms.toml");
        std::fs::write(&file, "deny = [\"run_shell\"]\n").expect("write perms");
        let perms = permissions_from_env_path(file.to_str().expect("utf8 path"))
            .expect("a valid file must parse");
        assert_eq!(perms.layers.len(), 1);
        assert_eq!(perms.layers[0].deny, vec!["run_shell".to_string()]);
        assert!(
            permissions_from_env_path("").is_none(),
            "an empty path is 'no env override', not an error"
        );
    }

    fn perms_section(
        allow: &[&str],
        deny: &[&str],
        interactive: &[&str],
    ) -> recursive::config_file::PermissionsSection {
        recursive::config_file::PermissionsSection {
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: deny.iter().map(|s| (*s).to_string()).collect(),
            interactive: interactive.iter().map(|s| (*s).to_string()).collect(),
            plan: Vec::new(),
            mode: None,
        }
    }

    /// Pins the `allow || deny || interactive` predicate that decides whether a
    /// user layer is pushed: only a non-empty list counts, and one non-empty
    /// list is enough (kills the per-clause `!` deletions and the `||`→`&&`
    /// swaps at line 278).
    #[test]
    fn permissions_section_has_rules_needs_exactly_one_non_empty_list() {
        assert!(!permissions_section_has_rules(&perms_section(
            &[],
            &[],
            &[]
        )));
        assert!(permissions_section_has_rules(&perms_section(
            &["a"],
            &[],
            &[]
        )));
        assert!(permissions_section_has_rules(&perms_section(
            &[],
            &["d"],
            &[]
        )));
        assert!(permissions_section_has_rules(&perms_section(
            &[],
            &[],
            &["i"]
        )));
    }

    /// A section with no rules contributes no layer; one with rules contributes
    /// a single `User` layer carrying them.
    #[test]
    fn permissions_from_section_pushes_a_layer_only_when_rules_exist() {
        let empty = permissions_from_section(perms_section(&[], &[], &[]));
        assert!(
            empty.layers.is_empty(),
            "an empty section contributes no layer"
        );

        let with_rules = permissions_from_section(perms_section(&["a"], &[], &[]));
        assert_eq!(with_rules.layers.len(), 1);
        assert_eq!(with_rules.layers[0].allow, vec!["a".to_string()]);
    }

    // ── MCP registration (lines 296/298/320/358) ─────────────────────────

    /// A tiny stdio JSON-RPC MCP server: it handles `initialize` and
    /// `tools/list` (returning two tools) by echoing the request id. Written in
    /// POSIX `sh` so no extra test binary or runtime is required.
    const MOCK_MCP_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":true}}}\n' "${id:-0}"
      ;;
    *'"method":"tools/list"'*)
      id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"alpha","description":"Alpha","inputSchema":{"type":"object"}},{"name":"beta","description":"Beta","inputSchema":{"type":"object"}}]}}\n' "${id:-0}"
      ;;
  esac
done
"#;

    fn mock_mcp_server_script(dir: &Path) -> String {
        let path = dir.join("mock_mcp_server.sh");
        std::fs::write(&path, MOCK_MCP_SERVER).expect("write mock server");
        path.to_string_lossy().into_owned()
    }

    fn explicit_mcp_config(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("explicit_mcp.json");
        let cfg = serde_json::json!({
            "servers": [ { "name": "mock", "command": "sh", "args": [script] } ]
        });
        std::fs::write(&path, cfg.to_string()).expect("write mcp config");
        path
    }

    /// `register_mcp_tools` must actually register the tools of an explicit
    /// config file (kills the whole-body `()` mutant) and must not bail out on
    /// a path that exists (kills the `!path.exists()` guard mutant).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_mcp_tools_registers_tools_from_an_explicit_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = mock_mcp_server_script(tmp.path());
        let config = explicit_mcp_config(tmp.path(), &script);
        let mut registry = ToolRegistry::local();
        register_mcp_tools(&mut registry, tmp.path(), Some(config), None).await;
        assert!(
            registry.find_by_name("mcp__mock__alpha").is_some(),
            "the explicit config's `alpha` tool must be registered"
        );
        assert!(
            registry.find_by_name("mcp__mock__beta").is_some(),
            "the explicit config's `beta` tool must be registered"
        );
    }

    /// Issues #70 / #65: `finish_tool_surface` + `apply_operator_allow_list`
    /// are the tail every agent-loop channel must run. Registration happens
    /// BEFORE the allow-list filter, so an operator can allow-list an MCP
    /// tool by name — and everything not listed (base or MCP) is dropped.
    /// The touched-files collector must be attached for checkpoint recording.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_tool_surface_registers_mcp_then_applies_allow_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = mock_mcp_server_script(tmp.path());
        let mcp_config = explicit_mcp_config(tmp.path(), &script);
        let mut cfg = test_config();
        cfg.workspace = tmp.path().to_path_buf();
        cfg.allow_tools = vec!["Read".into(), "mcp__mock__alpha".into()];

        let (tools, _) = build_tools(&cfg, None).await;
        let mut tools = finish_tool_surface(tools, &cfg, Some(mcp_config), None).await;
        apply_operator_allow_list(&mut tools, &cfg);

        assert!(
            tools.find_by_name("mcp__mock__alpha").is_some(),
            "allow-listed MCP tool must survive (issue #70)"
        );
        assert!(
            tools.find_by_name("mcp__mock__beta").is_none(),
            "MCP tools outside the allow-list must be dropped (issue #65)"
        );
        assert!(tools.find_by_name("Read").is_some());
        assert!(
            tools.find_by_name("Bash").is_none(),
            "base tools outside the allow-list must be dropped (issue #65)"
        );
        assert!(
            tools.touched_files().is_some(),
            "the checkpoint touched-files collector must be attached"
        );
    }

    /// Without an allow-list (and outside coordinator mode) the surface is
    /// only added to — MCP registers, nothing is pruned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_tool_surface_keeps_full_registry_without_filters() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = test_config();
        cfg.workspace = tmp.path().to_path_buf();

        let (tools, _) = build_tools(&cfg, None).await;
        let tools = finish_tool_surface(tools, &cfg, None, None).await;

        assert!(tools.find_by_name("Bash").is_some());
        assert!(tools.find_by_name("Read").is_some());
        assert!(tools.touched_files().is_some());
    }

    /// `register_mcp_server_tools` must report the real number of tools the
    /// server listed (kills the `Ok(0)` / `Ok(1)` return mutants).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_mcp_server_tools_reports_the_real_tool_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = mock_mcp_server_script(tmp.path());
        let server = McpServer {
            name: "mock".into(),
            command: "sh".into(),
            args: vec![script],
            url: None,
            env: None,
            headers: None,
            transport: None,
        };
        let mut registry = ToolRegistry::local();
        let count = register_mcp_server_tools(&mut registry, &server, None)
            .await
            .expect("spawn + list must succeed against the mock server");
        assert_eq!(count, 2, "must report the two tools the server listed");
    }

    /// The auto-discovery log line is emitted only for a non-empty result;
    /// the `!servers.is_empty()` guard mutant silences the message.
    #[test]
    fn auto_discovered_message_is_emitted_only_when_servers_exist() {
        assert!(auto_discovered_message(&[]).is_none());
        let server = McpServer {
            name: "mock".into(),
            command: "true".into(),
            args: Vec::new(),
            url: None,
            env: None,
            headers: None,
            transport: None,
        };
        let msg = auto_discovered_message(std::slice::from_ref(&server))
            .expect("a discovered server must produce a log line");
        assert!(msg.contains("auto-discovered 1 server(s)"), "got: {msg}");
    }

    // ── discover_loaded_skills (line 376) ────────────────────────────────

    #[test]
    fn discover_loaded_skills_reads_the_env_skill_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("skills");
        seed_skill(&root, "demo", "demo");
        let _env = EnvGuard::set(&[(
            "RECURSIVE_SKILL_PATHS",
            Some(root.to_str().expect("utf8 path")),
        )]);
        let cfg = test_config();
        let skills = discover_loaded_skills(&cfg);
        assert_eq!(skills.len(), 1, "the env skill path must be discovered");
        assert_eq!(skills[0].name, "demo");
    }

    // ── apply_skill_injection (lines 508/519/521/537) ────────────────────

    fn injected_one(name: &str, body: &str) -> Vec<(String, String)> {
        vec![(name.to_string(), body.to_string())]
    }

    /// The block is appended for a non-empty selection and skipped entirely for
    /// an empty one (kills the `!injected.is_empty()` guard mutants in both
    /// directions).
    #[test]
    fn apply_skill_injection_appends_a_block_and_is_a_noop_without_skills() {
        let out = apply_skill_injection("BASE".to_string(), &injected_one("demo", "hello"));
        // Byte-identical to the pre-refactor `format!("{}\n\n{}", …)` join: one
        // blank line between the assembled prompt and the injected block.
        assert!(
            out.starts_with("BASE\n\n=== Skill: demo (auto-loaded) ==="),
            "join separator drifted: {out:?}"
        );
        assert!(out.contains("=== Skill: demo (auto-loaded) ==="));
        assert!(out.contains("hello"));
        assert_eq!(
            apply_skill_injection("BASE".to_string(), &[]),
            "BASE",
            "no skills means the prompt is left untouched"
        );
    }

    /// An oversized block is truncated with the prefix+ellipsis form (kills the
    /// `+`→`*`, `>`→`<`, `>`→`==` mutants of the budget test and the
    /// `remaining > 20`→`<`/`==` mutants).
    #[test]
    fn apply_skill_injection_truncates_when_the_budget_is_exceeded() {
        let big = "x".repeat(20_000);
        let out = apply_skill_injection("BASE".to_string(), &injected_one("demo", &big));
        assert!(
            out.contains("[truncated]"),
            "oversized input must be truncated"
        );
        assert!(
            out.contains("..."),
            "the slice-truncation path keeps a prefix and an ellipsis"
        );
        assert!(
            !out.contains(&"x".repeat(9_000)),
            "no single block may exceed the 8192-byte budget"
        );
    }

    /// A block that exactly fills the budget is kept whole (kills the
    /// `>`→`>=` mutant of the budget test).
    #[test]
    fn apply_skill_injection_keeps_a_block_that_exactly_fills_the_budget() {
        let name = "s";
        let prefix = format!("=== Skill: {name} (auto-loaded) ===\n");
        let body = "y".repeat(8192 - prefix.len() - 2);
        let out = apply_skill_injection("BASE".to_string(), &injected_one(name, &body));
        assert!(
            out.contains(&body),
            "a block that exactly fills the budget is not truncated"
        );
        assert!(!out.contains("[truncated]"));
    }

    /// With <= 20 bytes of headroom the bare `[truncated]` marker is used
    /// (kills the `remaining > 20`→`>=` mutant).
    #[test]
    fn apply_skill_injection_uses_the_bare_marker_when_little_room_remains() {
        let p1 = "=== Skill: a (auto-loaded) ===\n".len();
        let body1 = "a".repeat(8172 - p1 - 2); // exactly 8172 bytes appended
        let p2 = "=== Skill: b (auto-loaded) ===\n".len();
        let body2 = "b".repeat(100 - p2 - 2); // 100 bytes, over the 20 left
        let injected = vec![("a".to_string(), body1), ("b".to_string(), body2)];
        let out = apply_skill_injection("BASE".to_string(), &injected);
        assert!(out.contains("[truncated]"));
        assert!(
            !out.contains("..."),
            "when <= 20 bytes remain the ellipsis form must not be used"
        );
    }

    /// The running total accumulates across blocks, so the second block is
    /// truncated once the first has eaten the budget (kills the `+=`→`*=` /
    /// `-=` mutants).
    #[test]
    fn apply_skill_injection_accumulates_the_running_total() {
        let p1 = "=== Skill: a (auto-loaded) ===\n".len();
        let body1 = "a".repeat(8000 - p1 - 2); // 8000 bytes, appended whole
        let injected = vec![("a".to_string(), body1), ("b".to_string(), "b".repeat(500))];
        let out = apply_skill_injection("BASE".to_string(), &injected);
        assert!(
            out.contains("[truncated]"),
            "the second block must be truncated once the first fills the budget"
        );
    }

    // ── build_runtime (lines 433/568) ────────────────────────────────────

    /// A non-empty seed is installed on the runtime transcript (kills the
    /// `!seed.is_empty()` guard mutant).
    #[tokio::test]
    async fn build_runtime_seeds_the_transcript_from_seed_messages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = test_config();
        cfg.workspace = tmp.path().to_path_buf();
        let seed = vec![recursive::message::Message::user("seeded hello")];
        let runtime = build_runtime(
            &cfg, None, seed, false, None, false, None, None, None, None, false,
        )
        .await
        .expect("build_runtime must succeed with a valid config");
        assert!(
            runtime
                .transcript()
                .iter()
                .any(|m| m.content == "seeded hello"),
            "a non-empty seed must be installed on the runtime transcript"
        );
    }

    /// The `"anthropic"` match arm constructs an `AnthropicProvider`, which
    /// POSTs to `/v1/messages` (vs the OpenAI arm's `/chat/completions`).
    /// Deleting the arm silently downgrades an Anthropic config to OpenAI, so
    /// this drives one real turn against a loopback server and pins the path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_runtime_uses_the_anthropic_endpoint_for_an_anthropic_config() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let captured = Arc::new(Mutex::new(String::new()));
        let sink = captured.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                if let Ok(n) = sock.read(&mut buf).await {
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let request_line = req.lines().next().unwrap_or_default().to_string();
                    *sink.lock().unwrap_or_else(|e| e.into_inner()) = request_line;
                }
                // Close without a response: the request path is what matters.
            }
        });

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = test_config();
        cfg.workspace = tmp.path().to_path_buf();
        cfg.provider_type = "anthropic".to_string();
        cfg.api_base = format!("http://{addr}");
        cfg.retry_max = 0;
        let mut runtime = build_runtime(
            &cfg,
            None,
            Vec::new(),
            false,
            None,
            false,
            None,
            None,
            None,
            None,
            false,
        )
        .await
        .expect("build_runtime must build an anthropic provider");
        // The turn errors out (the mock drops the connection); we only care that
        // it actually reached the Anthropic endpoint.
        let _ = runtime.run("hello").await;
        let _ = server.await;
        let request_line = captured.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            request_line.contains("/v1/messages"),
            "an anthropic config must POST to /v1/messages, got {request_line:?}"
        );
    }
}
