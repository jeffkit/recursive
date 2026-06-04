//! Build helpers: tool registry, agent runtime, MCP registration, skill discovery.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use recursive::config::{load_project_context, Config};
use recursive::mcp::{discover_mcp_servers, load_mcp_config, McpClient, McpServer, McpTool};
use recursive::skills::{discover_skills, skill_index, skills_for_injection, Skill};
use recursive::{
    llm::{AnthropicProvider, LlmProvider, OpenAiProvider},
    tools::EpisodicRecall,
    tools::{
        BackgroundJobManager, CheckBackground, EstimateTokens, Forget, GlobTool, LoadSkill,
        LocalTransport, ReadFile, Recall, Remember, RunBackground, RunShell, RunSkillScript,
        ScratchpadDelete, ScratchpadGet, ScratchpadList, SearchFiles, SpawnWorkerTool, SubAgent,
        TodoWriteTool, ToolTransport, WebFetch, WorkingMemoryTool, WriteFile,
    },
    tools::{ForgetFact, RecallFact, RememberFact, UpdateFact},
    AgentRuntime, AgentRuntimeBuilder, EventSink, NullSink, PlanningMode, RetryPolicy,
    ToolRegistry,
};

/// Build the tool registry, optionally registering MCP tools from a config file.
pub(crate) async fn build_tools(config: &Config) -> ToolRegistry {
    let root = &config.workspace;
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let bg_manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
    let mut registry = ToolRegistry::new(transport)
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(GlobTool::new(root)))
        .register(Arc::new(
            RunShell::new(root).with_timeout(Duration::from_secs(config.shell_timeout_secs)),
        ))
        .register(Arc::new(SearchFiles::new(root)))
        .register(Arc::new(WebFetch::new()))
        .register(Arc::new(RunBackground::new(root, bg_manager.clone())))
        .register(Arc::new(CheckBackground::new(bg_manager.clone())));
    registry = registry.register(Arc::new(EstimateTokens::new(root)));
    registry = registry
        .register(Arc::new(Remember::new(root)))
        .register(Arc::new(Recall::new(root)))
        .register(Arc::new(Forget::new(root)));
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
        registry = registry.register(Arc::new(LoadSkill::new(skills.clone())));
        registry = registry.register(Arc::new(RunSkillScript::new(
            skills,
            root.clone(),
            Duration::from_secs(config.shell_timeout_secs),
        )));
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
    registry
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
        if !path.is_empty() {
            match std::fs::read_to_string(&path) {
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
    }
    let file_config = recursive::config_file::FileConfig::load().ok().flatten()?;
    let section = file_config.permissions?;
    let mode = section.mode.unwrap_or_default();
    let mut layers = Vec::new();
    if !section.allow.is_empty() || !section.deny.is_empty() || !section.interactive.is_empty() {
        layers.push(recursive::permissions::PermissionLayer {
            source: recursive::permissions::RuleSource::User,
            allow: section.allow,
            deny: section.deny,
            interactive: section.interactive,
        });
    }
    Some(recursive::permissions::LayeredPermissionsConfig { mode, layers })
}

/// Register MCP tools from a config file into the registry.
pub(crate) async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    workspace: &Path,
    mcp_config_path: Option<PathBuf>,
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
                if !s.is_empty() {
                    eprintln!("mcp: auto-discovered {} server(s) from workspace", s.len());
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
        match register_mcp_server_tools(registry, server).await {
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
) -> anyhow::Result<usize> {
    let mut client = McpClient::spawn(server).await?;
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
/// Defaults: <workspace>/.recursive/skills/, ~/.recursive/skills/.
/// Override with RECURSIVE_SKILL_PATHS=path1:path2 (colon-separated).
fn discover_loaded_skills(config: &Config) -> Vec<Skill> {
    let paths: Vec<PathBuf> = if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
        env_paths.split(':').map(PathBuf::from).collect()
    } else {
        let mut defaults = vec![config.workspace.join(".recursive").join("skills")];
        if let Some(home) = std::env::var_os("HOME") {
            defaults.push(PathBuf::from(home).join(".recursive").join("skills"));
        }
        defaults
    };
    discover_skills(&paths)
}

/// Build an [`AgentRuntime`], optionally registering MCP tools from a config file.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_runtime(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    goal: Option<&str>,
    event_sink: Option<Arc<dyn EventSink>>,
    shutdown_token: Option<tokio_util::sync::CancellationToken>,
) -> anyhow::Result<AgentRuntime> {
    let api_key = config.require_api_key()?;
    let provider_type = &config.provider_type;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn LlmProvider> = match provider_type.as_str() {
        "anthropic" => {
            let anthropic_retry = recursive::llm::RetryPolicy {
                max_retries: config.retry_max,
                initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
            };
            let anthropic = AnthropicProvider::new(&config.api_base, api_key, &config.model)
                .with_temperature(config.temperature)
                .with_retry_policy(anthropic_retry);
            Arc::new(anthropic)
        }
        _ => {
            let openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)
                .with_temperature(config.temperature)
                .with_retry_policy(retry);
            Arc::new(openai)
        }
    };
    let mut tools = build_tools(config).await;
    register_mcp_tools(&mut tools, &config.workspace, mcp_config).await;

    // Always attach a TouchedFiles collector so AgentRuntime can record
    // per-turn file touches when checkpoints are enabled later via
    // enable_checkpoints(). When checkpoints are disabled this is a
    // no-op observer.
    tools = tools.with_touched_files(Arc::new(std::sync::Mutex::new(
        recursive::TouchedFiles::new(),
    )));

    let sub_agent_enabled = std::env::var("RECURSIVE_SUBAGENT_ENABLED").as_deref() == Ok("1");
    if sub_agent_enabled {
        let max_depth: usize = std::env::var("RECURSIVE_SUBAGENT_MAX_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);
        let sub = SubAgent::new(
            &config.workspace,
            provider.clone(),
            tools.clone(),
            max_depth,
            0,
            None,
        );
        tools = tools.register(Arc::new(sub));

        // Also register spawn_worker — the coordinator-pattern first-class
        // delegation tool.  Uses the same depth limit as sub_agent.
        let worker = SpawnWorkerTool::new(
            &config.workspace,
            provider.clone(),
            tools.clone(),
            max_depth,
            0,
            None,
        );
        tools = tools.register(Arc::new(worker));
    }

    let skills = discover_loaded_skills(config);
    let project_context = load_project_context(&config.workspace);
    let mut system_prompt = match (&project_context, skills.is_empty()) {
        (Some(ctx), true) => {
            format!(
                "# Project context (AGENTS.md)\n\n{}\n\n---\n\n{}",
                ctx, config.system_prompt
            )
        }
        (Some(ctx), false) => {
            format!(
                "# Project context (AGENTS.md)\n\n{}\n\n---\n\n{}\n{}",
                ctx,
                config.system_prompt,
                skill_index(&skills)
            )
        }
        (None, true) => config.system_prompt.clone(),
        (None, false) => format!("{}\n{}", config.system_prompt, skill_index(&skills)),
    };
    let injected = skills_for_injection(&skills, goal.unwrap_or(""));
    if !injected.is_empty() {
        let mut injection_block = String::new();
        let mut total_chars = 0usize;
        let max_injection_chars = 8192usize;
        for (name, body) in &injected {
            let snippet = format!(
                "=== Skill: {name} (auto-loaded) ===
{body}

"
            );
            if total_chars + snippet.len() > max_injection_chars {
                let remaining = max_injection_chars.saturating_sub(total_chars);
                let truncated = if remaining > 20 {
                    format!(
                        "{}...
[truncated]
",
                        &snippet[..remaining.saturating_sub(20)]
                    )
                } else {
                    "[truncated]
"
                    .to_string()
                };
                injection_block.push_str(&truncated);
                break;
            }
            injection_block.push_str(&snippet);
            total_chars += snippet.len();
        }
        system_prompt = format!(
            "{}

{}",
            system_prompt, injection_block
        );
    }
    let system_prompt = if sub_agent_enabled {
        format!(
            "{}\n\nWhen you need to do focused research or scan files without polluting your main context, use the `sub_agent` tool. It spawns a fresh agent with its own transcript and a restricted tool set (read-only by default).",
            system_prompt
        )
    } else {
        system_prompt
    };

    let mut builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .max_steps(config.max_steps)
        .streaming(stream);
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if let Some(token) = shutdown_token {
        builder = builder.shutdown_token(token);
    }
    if !seed.is_empty() {
        builder = builder.seed_transcript(seed);
    }
    // Determine the compaction threshold (chars):
    //   RECURSIVE_COMPACT_THRESHOLD=<n>  → explicit override (0 = disabled)
    //   RECURSIVE_COMPACT_THRESHOLD unset → auto-compute from model context window
    //   RECURSIVE_COMPACT_THRESHOLD=0    → explicitly disabled
    let compact_threshold: Option<usize> =
        match std::env::var("RECURSIVE_COMPACT_THRESHOLD").as_deref() {
            Ok("0") | Ok("off") | Ok("false") => None, // explicitly disabled
            Ok(s) => s.parse::<usize>().ok().filter(|&n| n > 0),
            Err(_) => {
                // Auto-compute: mirrors fake-cc's getAutoCompactThreshold.
                Some(recursive::llm::default_compact_threshold_chars(
                    &config.model,
                ))
            }
        };
    if let Some(n) = compact_threshold {
        builder = builder.compactor(recursive::Compactor::new(n));
    }
    if hook_timing {
        use recursive::hooks::HookRegistry;
        let mut hooks = HookRegistry::new();
        hooks.register(Arc::new(recursive::hooks::ToolTimingHook::new()));
        builder = builder.hooks(hooks);
    }
    if plan_first {
        builder = builder.planning_mode(PlanningMode::PlanFirst);
    }
    if let Some(sink) = event_sink {
        builder = builder.event_sink(sink);
    }
    builder.build().map_err(Into::into)
}
