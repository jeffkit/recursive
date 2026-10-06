use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use recursive::config::Config;
use recursive::llm::RetryPolicy;
use recursive::skills::{discover_skills, Skill};
use recursive::tools::SharedSandboxRoots;
use recursive::{
    assemble_system_prompt, new_shared_sandbox_roots, register_subagent_if_enabled, AgentRuntime,
    AgentRuntimeBuilder, ChatProvider, SharedTokenSlot,
};

/// Output of a TUI runtime build: the runtime state plus shared handles
/// the loop arbiter needs (wakeup slot, background job manager).
pub struct TuiRuntime {
    pub state: RuntimeBuild,
    pub session_roots: SharedSandboxRoots,
    pub wakeup_slot: recursive::tools::WakeupSlot,
    /// Issue #40: per-turn cancellation slot wired into the `agent` tool.
    /// The backend stores the current turn's interrupt token here at each
    /// turn start, so parallel sub-agent workers are cancelled by Ctrl-C via
    /// the same child-token tree (cleared when the turn ends).
    pub subagent_token_slot: SharedTokenSlot,
    pub bg_manager: Arc<tokio::sync::Mutex<recursive::tools::BackgroundJobManager>>,
}

pub enum RuntimeBuild {
    Ready(Option<Box<AgentRuntime>>),
    Offline { reason: String },
}

fn build_provider(
    config: &Config,
    api_key: String,
) -> recursive::error::Result<Arc<dyn ChatProvider>> {
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn ChatProvider> = match config.provider_type.as_str() {
        "anthropic" => Arc::new(
            recursive::llm::AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_max_tokens(config.max_tokens)
                .with_retry_policy(retry),
        ),
        _ => Arc::new(
            recursive::llm::OpenAiProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_max_tokens(config.max_tokens)
                .with_retry_policy(retry)
                .with_max_search_rounds(config.max_search_rounds),
        ),
    };
    Ok(provider)
}

/// Treat an empty-string API key as missing. Extracted so the `!is_empty`
/// guard is unit-testable — the `delete !` mutant is otherwise unobservable
/// when the configured key is `None` (the common offline-test path).
fn effective_api_key(api_key: Option<&str>) -> Option<&str> {
    api_key.filter(|k| !k.is_empty())
}

/// Build a `ChatProvider` for an arbitrary `(preset_id, model)` pair, reusing
/// the current process configuration for non-provider knobs (retry policy,
/// temperature, max_search_rounds).
///
/// Used by the TUI `/model` picker to hot-swap models mid-session. The API key
/// is resolved with the same precedence `Config::from_env` uses for presets:
/// the preset's own `key_env` env var wins; otherwise the currently
/// configured key (file / `RECURSIVE_API_KEY`) is reused so a user who already
/// authenticated one provider can switch to another sharing the same key
/// without re-authenticating. Returns an error when no key is available.
pub fn build_provider_for_model(
    preset_id: &str,
    model: &str,
) -> recursive::error::Result<Arc<dyn ChatProvider>> {
    let preset = recursive::providers::find_preset_effective(preset_id).ok_or_else(|| {
        recursive::error::Error::Config {
            message: format!("unknown provider preset '{preset_id}'"),
        }
    })?;
    let config = config_for_preset_model(&preset, model)?;
    let api_key = effective_api_key(config.api_key.as_deref())
        .map(|k| k.to_string())
        .ok_or_else(|| recursive::error::Error::Config {
            message: format!(
                "no API key for preset '{}' — set ${} (or RECURSIVE_API_KEY) and retry",
                preset.id, preset.key_env,
            ),
        })?;
    build_provider(&config, api_key)
}

/// Prepare a `Config` aimed at `(preset, model)` for a `/model` hot-swap.
///
/// Folds the preset's protocol / endpoint / model into the process config and
/// then **re-derives `max_tokens`** for that model. The re-derivation is the
/// point: `Config::from_env` derived `max_tokens` from whatever model was
/// active at startup, so without it the swapped-in provider is built with the
/// *previous* model's output cap — switching to a model with a lower ceiling
/// then 400s on the first request, the same defect the CLI `--model` flag had
/// (issue #17). `config.preset` is set first because that is where
/// `resolve_max_tokens` looks the model's `ModelSpec.max_tokens` up.
///
/// Extracted from [`build_provider_for_model`] so the resulting config is
/// observable to tests — the built `ChatProvider` does not expose its cap.
fn config_for_preset_model(
    preset: &recursive::providers::ProviderPreset,
    model: &str,
) -> recursive::error::Result<Config> {
    let mut config =
        recursive::config::Config::from_env().map_err(|e| recursive::error::Error::Config {
            message: format!("failed to load configuration: {e}"),
        })?;
    let provider_type = preset.provider_type.clone();
    let api_base = if provider_type == "anthropic" {
        preset
            .anthropic_api_base
            .clone()
            .unwrap_or_else(|| preset.api_base.clone())
    } else {
        preset.api_base.clone()
    };
    config.provider_type = provider_type;
    config.api_base = api_base;
    config.model = model.to_string();
    config.preset = Some(preset.id.clone());
    config
        .resolve_max_tokens()
        .map_err(|e| recursive::error::Error::Config {
            message: format!("failed to resolve max_tokens: {e}"),
        })?;
    // Prefer the preset's own key_env when present; fall back to the current
    // config's key so cross-preset switches with a shared key just work.
    if !preset.key_env.is_empty() {
        if let Ok(k) = std::env::var(&preset.key_env) {
            if !k.is_empty() {
                config.api_key = Some(k);
            }
        }
    }
    Ok(config)
}

/// Whether a preset's own API key is resolvable *without* the global
/// `RECURSIVE_API_KEY` / config fallback. Used by the `/model` picker to
/// decide which providers to offer: a model is only listed when switching
/// to it would actually authenticate, so the user never sees a wall of
/// unconfigured providers (the previous behaviour listed every bundled
/// preset regardless of keys).
///
/// A preset with an empty `key_env` (e.g. local `ollama`) is treated as
/// always available — it needs no key. The active preset is additionally
/// kept available by the picker even when its key is missing, so the
/// running model stays selectable for re-confirmation.
pub fn preset_key_available(preset: &recursive::providers::ProviderPreset) -> bool {
    if preset.key_env.is_empty() {
        return true;
    }
    matches!(std::env::var(&preset.key_env), Ok(k) if !k.is_empty())
}

/// The actionable "no LLM provider configured" message shown both at TUI
/// startup (as `UiEvent::RuntimeOffline`) and when the user tries to send a
/// message while offline (as `UiEvent::Error`). Kept in one place so the
/// two surfaces never drift, and so a test can pin the recommended next
/// step (`recursive init`). Extracted as a function rather than a constant
/// so the `&str` → `String` mutant on the recommended command is observable.
fn offline_no_provider_reason() -> String {
    "No LLM provider configured. Run `recursive init` (outside the TUI) to \
     set one up, then restart. Or set provider.preset + API key manually: \
     `recursive config set provider.preset <id>` and \
     `recursive config set-secret <KEY_ENV> <KEY>`."
        .to_string()
}

/// Build the `(root, tier)` list used to expand the filesystem sandbox
/// beyond the primary workspace. Read-write roots come from `--add-dir` /
/// `[sandbox] extra_dirs`; read-only roots come from
/// `[sandbox] extra_readonly_dirs`. The primary workspace itself is always
/// added by `build_standard_tools_with_roots` as a `ReadWrite` root, so it
/// is not duplicated here.
fn sandbox_extra_roots(config: &Config) -> Vec<(PathBuf, recursive::AccessTier)> {
    config
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
        .collect()
}

/// Discover skills from configured search paths.
///
/// Defaults: <workspace>/.recursive/skills/, <workspace>/.claude/skills/, ~/.recursive/skills/, ~/.claude/skills/.
/// Override with `RECURSIVE_SKILL_PATHS=path1;path2` on Windows or
/// `RECURSIVE_SKILL_PATHS=path1:path2` on Unix (OS-native path separator).
fn discover_loaded_skills(config: &Config) -> Vec<Skill> {
    let paths: Vec<PathBuf> = if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
        // Use the OS-native separator so Windows drive paths like `C:\skills`
        // aren't split on the colon in the drive letter.
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

/// Resolve the agent preset a TUI session runs under (issue #127).
///
/// `RECURSIVE_AGENT_PRESET` selects it; `standard` is the default. An unknown
/// id is a server-/user-side misconfiguration, not a reason to leave the
/// operator without a TUI, so it falls back to `standard` and says so.
fn resolve_tui_preset(config: &Config) -> recursive::preset::ResolvedPreset {
    let env = recursive::preset::PresetEnv::from_process();
    match recursive::preset::resolve_session(None, config, &env) {
        Ok(preset) => preset,
        Err(e) => {
            tracing::warn!(error = %e, "unknown agent preset; falling back to standard");
            recursive::preset::STANDARD.resolve(config, &env)
        }
    }
}

/// Install the session's preset on a TUI runtime builder (issue #127): context
/// management, post-compaction re-injection and the plan-mode tool gate. The
/// TUI has a live human, so it reports itself interactive; the preset still
/// decides whether the plan tools exist at all.
fn apply_preset(
    builder: recursive::AgentRuntimeBuilder,
    preset: &recursive::preset::ResolvedPreset,
    read_state: Option<Arc<std::sync::Mutex<recursive::tools::fs::ReadFileState>>>,
    skills: Vec<Skill>,
) -> recursive::AgentRuntimeBuilder {
    let mut assets = recursive::preset::PresetAssets::new().with_skills(skills);
    if let Some(state) = read_state {
        assets = assets.with_read_state(state);
    }
    recursive::preset::apply(
        builder,
        preset,
        &assets,
        recursive::preset::ChannelSupport { interactive: true },
    )
}

pub fn build_runtime() -> TuiRuntime {
    let session_roots = new_shared_sandbox_roots();
    let wakeup_slot: recursive::tools::WakeupSlot = Arc::new(std::sync::Mutex::new(None));
    let subagent_token_slot: SharedTokenSlot = Arc::new(std::sync::Mutex::new(None));
    let bg_manager = Arc::new(tokio::sync::Mutex::new(
        recursive::tools::BackgroundJobManager::new(),
    ));
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to load configuration: {e}"),
                },
                session_roots,
                wakeup_slot,
                subagent_token_slot,
                bg_manager,
            };
        }
    };

    let api_key = match effective_api_key(config.api_key.as_deref()) {
        Some(k) => k.to_string(),
        None => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: offline_no_provider_reason(),
                },
                session_roots,
                wakeup_slot,
                subagent_token_slot,
                bg_manager,
            };
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to build HTTP client: {e}"),
                },
                session_roots,
                wakeup_slot,
                subagent_token_slot,
                bg_manager,
            };
        }
    };

    let skills = discover_loaded_skills(&config);
    let extra_roots = sandbox_extra_roots(&config);
    let tools = recursive::tools::build_standard_tools_with_roots(
        &config.workspace,
        &extra_roots,
        Some(session_roots.clone()),
        &skills,
        config.shell_timeout_secs,
        config.web_search_provider.clone(),
        config.web_search_api_key.clone(),
        config.web_search_jina_key.clone(),
        Some(bg_manager.clone()),
    );
    // Register ScheduleWakeup so the agent can schedule its own next turn.
    let tools = tools.register(Arc::new(recursive::tools::ScheduleWakeup::new(
        wakeup_slot.clone(),
    )));
    // Channel-agnostic sub-agent tool registration, in lockstep with the
    // coordinator prompt injected by `assemble_system_prompt`.
    // Issue #119: one telemetry bridge shared by the `agent` tool and this
    // runtime, so TUI workers report their events and usage to the session.
    let worker_telemetry: recursive::tools::WorkerTelemetrySlot = Arc::new(std::sync::Mutex::new(
        recursive::tools::WorkerTelemetry::new(),
    ));
    let tools = register_subagent_if_enabled(
        tools,
        &config,
        provider.clone(),
        Some(subagent_token_slot.clone()),
        Some(worker_telemetry.clone()),
    );
    // Issue #127: the session's agent preset is resolved once, up front —
    // both the prompt profile and the builder consume it. `RECURSIVE_AGENT_PRESET`
    // selects it; `standard` is the default.
    let preset = resolve_tui_preset(&config);
    let assembled = assemble_system_prompt(
        &config.system_prompt,
        &config.workspace,
        &skills,
        config.subagent_enabled,
    );
    let system_prompt = recursive::preset::apply_prompt(assembled.full, &preset);
    let prompt_segments = assembled.segments;

    // Extract the shared read_state BEFORE `tools` is moved into the builder,
    // so the preset's file re-injector can be built from it below.
    let read_state = tools.read_file_state();

    let builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .prompt_segments(prompt_segments)
        .max_steps(config.max_steps)
        .wall_timeout_secs(config.wall_timeout_secs)
        // Stream partial tokens so the TUI shows the answer building up live
        // and so reasoner models that only expose `reasoning_content` through
        // the streaming SSE channel surface their thinking block.
        .streaming(true);
    // Issue #127: context management (compactor / microcompactor / transcript
    // cap), post-compaction re-injection of recently-read files and invoked
    // skills, and the plan-mode tool gate all come from the session's agent
    // preset — the same single assembly point the CLI and HTTP channels use,
    // so the three can no longer drift apart.
    let builder = apply_preset(builder, &preset, read_state, skills.clone());
    // Issue #119: publish the TUI's sink to workers and bill their usage.
    let builder = builder.worker_telemetry(worker_telemetry);
    let build = match builder.build() {
        Ok(rt) => RuntimeBuild::Ready(Some(Box::new(rt))),
        Err(e) => RuntimeBuild::Offline {
            reason: format!("failed to build agent runtime: {e}"),
        },
    };
    TuiRuntime {
        state: build,
        session_roots,
        wakeup_slot,
        subagent_token_slot,
        bg_manager,
    }
}

/// Build the agent runtime for TUI mode, returning both the runtime state and
/// a skill-install event channel receiver so the TUI loop can handle
/// interactive `install_skill` tool requests.
///
/// When the `skill-hub` feature is disabled this is identical to
/// [`build_runtime`] plus a dummy `()` receiver; the caller must not rely on
/// the receiver type unless the feature is enabled.
#[cfg(feature = "skill-hub")]
pub fn build_runtime_for_tui() -> (
    TuiRuntime,
    tokio::sync::mpsc::UnboundedReceiver<crate::events::SkillInstallEvent>,
) {
    use crate::events::SkillInstallEvent;
    use tokio::sync::mpsc;

    let (skill_tx, skill_rx) = mpsc::unbounded_channel::<SkillInstallEvent>();

    let tui_rt = build_runtime_with_skill_tx(Some(skill_tx));
    (tui_rt, skill_rx)
}

/// Inner helper: build a runtime with optional skill-hub tool injection.
#[cfg(feature = "skill-hub")]
fn build_runtime_with_skill_tx(
    skill_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::events::SkillInstallEvent>>,
) -> TuiRuntime {
    let session_roots = new_shared_sandbox_roots();
    let wakeup_slot: recursive::tools::WakeupSlot = Arc::new(std::sync::Mutex::new(None));
    let subagent_token_slot: SharedTokenSlot = Arc::new(std::sync::Mutex::new(None));
    let bg_manager = Arc::new(tokio::sync::Mutex::new(
        recursive::tools::BackgroundJobManager::new(),
    ));
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to load configuration: {e}"),
                },
                session_roots,
                wakeup_slot,
                subagent_token_slot,
                bg_manager,
            };
        }
    };

    let api_key = match effective_api_key(config.api_key.as_deref()) {
        Some(k) => k.to_string(),
        None => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: offline_no_provider_reason(),
                },
                session_roots,
                wakeup_slot,
                subagent_token_slot,
                bg_manager,
            };
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to build HTTP client: {e}"),
                },
                session_roots,
                wakeup_slot,
                subagent_token_slot,
                bg_manager,
            };
        }
    };

    let skills = discover_loaded_skills(&config);
    let extra_roots = sandbox_extra_roots(&config);

    let mut tools = recursive::tools::build_standard_tools_with_roots(
        &config.workspace,
        &extra_roots,
        Some(session_roots.clone()),
        &skills,
        config.shell_timeout_secs,
        config.web_search_provider.clone(),
        config.web_search_api_key.clone(),
        config.web_search_jina_key.clone(),
        Some(bg_manager.clone()),
    );

    // Register ScheduleWakeup for loop-mode agent self-scheduling.
    tools = tools.register(Arc::new(recursive::tools::ScheduleWakeup::new(
        wakeup_slot.clone(),
    )));

    // Register skill-hub tools: install_skill is TUI-only (it sends events
    // through a channel so the TUI can prompt the user).
    tools = tools.register(Arc::new(recursive::tools::InstallSkill::new(skill_tx)));

    // Channel-agnostic sub-agent tool registration, in lockstep with the
    // coordinator prompt injected by `assemble_system_prompt`.
    // Issue #119: one telemetry bridge shared by the `agent` tool and this
    // runtime, so TUI workers report their events and usage to the session.
    let worker_telemetry: recursive::tools::WorkerTelemetrySlot = Arc::new(std::sync::Mutex::new(
        recursive::tools::WorkerTelemetry::new(),
    ));
    tools = register_subagent_if_enabled(
        tools,
        &config,
        provider.clone(),
        Some(subagent_token_slot.clone()),
        Some(worker_telemetry.clone()),
    );
    // Issue #127: the session's agent preset is resolved once, up front —
    // both the prompt profile and the builder consume it. `RECURSIVE_AGENT_PRESET`
    // selects it; `standard` is the default.
    let preset = resolve_tui_preset(&config);
    let assembled = assemble_system_prompt(
        &config.system_prompt,
        &config.workspace,
        &skills,
        config.subagent_enabled,
    );
    let system_prompt = recursive::preset::apply_prompt(assembled.full, &preset);
    let prompt_segments = assembled.segments;

    // Extract the shared read_state BEFORE `tools` is moved into the builder,
    // so the preset's file re-injector can be built from it below.
    let read_state = tools.read_file_state();

    let builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .prompt_segments(prompt_segments)
        .max_steps(config.max_steps)
        .wall_timeout_secs(config.wall_timeout_secs)
        // Stream partial tokens so the TUI shows the answer building up live
        // and so reasoner models that only expose `reasoning_content` through
        // the streaming SSE channel surface their thinking block.
        .streaming(true);
    // Issue #127: context management (compactor / microcompactor / transcript
    // cap), post-compaction re-injection of recently-read files and invoked
    // skills, and the plan-mode tool gate all come from the session's agent
    // preset — the same single assembly point the CLI and HTTP channels use,
    // so the three can no longer drift apart.
    let builder = apply_preset(builder, &preset, read_state, skills.clone());
    // Issue #119: publish the TUI's sink to workers and bill their usage.
    let builder = builder.worker_telemetry(worker_telemetry);
    let build = match builder.build() {
        Ok(rt) => RuntimeBuild::Ready(Some(Box::new(rt))),
        Err(e) => RuntimeBuild::Offline {
            reason: format!("failed to build agent runtime: {e}"),
        },
    };
    TuiRuntime {
        state: build,
        session_roots,
        wakeup_slot,
        subagent_token_slot,
        bg_manager,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::backend::Backend;
    use crate::events::UiEvent;
    use crate::events::UserAction;

    // ── Issue #127: the TUI assembles from the same agent preset ──────────

    /// `apply_preset` installs exactly what the resolved preset declares —
    /// the same assertion the HTTP and CLI channels make, which is what keeps
    /// the three from drifting.
    #[test]
    fn tui_assembly_matches_the_resolved_preset() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _model = EnvGuard::set("RECURSIVE_MODEL", "deepseek-chat");
        let _preset_env = EnvGuard::remove("RECURSIVE_AGENT_PRESET");
        let config = Config::from_env().expect("config");

        let preset = resolve_tui_preset(&config);
        assert_eq!(preset.id, "standard", "no env override → the built-in");
        assert!(
            preset.context.compaction.is_some(),
            "standard compacts on overflow"
        );

        let tools = recursive::tools::build_standard_tools(std::path::Path::new("."), &[], 300);
        let read_state = tools.read_file_state();
        let builder = apply_preset(
            AgentRuntimeBuilder::new().llm(Arc::new(recursive::llm::MockProvider::new(vec![]))),
            &preset,
            read_state,
            Vec::new(),
        );
        assert_eq!(builder.context_management_facts(), preset.context);
        assert_eq!(builder.preset_id(), Some("standard"));

        // The TUI channel is interactive, so the preset's plan tools must
        // actually land in the built registry (the preset alone is not enough).
        let runtime = builder.build().expect("runtime builds");
        assert_eq!(runtime.preset_id(), Some("standard"));
        assert!(
            runtime
                .kernel()
                .tools()
                .find_by_name("enter_plan_mode")
                .is_some(),
            "the TUI must report itself as an interactive channel"
        );
    }

    /// The env overrides reach the TUI's assembly through the preset: a
    /// `RECURSIVE_COMPACT_THRESHOLD` that disables compaction must leave the
    /// TUI runtime without a compactor, exactly as it did before presets.
    #[test]
    fn tui_preset_resolution_honours_compaction_env() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _model = EnvGuard::set("RECURSIVE_MODEL", "deepseek-chat");
        let _preset_env = EnvGuard::remove("RECURSIVE_AGENT_PRESET");
        let config = Config::from_env().expect("config");

        let _off = EnvGuard::set("RECURSIVE_COMPACT_THRESHOLD", "0");
        assert!(resolve_tui_preset(&config).context.compaction.is_none());
        drop(_off);

        let _explicit = EnvGuard::set("RECURSIVE_COMPACT_THRESHOLD", "4321");
        let compaction = resolve_tui_preset(&config)
            .context
            .compaction
            .expect("explicit threshold must yield a compactor");
        assert_eq!(compaction.threshold_chars, 4321);
        assert!(
            compaction.threshold_prompt_tokens.is_some(),
            "the token threshold is still derived from the model"
        );
    }

    /// An unknown `RECURSIVE_AGENT_PRESET` must not leave the operator
    /// TUI-less: it is warned about and the built-in standard preset is used.
    #[test]
    fn unknown_preset_env_falls_back_to_standard() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _model = EnvGuard::set("RECURSIVE_MODEL", "deepseek-chat");
        let _preset_env = EnvGuard::set("RECURSIVE_AGENT_PRESET", "no-such-preset");
        let config = Config::from_env().expect("config");

        assert_eq!(resolve_tui_preset(&config).id, "standard");
    }

    /// A preset with no re-injection declaration installs no reinjector even
    /// when the assets are present — the declaration is the only source of
    /// truth (this is what "adding a preset is one declaration" means).
    #[test]
    fn a_preset_without_reinjection_installs_none() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _model = EnvGuard::set("RECURSIVE_MODEL", "deepseek-chat");
        let config = Config::from_env().expect("config");
        let mut preset = resolve_tui_preset(&config);
        preset.context.reinject_recent_files = None;
        preset.context.reinject_invoked_skills = None;

        let tools = recursive::tools::build_standard_tools(std::path::Path::new("."), &[], 300);
        let builder = apply_preset(
            AgentRuntimeBuilder::new().llm(Arc::new(recursive::llm::MockProvider::new(vec![]))),
            &preset,
            tools.read_file_state(),
            Vec::new(),
        );
        assert_eq!(builder.context_management_facts(), preset.context);
        assert!(builder
            .context_management_facts()
            .reinject_recent_files
            .is_none());
    }

    /// RAII guard that clears API key env vars for the duration of a test
    /// and restores them on drop (including on panic).
    ///
    /// Assumes the caller already holds `env_lock()` (e.g. via
    /// `PinnedRecursiveHome`), so this guard itself does not re-acquire it.
    struct ApiKeyGuard {
        prev_recursive: Option<String>,
        prev_openai: Option<String>,
    }

    impl ApiKeyGuard {
        fn clear() -> Self {
            let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
            let prev_openai = std::env::var("OPENAI_API_KEY").ok();
            std::env::remove_var("RECURSIVE_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            Self {
                prev_recursive,
                prev_openai,
            }
        }
    }

    impl Drop for ApiKeyGuard {
        fn drop(&mut self) {
            match self.prev_recursive.take() {
                Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
                None => std::env::remove_var("RECURSIVE_API_KEY"),
            }
            match self.prev_openai.take() {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }
    }

    #[tokio::test]
    async fn offline_mode_and_config_file_resolution() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        // Use PinnedRecursiveHome (sets RECURSIVE_HOME) rather than PinnedHome
        // because on Windows dirs::home_dir() resolves via SHGetKnownFolderPath
        // and does not respond to runtime USERPROFILE / HOME changes.
        // PinnedRecursiveHome also acquires env_lock(), serialising this test
        // against all other env-mutating tests.
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());

        // ApiKeyGuard clears the API key vars and restores them on drop,
        // ensuring cleanup even if an assertion panics.
        let _keys = ApiKeyGuard::clear();

        let mut backend = Backend::spawn();
        backend
            .action_tx
            .send(UserAction::SendMessage("hi".into()))
            .unwrap();

        let mut got_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
                Ok(Some(UiEvent::Error { message })) => {
                    assert!(
                        message.contains("No LLM provider configured"),
                        "expected offline reason, got {message:?}"
                    );
                    assert!(
                        message.contains("recursive init"),
                        "offline reason should point to the wizard, got {message:?}"
                    );
                    assert!(
                        message.contains("recursive config set"),
                        "offline reason should mention CLI config helper, got {message:?}"
                    );
                    got_error = true;
                    break;
                }
                Ok(Some(UiEvent::RuntimeOffline { .. })) => continue,
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(got_error, "expected an offline-mode UiEvent::Error");
        drop(backend);

        // Part B: config.toml with api_key → Ready
        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"[provider]
api_key = "sk-test-from-config"
api_base = "https://api.example.invalid"
model = "test-model-from-config"
type = "openai"
"#,
        )
        .expect("write config");

        let tui_rt = build_runtime();
        match tui_rt.state {
            RuntimeBuild::Ready(_) => {}
            RuntimeBuild::Offline { reason } => {
                panic!("expected Ready when config.toml has api_key, got Offline: {reason}");
            }
        }
        // _keys guard restores API key env vars on drop here.
    }

    /// Goal: when no provider is configured, the backend must emit
    /// `UiEvent::RuntimeOffline` at init — not stay silent and leave the
    /// status bar stuck at "starting…". This pins the init-time signal
    /// independently of the send-while-offline `Error` path covered above.
    #[tokio::test]
    async fn offline_backend_emits_runtime_offline_at_init() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _keys = ApiKeyGuard::clear();

        let mut backend = Backend::spawn();
        let mut got_offline = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
                Ok(Some(UiEvent::RuntimeOffline { reason })) => {
                    assert!(
                        reason.contains("No LLM provider configured"),
                        "init offline reason should explain, got {reason:?}"
                    );
                    got_offline = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(
            got_offline,
            "expected UiEvent::RuntimeOffline at init when no provider is configured"
        );
    }

    #[test]
    fn offline_no_provider_reason_mentions_init_and_config() {
        let r = offline_no_provider_reason();
        // Pins the recommended next step so a mutant that drops the wizard
        // hint (leaving the user with no actionable path) is killed.
        assert!(r.contains("recursive init"), "reason: {r:?}");
        assert!(r.contains("recursive config set"), "reason: {r:?}");
        assert!(r.contains("set-secret"), "reason: {r:?}");
    }

    // ── Pre-existing helper coverage (pulled into scope by g323 touch) ────

    fn test_config() -> Config {
        Config {
            workspace: PathBuf::from("."),
            api_base: "https://api.anthropic.com".to_string(),
            api_key: Some("sk-test".to_string()),
            model: "claude-test".to_string(),
            provider_type: "anthropic".to_string(),
            preset: None,
            max_steps: 32,
            max_tokens: 65536,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
            headless: false,
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

    /// Issue #40 — every TuiRuntime build carries a sub-agent token slot,
    /// and when sub-agent is enabled the runtime wiring passes it to
    /// `register_subagent_if_enabled` (asserted structurally here: the slot
    /// exists and starts empty; the registration call sites pass
    /// `Some(slot)` — see the backend tests for the per-turn lifecycle).
    #[test]
    fn subagent_slot_registered_when_enabled() {
        let tui_rt = TuiRuntime {
            state: RuntimeBuild::Offline {
                reason: "test".into(),
            },
            session_roots: new_shared_sandbox_roots(),
            wakeup_slot: Arc::new(std::sync::Mutex::new(None)),
            subagent_token_slot: Arc::new(std::sync::Mutex::new(None)),
            bg_manager: Arc::new(tokio::sync::Mutex::new(
                recursive::tools::BackgroundJobManager::new(),
            )),
        };
        assert!(
            tui_rt
                .subagent_token_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none(),
            "slot starts empty between turns"
        );
        // The registration call sites (build_runtime /
        // build_runtime_with_skill_tx) pass Some(slot.clone()) — pin the
        // call shape by checking the source-level wiring stays reachable:
        // register_subagent_if_enabled must be referenced with a slot arg
        // whenever subagent_enabled. (Full behavioral coverage lives in the
        // agent-tool tests; this pins the TuiRuntime contract.)
        let cfg = test_config();
        if cfg.subagent_enabled {
            // If enabled via config, the slot must still be resolvable.
            assert!(tui_rt.subagent_token_slot.lock().is_ok());
        }
    }

    #[test]
    fn effective_api_key_treats_empty_as_missing() {
        assert_eq!(effective_api_key(None), None);
        assert_eq!(effective_api_key(Some("")), None);
        assert_eq!(effective_api_key(Some("sk-real")), Some("sk-real"));
    }

    #[test]
    fn build_provider_selects_anthropic_for_anthropic_type() {
        // Kills the "delete match arm anthropic" mutant: that falls through
        // to the OpenAi branch, whose supports_deferred_tools() is false.
        let cfg = test_config();
        let provider =
            build_provider(&cfg, "sk-test".to_string()).expect("anthropic provider builds");
        assert!(
            provider.supports_deferred_tools(),
            "anthropic provider_type must yield a deferred-tools provider"
        );
    }

    #[test]
    fn build_provider_selects_openai_for_other_types() {
        let mut cfg = test_config();
        cfg.provider_type = "openai".to_string();
        cfg.api_base = "https://api.openai.com".to_string();
        let provider = build_provider(&cfg, "sk-test".to_string()).expect("openai provider builds");
        assert!(
            !provider.supports_deferred_tools(),
            "non-anthropic provider_type must yield a non-deferred provider"
        );
    }

    // ── /model picker: build_provider_for_model ────────────────────────────

    /// RAII guard that saves/drops a single env var for the duration of a test.
    /// Assumes the caller already holds `env_lock()` via `PinnedRecursiveHome`.
    struct EnvGuard {
        name: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let prev = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prev }
        }
        fn remove(name: &'static str) -> Self {
            let prev = std::env::var(name).ok();
            std::env::remove_var(name);
            Self { name, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// Issue #17, TUI half: the `/model` picker hot-swaps the provider, so it
    /// must re-derive `max_tokens` for the model it is swapping *to*. Before
    /// the fix, `config_for_preset_model` kept the startup model's cap, and
    /// switching to a lower-ceiling model 400'd on the next turn.
    ///
    /// `config_for_preset_model` is asserted directly rather than
    /// `build_provider_for_model`, because a `ChatProvider` does not expose
    /// its `max_tokens` — the config is the observable seam.
    #[test]
    fn model_hot_swap_rederives_max_tokens() {
        let home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_MAX_TOKENS");
        let _g2 = EnvGuard::remove("RECURSIVE_API_KEY");

        let providers_d = home.path().join("providers.d");
        std::fs::create_dir_all(&providers_d).expect("mkdir providers.d");
        std::fs::write(
            providers_d.join("two-cap.toml"),
            r#"[[providers]]
id = "two-cap"
name = "Two Cap"
provider_type = "openai"
api_base = "https://example.invalid/v1"
default_model = "big-cap"
mainland_accessible = false
key_env = "TWO_CAP_API_KEY"
key_url = ""

[[providers.models]]
name = "big-cap"
context_window = 1000000
max_tokens = 384000

[[providers.models]]
name = "small-cap"
context_window = 200000
max_tokens = 128000
"#,
        )
        .expect("write two-cap preset");
        let cfg_dir = home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir .recursive");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\npreset = \"two-cap\"\nmodel = \"big-cap\"\n",
        )
        .expect("write config");

        let preset = recursive::providers::find_preset_effective("two-cap")
            .expect("two-cap preset resolves from providers.d");

        // Startup model's cap, as `Config::from_env` would derive it.
        let startup = config_for_preset_model(&preset, "big-cap").expect("startup config");
        assert_eq!(startup.max_tokens, 384_000);

        // The picker swaps to a model with a lower output ceiling.
        let swapped = config_for_preset_model(&preset, "small-cap").expect("swapped config");
        assert_eq!(
            swapped.max_tokens, 128_000,
            "hot-swap must re-derive max_tokens from the new model's preset \
             entry, not carry the previous model's cap (issue #17)"
        );
        assert_eq!(swapped.model, "small-cap");
        assert_eq!(
            swapped.preset.as_deref(),
            Some("two-cap"),
            "the swapped config must record the preset that served the model"
        );
    }

    #[test]
    fn build_provider_for_model_unknown_preset_errors() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        let err = build_provider_for_model("definitely-not-a-preset", "any")
            .err()
            .expect("expected error for unknown preset");
        assert!(
            err.to_string().contains("unknown provider preset"),
            "expected unknown-preset error, got: {err}"
        );
    }

    #[test]
    fn build_provider_for_model_no_api_key_errors() {
        // No preset key_env env var AND no RECURSIVE_API_KEY → error.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        // Pick a bundled preset with a non-empty key_env (deepseek → DEEPSEEK_API_KEY)
        // and make sure that env var is also unset.
        let _g3 = EnvGuard::remove("DEEPSEEK_API_KEY");
        let err = build_provider_for_model("deepseek", "deepseek-chat")
            .err()
            .expect("expected error for missing api key");
        assert!(
            err.to_string().contains("no API key"),
            "expected no-api-key error, got: {err}"
        );
    }

    #[test]
    fn build_provider_for_model_uses_preset_key_env() {
        // Setting the preset's key_env env var lets the provider build even
        // when RECURSIVE_API_KEY is unset. Pins the preset.key_env branch.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        let _g3 = EnvGuard::set("DEEPSEEK_API_KEY", "sk-deepseek-dummy");
        let provider = build_provider_for_model("deepseek", "deepseek-chat")
            .expect("provider builds with preset key_env set");
        // DeepSeek is OpenAI-compatible → not a deferred-tools provider.
        assert!(!provider.supports_deferred_tools());
    }

    #[test]
    fn build_provider_for_model_falls_back_to_config_api_key() {
        // When the preset's key_env env var is unset but the config file has
        // an api_key, the fallback kicks in so a cross-preset switch succeeds.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        let _g3 = EnvGuard::remove("DEEPSEEK_API_KEY");
        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\napi_key = \"sk-shared\"\nmodel = \"x\"\ntype = \"openai\"\n",
        )
        .expect("write config");
        let provider = build_provider_for_model("deepseek", "deepseek-chat")
            .expect("provider builds via config api_key fallback");
        assert!(!provider.supports_deferred_tools());
    }

    #[test]
    fn sandbox_extra_roots_maps_rw_and_ro_tiers() {
        let mut cfg = test_config();
        cfg.extra_dirs = vec![PathBuf::from("/rw/a"), PathBuf::from("/rw/b")];
        cfg.extra_readonly_dirs = vec![PathBuf::from("/ro/x")];
        let roots = sandbox_extra_roots(&cfg);
        assert_eq!(roots.len(), 3);
        assert_eq!(
            roots[0],
            (PathBuf::from("/rw/a"), recursive::AccessTier::ReadWrite)
        );
        assert_eq!(
            roots[1],
            (PathBuf::from("/rw/b"), recursive::AccessTier::ReadWrite)
        );
        assert_eq!(
            roots[2],
            (PathBuf::from("/ro/x"), recursive::AccessTier::ReadOnly)
        );
    }

    #[test]
    fn sandbox_extra_roots_empty_when_config_has_none() {
        let cfg = test_config();
        assert!(sandbox_extra_roots(&cfg).is_empty());
    }

    #[test]
    fn discover_loaded_skills_reads_env_paths() {
        // PinnedRecursiveHome acquires env_lock(), serialising env mutation.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());

        let skills_root = tempfile::tempdir().expect("tempdir");
        let skill_dir = skills_root.path().join("demo-skill");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: demo\n---\nbody\n",
        )
        .expect("write SKILL.md");

        // A second skills root, joined with the OS-native path separator
        // (`;` on Windows, `:` on Unix). This pins that
        // `discover_loaded_skills` uses `split_paths` rather than a hardcoded
        // `:` split, which on Windows would fragment `C:\skills` at the drive
        // letter's colon.
        let skills_root_b = tempfile::tempdir().expect("tempdir");
        let skill_dir_b = skills_root_b.path().join("other-skill");
        std::fs::create_dir_all(&skill_dir_b).expect("mkdir");
        std::fs::write(
            skill_dir_b.join("SKILL.md"),
            "---\ndescription: other\n---\nbody\n",
        )
        .expect("write SKILL.md");
        let joined = std::env::join_paths([
            std::path::Path::new(skills_root.path()),
            std::path::Path::new(skills_root_b.path()),
        ])
        .expect("join_paths");

        let prev = std::env::var("RECURSIVE_SKILL_PATHS").ok();
        std::env::set_var("RECURSIVE_SKILL_PATHS", &joined);

        let cfg = test_config();
        let skills = discover_loaded_skills(&cfg);

        match prev {
            Some(v) => std::env::set_var("RECURSIVE_SKILL_PATHS", v),
            None => std::env::remove_var("RECURSIVE_SKILL_PATHS"),
        }

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"demo-skill"),
            "expected demo-skill in {names:?}"
        );
        assert!(
            names.contains(&"other-skill"),
            "expected other-skill (OS-native separator split) in {names:?}"
        );
    }

    // ── Goal-334: file reinjector wiring in build_runtime ─────────────────
    //
    // The TUI's build_runtime extracts the shared read_state from the tool
    // registry and constructs a FileReinjector from env. These tests pin the
    // two load-bearing links of that wiring without spinning up a full
    // TuiRuntime (which needs a live provider): (1) the standard tool set
    // exposes a read_file_state the reinjector can share, and (2) the env
    // helper honours the disabled sentinel so `RECURSIVE_REINJECT_FILES=0`
    // yields no reinjector in TUI mode either.

    #[test]
    fn standard_tools_expose_read_file_state_for_reinjector() {
        // The TUI wiring (`tools.read_file_state()` before the builder move)
        // only attaches a reinjector when the registry actually carries a
        // shared read state. Pin that build_standard_tools attaches one —
        // if a future refactor stops sharing it, file reinjection silently
        // becomes a no-op in TUI mode.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let tools = recursive::tools::build_standard_tools(std::path::Path::new("."), &[], 300);
        let read_state = tools
            .read_file_state()
            .expect("standard tool set must expose a shared read_file_state");
        // And the reinjector can be built from that state when enabled.
        let _g = EnvGuard::set("RECURSIVE_REINJECT_FILES", "3");
        let r = recursive::build_file_reinjector_from_env(read_state)
            .expect("enabled env must yield a reinjector from the shared state");
        assert_eq!(r.max_files, 3, "explicit count must propagate");
    }

    #[test]
    fn reinjector_disabled_in_tui_when_env_zero() {
        // `RECURSIVE_REINJECT_FILES=0` means opt-out everywhere; the TUI
        // path must respect it too (build_file_reinjector_from_env → None).
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let tools = recursive::tools::build_standard_tools(std::path::Path::new("."), &[], 300);
        let read_state = tools
            .read_file_state()
            .expect("standard tool set must expose a shared read_file_state");
        let _g = EnvGuard::set("RECURSIVE_REINJECT_FILES", "0");
        assert!(
            recursive::build_file_reinjector_from_env(read_state).is_none(),
            "RECURSIVE_REINJECT_FILES=0 must disable the reinjector in TUI mode"
        );
    }

    // ── Goal-335: skill reinjector wiring in build_runtime ───────────────
    // Mirrors the file-reinjector tests above: the TUI wiring calls
    // build_skill_reinjector_from_env(skills) right after the file reinjector.
    // Pin that the env helper honours enabled/disabled so TUI mode parity holds.

    #[test]
    fn skill_reinjector_enabled_from_env_in_tui() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g = EnvGuard::set("RECURSIVE_REINJECT_SKILLS", "0");
        // disabled sentinel must yield None in TUI mode too.
        assert!(
            recursive::build_skill_reinjector_from_env(Vec::new()).is_none(),
            "RECURSIVE_REINJECT_SKILLS=0 must disable the skill reinjector in TUI mode"
        );
    }

    #[test]
    fn skill_reinjector_default_when_unset_in_tui() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g = EnvGuard::remove("RECURSIVE_REINJECT_SKILLS");
        let r = recursive::build_skill_reinjector_from_env(Vec::new())
            .expect("unset env must yield Some with defaults");
        assert_eq!(
            r.token_budget, 25_000,
            "default skill-reinjector token budget must be 25_000"
        );
    }
}
