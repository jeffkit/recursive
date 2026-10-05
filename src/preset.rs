//! Agent presets (issue #127): session-level declarative composition.
//!
//! A preset is the declared answer to "how is this session wired": which
//! prompt profile, which tool profile, which context management (compaction /
//! microcompaction / transcript cap) and which post-compaction re-injection.
//! Every frontend (HTTP, CLI, TUI) resolves the same [`ResolvedPreset`] for a
//! session and hands it to [`apply`] — the single assembly point — so the
//! channels cannot drift apart and adding a preset is one declaration plus
//! zero builder branches.
//!
//! # Precedence
//!
//! ```text
//! environment variable  >  preset declaration
//! ```
//!
//! The env vars stay the operator escape hatch — they are what the channels
//! read today, so the default behaviour of the `standard` preset is unchanged
//! — but the preset turns the scattered "the mechanism exists yet is off by
//! default" switches into an explicit inventory ([`Capability`]) that a
//! session's preset lists in full.
//!
//! # Session selection
//!
//! `POST /sessions`'s `preset` field (HTTP) / `RECURSIVE_AGENT_PRESET` (every
//! channel). An unknown id is an error, never a silent fallback.

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::compact::micro::Microcompactor;
use crate::compact::{Compactor, FileReinjector, SkillReinjector};
use crate::config::Config;
use crate::llm::{default_compact_threshold_chars, default_compact_threshold_tokens};
use crate::runtime::AgentRuntimeBuilder;
use crate::skills::Skill;
use crate::tools::fs::ReadFileState;

// ── env var names (declared once: the inventory and the resolver share them) ─

/// Selects the preset for a session when the channel has no explicit id.
pub const PRESET_ENV_VAR: &str = "RECURSIVE_AGENT_PRESET";
/// Cross-turn compaction char threshold (`0`/`off`/`false` disables).
pub const COMPACT_THRESHOLD_ENV: &str = "RECURSIVE_COMPACT_THRESHOLD";
/// Proactive tool-result pruning trigger (unset/`0`/`off`/`false` = off).
pub const MICROCOMPACT_TRIGGER_ENV: &str = "RECURSIVE_MICROCOMPACT_TRIGGER";
/// Keep-count for the microcompactor.
pub const MICROCOMPACT_KEEP_ENV: &str = "RECURSIVE_MICROCOMPACT_KEEP";
/// Hard transcript char cap (unset/`0`/`off`/`false` = unlimited).
pub const MAX_TRANSCRIPT_CHARS_ENV: &str = "RECURSIVE_MAX_TRANSCRIPT_CHARS";
/// Recently-read files re-injected after compaction (`0`/`off`/`false` = off).
pub const REINJECT_FILES_ENV: &str = "RECURSIVE_REINJECT_FILES";
/// Token budget for the file re-injection.
pub const REINJECT_FILE_BUDGET_ENV: &str = "RECURSIVE_REINJECT_FILE_BUDGET";
/// Invoked skills re-injected after compaction (`0`/`off`/`false` = off).
pub const REINJECT_SKILLS_ENV: &str = "RECURSIVE_REINJECT_SKILLS";
/// Token budget for the skill re-injection.
pub const REINJECT_SKILL_BUDGET_ENV: &str = "RECURSIVE_REINJECT_SKILL_BUDGET";

/// File re-injection defaults (mirrors [`FileReinjector`]'s).
pub const DEFAULT_REINJECT_FILES: usize = 5;
pub const DEFAULT_REINJECT_FILE_BUDGET: usize = 50_000;
pub const DEFAULT_PER_FILE_BUDGET: usize = 5_000;
/// Skill re-injection defaults (mirrors [`SkillReinjector`]'s).
pub const DEFAULT_REINJECT_SKILL_BUDGET: usize = 25_000;
pub const DEFAULT_PER_SKILL_BUDGET: usize = 5_000;

// ── declaration ───────────────────────────────────────────────────────────

/// Prompt profile: what the session's system prompt is composed of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PromptProfile {
    /// Appended to the assembled base prompt. `None` = the channel's own
    /// prompt is used verbatim.
    pub persona_suffix: Option<&'static str>,
    /// Auto-load the full bodies of the skills matching the session goal (the
    /// CLI's goal-based injection). Channels with no goal at prompt-build time
    /// cannot honour it.
    pub auto_skill_injection: bool,
}

/// Tool profile: which optional tool groups the preset asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToolProfile {
    /// Register `enter_plan_mode` / `exit_plan_mode` / `request_plan_mode`.
    /// These block on a live human, so the channel must also report itself
    /// interactive ([`ChannelSupport`]) for them to be registered at all.
    pub plan_mode_tools: bool,
}

/// How the cross-turn compactor's threshold is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CompactionMode {
    /// From the model's context window (80 % of the effective window).
    Auto,
    /// Explicit character count.
    Chars(usize),
}

/// Proactive tool-result pruning (no LLM call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MicrocompactionSpec {
    pub trigger_tool_count: usize,
    pub keep_recent: usize,
}

/// Recently-read files re-injected after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FileReinjectSpec {
    pub max_files: usize,
    pub token_budget: usize,
    pub per_file_budget: usize,
}

/// Invoked skills re-injected after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SkillReinjectSpec {
    pub token_budget: usize,
    pub per_skill_budget: usize,
}

/// Context management a preset declares. `None` means "capability off".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContextProfile {
    pub compaction: Option<CompactionMode>,
    pub microcompaction: Option<MicrocompactionSpec>,
    pub max_transcript_chars: Option<usize>,
    pub reinject_recent_files: Option<FileReinjectSpec>,
    pub reinject_invoked_skills: Option<SkillReinjectSpec>,
}

/// Whether a declared capability is on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityDefault {
    Enabled,
    Disabled,
}

/// One row of a preset's capability inventory.
///
/// The point of the inventory is discoverability: a capability that exists but
/// is off by default must be listed here with the switch that turns it on
/// (the DeepSeek-Harness `disabled: true` row semantics), instead of only
/// being visible to whoever reads the assembly code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capability {
    pub name: &'static str,
    pub default: CapabilityDefault,
    /// Env var (or config key) that toggles it; `""` when the channel alone
    /// decides.
    pub toggle: &'static str,
    pub note: &'static str,
}

/// A session preset: data only. Every frontend resolves it the same way and
/// never branches on the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentPreset {
    pub id: &'static str,
    pub description: &'static str,
    pub prompt: PromptProfile,
    pub tools: ToolProfile,
    pub context: ContextProfile,
    pub capabilities: &'static [Capability],
}

impl AgentPreset {
    /// Resolve this declaration into the concrete numbers a session runs with:
    /// the declaration with the environment overlaid.
    ///
    /// Pure — the env is passed in as a [`PresetEnv`] snapshot, so the result
    /// is testable without touching process-global state.
    pub fn resolve(&self, config: &Config, env: &PresetEnv) -> ResolvedPreset {
        ResolvedPreset {
            id: self.id.to_string(),
            prompt: self.prompt,
            tools: self.tools,
            context: self.resolve_context(&config.model, env),
        }
    }

    fn resolve_context(&self, model: &str, env: &PresetEnv) -> ContextFacts {
        let decl = self.context;

        // Compaction: env `0`/`off`/`false` (or anything unparsable) disables;
        // a positive integer is an explicit char threshold; unset takes the
        // declaration. The token threshold is always derived from the model —
        // it is the more reliable trigger for CJK content.
        let compaction = match parse_toggle(env.compact_threshold.as_deref()) {
            Toggle::Unset => decl.compaction,
            Toggle::Disabled => None,
            Toggle::Value(n) => Some(CompactionMode::Chars(n)),
        }
        .map(|mode| {
            let threshold_chars = match mode {
                CompactionMode::Auto => default_compact_threshold_chars(model),
                CompactionMode::Chars(n) => n,
            };
            // Borrow the default keep-count from the compactor itself so the
            // two cannot drift.
            let template = Compactor::new(threshold_chars);
            CompactionFacts {
                threshold_chars,
                threshold_prompt_tokens: Some(default_compact_threshold_tokens(model)),
                keep_recent_n: template.keep_recent_n,
            }
        });

        let microcompaction = match parse_toggle(env.microcompact_trigger.as_deref()) {
            // Unset: the declaration is already the resolved shape.
            Toggle::Unset => decl.microcompaction,
            Toggle::Disabled => None,
            // Env-derived: reuse the historical parser so the keep-count
            // default and the `<= 0` handling stay in one place.
            Toggle::Value(_) => crate::compact::micro::build_microcompactor_from_env(
                env.microcompact_trigger.as_deref(),
                env.microcompact_keep.as_deref(),
            )
            .map(MicrocompactionSpec::facts),
        };

        let max_transcript_chars = match parse_toggle(env.max_transcript_chars.as_deref()) {
            Toggle::Unset => decl.max_transcript_chars,
            Toggle::Disabled => None,
            Toggle::Value(n) => Some(n),
        };

        // Re-injection: an explicit env value is parsed by the same helpers
        // the pre-preset channels used (so its tolerance rules stay in one
        // place); unset takes the declaration, with the budget env still able
        // to override the declared budget.
        let reinject_recent_files = match env.reinject_files.as_deref() {
            None => decl.reinject_recent_files.map(|spec| FileReinjectSpec {
                token_budget: parse_positive(env.reinject_file_budget.as_deref())
                    .unwrap_or(spec.token_budget),
                ..spec
            }),
            Some(_) => crate::compact::reinject::file_reinjector_spec_from_env(
                env.reinject_files.as_deref(),
                env.reinject_file_budget.as_deref(),
            ),
        };

        let reinject_invoked_skills = match env.reinject_skills.as_deref() {
            None => decl.reinject_invoked_skills.map(|spec| SkillReinjectSpec {
                token_budget: parse_positive(env.reinject_skill_budget.as_deref())
                    .unwrap_or(spec.token_budget),
                ..spec
            }),
            Some(_) => crate::compact::reinject::skill_reinjector_spec_from_env(
                env.reinject_skills.as_deref(),
                env.reinject_skill_budget.as_deref(),
            ),
        };

        ContextFacts {
            compaction,
            microcompaction,
            max_transcript_chars,
            reinject_recent_files,
            reinject_invoked_skills,
        }
    }
}

impl MicrocompactionSpec {
    fn facts(mc: Microcompactor) -> Self {
        Self {
            trigger_tool_count: mc.trigger_tool_count,
            keep_recent: mc.keep_recent,
        }
    }
}

// ── resolved form ─────────────────────────────────────────────────────────

/// Concrete compactor settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompactionFacts {
    pub threshold_chars: usize,
    pub threshold_prompt_tokens: Option<u32>,
    pub keep_recent_n: usize,
}

/// The context management a session actually runs with. Produced by
/// [`AgentPreset::resolve`] and read back from an assembled builder / runtime
/// via `context_management_facts`, so a cross-channel parity test compares
/// like with like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct ContextFacts {
    pub compaction: Option<CompactionFacts>,
    pub microcompaction: Option<MicrocompactionSpec>,
    pub max_transcript_chars: Option<usize>,
    pub reinject_recent_files: Option<FileReinjectSpec>,
    pub reinject_invoked_skills: Option<SkillReinjectSpec>,
}

/// A preset declaration with the environment overlaid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPreset {
    pub id: String,
    pub prompt: PromptProfile,
    pub tools: ToolProfile,
    pub context: ContextFacts,
}

/// An unknown preset id, with the ids that do exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPreset {
    pub id: String,
    pub known: Vec<&'static str>,
}

impl std::fmt::Display for UnknownPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown agent preset {:?} (known: {})",
            self.id,
            self.known.join(", ")
        )
    }
}

impl std::error::Error for UnknownPreset {}

// ── env snapshot ──────────────────────────────────────────────────────────

/// Snapshot of the preset-related environment.
///
/// Values are kept raw: parsing (and its tolerance rules) belongs to
/// [`AgentPreset::resolve`], and an unparsable value must behave exactly as
/// the channel read it before presets existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetEnv {
    pub agent_preset: Option<String>,
    pub compact_threshold: Option<String>,
    pub microcompact_trigger: Option<String>,
    pub microcompact_keep: Option<String>,
    pub max_transcript_chars: Option<String>,
    pub reinject_files: Option<String>,
    pub reinject_file_budget: Option<String>,
    pub reinject_skills: Option<String>,
    pub reinject_skill_budget: Option<String>,
}

impl PresetEnv {
    /// Read the process environment.
    pub fn from_process() -> Self {
        Self {
            agent_preset: var(PRESET_ENV_VAR),
            compact_threshold: var(COMPACT_THRESHOLD_ENV),
            microcompact_trigger: var(MICROCOMPACT_TRIGGER_ENV),
            microcompact_keep: var(MICROCOMPACT_KEEP_ENV),
            max_transcript_chars: var(MAX_TRANSCRIPT_CHARS_ENV),
            reinject_files: var(REINJECT_FILES_ENV),
            reinject_file_budget: var(REINJECT_FILE_BUDGET_ENV),
            reinject_skills: var(REINJECT_SKILLS_ENV),
            reinject_skill_budget: var(REINJECT_SKILL_BUDGET_ENV),
        }
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `on` / `off` / value tri-state used by every "count or switch" env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Toggle {
    /// Not set — the declaration decides.
    Unset,
    /// Explicitly off, or unparsable (the historical tolerance).
    Disabled,
    Value(usize),
}

fn parse_toggle(raw: Option<&str>) -> Toggle {
    match raw {
        None => Toggle::Unset,
        Some("0") | Some("off") | Some("false") => Toggle::Disabled,
        Some(s) => match parse_positive(Some(s)) {
            Some(n) => Toggle::Value(n),
            None => Toggle::Disabled,
        },
    }
}

fn parse_positive(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|s| s.parse::<usize>().ok()).filter(|&n| n > 0)
}

// ── registry ──────────────────────────────────────────────────────────────

/// Capability inventory of the `standard` preset.
static STANDARD_CAPABILITIES: &[Capability] = &[
    Capability {
        name: "cross-turn-compaction",
        default: CapabilityDefault::Enabled,
        toggle: COMPACT_THRESHOLD_ENV,
        note: "LLM-summary compaction when the transcript outgrows the model's window; \
               the threshold auto-derives from the model unless the env var pins it.",
    },
    Capability {
        name: "proactive-tool-result-pruning",
        default: CapabilityDefault::Disabled,
        toggle: MICROCOMPACT_TRIGGER_ENV,
        note: "no-LLM microcompaction; opt-in because the old default fired at ~5% of a \
               1M-token window.",
    },
    Capability {
        name: "transcript-char-cap",
        default: CapabilityDefault::Disabled,
        toggle: MAX_TRANSCRIPT_CHARS_ENV,
        note: "pre-turn hard trim of the transcript; unset means unlimited.",
    },
    Capability {
        name: "recently-read-file-reinjection",
        default: CapabilityDefault::Enabled,
        toggle: REINJECT_FILES_ENV,
        note: "re-attach the last N read files as System messages after compaction, so \
               the model does not pay to re-read them.",
    },
    Capability {
        name: "invoked-skill-reinjection",
        default: CapabilityDefault::Enabled,
        toggle: REINJECT_SKILLS_ENV,
        note: "re-attach invoked skill bodies after compaction, so the model keeps \
               following them.",
    },
    Capability {
        name: "plan-mode-tools",
        default: CapabilityDefault::Enabled,
        toggle: "",
        note: "enter_plan_mode / exit_plan_mode / request_plan_mode; also requires an \
               interactive channel to answer the approval prompt.",
    },
    Capability {
        name: "goal-based-skill-injection",
        default: CapabilityDefault::Enabled,
        toggle: "",
        note: "auto-load skill bodies matching the session goal; requires a goal at \
               prompt-build time (CLI run, not HTTP sessions).",
    },
    Capability {
        name: "subagent-delegation",
        default: CapabilityDefault::Disabled,
        toggle: "RECURSIVE_SUBAGENT_ENABLED",
        note: "the `agent` tool plus the coordinator prompt; channel-agnostic, switched \
               on when the operator enables it.",
    },
    Capability {
        name: "self-scheduling-wakeup",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "ScheduleWakeup for loop / TUI sessions; registered by channels that own a \
               wakeup slot.",
    },
    Capability {
        name: "skill-install-tool",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "InstallSkill; TUI-only because it prompts through the UI channel.",
    },
    Capability {
        name: "mcp-tools",
        default: CapabilityDefault::Disabled,
        toggle: "RECURSIVE_MCP_CONFIG",
        note: "mcp__* tools; added to the process-wide registry at startup.",
    },
];

/// The built-in `standard` preset: the behaviour every channel had before
/// presets existed, stated once.
pub static STANDARD: AgentPreset = AgentPreset {
    id: "standard",
    description: "General coding session: auto compaction, post-compaction re-injection \
                  of recently-read files and invoked skills, plan-mode tools.",
    prompt: PromptProfile {
        persona_suffix: None,
        auto_skill_injection: true,
    },
    tools: ToolProfile {
        plan_mode_tools: true,
    },
    context: ContextProfile {
        compaction: Some(CompactionMode::Auto),
        microcompaction: None,
        max_transcript_chars: None,
        reinject_recent_files: Some(FileReinjectSpec {
            max_files: DEFAULT_REINJECT_FILES,
            token_budget: DEFAULT_REINJECT_FILE_BUDGET,
            per_file_budget: DEFAULT_PER_FILE_BUDGET,
        }),
        reinject_invoked_skills: Some(SkillReinjectSpec {
            token_budget: DEFAULT_REINJECT_SKILL_BUDGET,
            per_skill_budget: DEFAULT_PER_SKILL_BUDGET,
        }),
    },
    capabilities: STANDARD_CAPABILITIES,
};

/// Every built-in preset. Adding one is a declaration plus a line here — no
/// builder branch anywhere (issue #127 acceptance 3).
static BUILTIN: [&AgentPreset; 1] = [&STANDARD];

/// Every built-in preset.
pub fn builtin() -> &'static [&'static AgentPreset] {
    &BUILTIN
}

/// Look up a built-in preset by id.
pub fn find(id: &str) -> Option<&'static AgentPreset> {
    builtin().iter().copied().find(|p| p.id == id)
}

/// The ids of every built-in preset.
pub fn builtin_ids() -> Vec<&'static str> {
    builtin().iter().map(|p| p.id).collect()
}

/// Pick the preset for a session: explicit id, else `RECURSIVE_AGENT_PRESET`,
/// else `standard`.
pub fn select(
    explicit: Option<&str>,
    env: &PresetEnv,
) -> Result<&'static AgentPreset, UnknownPreset> {
    let wanted = explicit.or(env.agent_preset.as_deref());
    let Some(id) = wanted else {
        return Ok(&STANDARD);
    };
    find(id).ok_or_else(|| UnknownPreset {
        id: id.to_string(),
        known: builtin_ids(),
    })
}

/// Resolve the preset a session runs with. This is the entry every frontend
/// uses; it exists so "which preset is this?" is answered in one place.
pub fn resolve_session(
    explicit: Option<&str>,
    config: &Config,
    env: &PresetEnv,
) -> Result<ResolvedPreset, UnknownPreset> {
    Ok(select(explicit, env)?.resolve(config, env))
}

// ── assembly ──────────────────────────────────────────────────────────────

/// What the calling channel can host.
///
/// A preset declares a capability; the channel reports whether it can actually
/// support it. Plan-mode tools block until a human answers the approval
/// prompt, so a headless channel must never get them even though the preset
/// asks for them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelSupport {
    pub interactive: bool,
}

/// Side objects the reinjectors need. Missing pieces simply disable the
/// corresponding re-injection (a channel with no shared read state has nothing
/// to re-inject).
#[derive(Default, Clone)]
pub struct PresetAssets {
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
    pub skills: Vec<Skill>,
}

impl PresetAssets {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_read_state(mut self, read_state: Arc<Mutex<ReadFileState>>) -> Self {
        self.read_state = Some(read_state);
        self
    }

    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.skills = skills;
        self
    }
}

/// Build a [`PresetAssets`] from a tool registry (read state) and a skill
/// catalog. Every frontend goes through this so the assets are gathered the
/// same way.
pub fn assets_from_registry(
    registry: &crate::tools::ToolRegistry,
    skills: Vec<Skill>,
) -> PresetAssets {
    let assets = PresetAssets::new().with_skills(skills);
    match registry.read_file_state() {
        Some(state) => assets.with_read_state(state),
        None => assets,
    }
}

/// The single assembly point (issue #127): install everything the resolved
/// preset declares onto `builder`. Called once per builder chain, so "off" is
/// expressed by not installing anything.
pub fn apply(
    mut builder: AgentRuntimeBuilder,
    preset: &ResolvedPreset,
    assets: &PresetAssets,
    channel: ChannelSupport,
) -> AgentRuntimeBuilder {
    if let Some(c) = preset.context.compaction {
        let mut compactor = Compactor::new(c.threshold_chars).keep_recent_n(c.keep_recent_n);
        if let Some(tokens) = c.threshold_prompt_tokens {
            compactor = compactor.threshold_prompt_tokens(tokens);
        }
        builder = builder.compactor(compactor);
    }

    if let Some(mc) = preset.context.microcompaction {
        builder =
            builder.microcompactor(Microcompactor::new(mc.trigger_tool_count, mc.keep_recent));
    }

    if let Some(n) = preset.context.max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }

    if let (Some(spec), Some(read_state)) = (
        preset.context.reinject_recent_files,
        assets.read_state.clone(),
    ) {
        builder = builder.file_reinjector(FileReinjector {
            max_files: spec.max_files,
            token_budget: spec.token_budget,
            per_file_budget: spec.per_file_budget,
            read_state,
        });
    }

    if let Some(spec) = preset.context.reinject_invoked_skills {
        // Installed even with an empty catalog (it then has nothing to look
        // up) — the channels did exactly that before presets, and keeping the
        // assembly a pure function of (preset, assets) is what lets a parity
        // test compare `context_management_facts()` against the declaration.
        builder = builder.skill_reinjector(SkillReinjector {
            token_budget: spec.token_budget,
            per_skill_budget: spec.per_skill_budget,
            skills: assets.skills.clone(),
        });
    }

    builder
        .with_plan_mode_tools(preset.tools.plan_mode_tools && channel.interactive)
        .with_preset_id(preset.id.clone())
}

/// Apply the preset's prompt profile to an already-assembled system prompt.
pub fn apply_prompt(prompt: String, preset: &ResolvedPreset) -> String {
    match preset.prompt.persona_suffix {
        Some(suffix) => format!("{prompt}\n{suffix}"),
        None => prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockProvider;

    fn config_for(model: &str) -> Config {
        Config {
            model: model.to_string(),
            ..crate::http::test_config_stub()
        }
    }

    fn fresh_builder() -> AgentRuntimeBuilder {
        AgentRuntimeBuilder::new().llm(Arc::new(MockProvider::new(vec![])))
    }

    // ── declaration / resolution ──────────────────────────────────────────

    #[test]
    fn standard_preset_declares_compaction_and_reinjection() {
        let config = config_for("preset-test-model");
        let resolved = STANDARD.resolve(&config, &PresetEnv::default());

        let compaction = resolved.context.compaction.expect("compaction on");
        assert_eq!(
            compaction.threshold_chars,
            default_compact_threshold_chars("preset-test-model")
        );
        assert_eq!(
            compaction.threshold_prompt_tokens,
            Some(default_compact_threshold_tokens("preset-test-model"))
        );
        assert!(compaction.keep_recent_n > 0);
        assert_eq!(
            resolved.context.reinject_recent_files,
            Some(FileReinjectSpec {
                max_files: DEFAULT_REINJECT_FILES,
                token_budget: DEFAULT_REINJECT_FILE_BUDGET,
                per_file_budget: DEFAULT_PER_FILE_BUDGET,
            })
        );
        assert_eq!(
            resolved.context.reinject_invoked_skills,
            Some(SkillReinjectSpec {
                token_budget: DEFAULT_REINJECT_SKILL_BUDGET,
                per_skill_budget: DEFAULT_PER_SKILL_BUDGET,
            })
        );
        // Disabled-by-default stays disabled until something turns it on.
        assert!(resolved.context.microcompaction.is_none());
        assert!(resolved.context.max_transcript_chars.is_none());
    }

    /// The declaration's reinjection defaults must stay equal to the
    /// constructors' defaults, or the preset and the reinjectors disagree.
    #[test]
    fn declaration_defaults_match_the_reinjector_constructors() {
        let skills = SkillReinjector::new(Vec::new());
        assert_eq!(skills.token_budget, DEFAULT_REINJECT_SKILL_BUDGET);
        assert_eq!(skills.per_skill_budget, DEFAULT_PER_SKILL_BUDGET);

        let read_state: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let files = FileReinjector::new(read_state);
        assert_eq!(files.max_files, DEFAULT_REINJECT_FILES);
        assert_eq!(files.token_budget, DEFAULT_REINJECT_FILE_BUDGET);
        assert_eq!(files.per_file_budget, DEFAULT_PER_FILE_BUDGET);
    }

    #[test]
    fn env_overrides_every_declared_context_knob() {
        let config = config_for("preset-test-model");

        // Compaction off.
        for raw in ["0", "off", "false", "not-a-number"] {
            let env = PresetEnv {
                compact_threshold: Some(raw.to_string()),
                ..PresetEnv::default()
            };
            assert!(
                STANDARD.resolve(&config, &env).context.compaction.is_none(),
                "{raw:?} must disable compaction"
            );
        }

        // Explicit compaction threshold.
        let env = PresetEnv {
            compact_threshold: Some("4242".to_string()),
            ..PresetEnv::default()
        };
        let resolved = STANDARD.resolve(&config, &env);
        let compaction = resolved.context.compaction.expect("compactor");
        assert_eq!(compaction.threshold_chars, 4242);
        assert_eq!(
            compaction.threshold_prompt_tokens,
            Some(default_compact_threshold_tokens("preset-test-model")),
            "the model-derived token trigger survives an explicit char threshold"
        );

        // Microcompaction is opt-in, and env is the opt-in.
        let env = PresetEnv {
            microcompact_trigger: Some("40".to_string()),
            microcompact_keep: Some("6".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(
            STANDARD.resolve(&config, &env).context.microcompaction,
            Some(MicrocompactionSpec {
                trigger_tool_count: 40,
                keep_recent: 6,
            })
        );

        // Transcript cap.
        let env = PresetEnv {
            max_transcript_chars: Some("999".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(
            STANDARD.resolve(&config, &env).context.max_transcript_chars,
            Some(999)
        );

        // Reinjection off / budget override.
        let env = PresetEnv {
            reinject_files: Some("off".to_string()),
            reinject_skills: Some("0".to_string()),
            ..PresetEnv::default()
        };
        let resolved = STANDARD.resolve(&config, &env);
        assert!(resolved.context.reinject_recent_files.is_none());
        assert!(resolved.context.reinject_invoked_skills.is_none());

        let env = PresetEnv {
            reinject_file_budget: Some("1234".to_string()),
            reinject_skill_budget: Some("5678".to_string()),
            ..PresetEnv::default()
        };
        let resolved = STANDARD.resolve(&config, &env);
        let files = resolved.context.reinject_recent_files.expect("files on");
        assert_eq!(files.token_budget, 1234);
        assert_eq!(files.max_files, DEFAULT_REINJECT_FILES);
        let sk = resolved.context.reinject_invoked_skills.expect("skills on");
        assert_eq!(sk.token_budget, 5678);
    }

    /// The resolver must reproduce the historical env helpers' decision
    /// (enabled/disabled + budgets) exactly, or switching a channel over to
    /// presets would silently change its behaviour. Expected values are
    /// spelled out — not recomputed — so this pins behaviour, not arithmetic.
    #[test]
    fn resolve_matches_the_file_reinjector_env_helper() {
        let config = config_for("preset-test-model");
        let full = |max_files: usize, token_budget: usize| FileReinjectSpec {
            max_files,
            token_budget,
            per_file_budget: 5_000,
        };
        let cases: [(Option<&str>, Option<&str>, Option<FileReinjectSpec>); 8] = [
            (None, None, Some(full(5, 50_000))),
            (None, Some("1234"), Some(full(5, 1_234))),
            (None, Some("nope"), Some(full(5, 50_000))),
            (Some("7"), None, Some(full(7, 50_000))),
            (Some("7"), Some("1234"), Some(full(7, 1_234))),
            (Some("0"), None, None),
            (Some("off"), Some("1234"), None),
            (Some("nope"), None, None),
        ];
        for (files, budget, expected) in cases {
            let env = PresetEnv {
                reinject_files: files.map(str::to_string),
                reinject_file_budget: budget.map(str::to_string),
                ..PresetEnv::default()
            };
            assert_eq!(
                STANDARD
                    .resolve(&config, &env)
                    .context
                    .reinject_recent_files,
                expected,
                "files={files:?} budget={budget:?}"
            );
        }
    }

    /// Same contract for skills — including its deliberate difference: here an
    /// unparsable value means "on with the default budget", not "off".
    #[test]
    fn resolve_matches_the_skill_reinjector_env_helper() {
        let config = config_for("preset-test-model");
        let full = |token_budget: usize| SkillReinjectSpec {
            token_budget,
            per_skill_budget: 5_000,
        };
        let cases: [(Option<&str>, Option<&str>, Option<SkillReinjectSpec>); 8] = [
            (None, None, Some(full(25_000))),
            (None, Some("4321"), Some(full(4_321))),
            (None, Some("nope"), Some(full(25_000))),
            (Some("1234"), None, Some(full(1_234))),
            (Some("1234"), Some("4321"), Some(full(1_234))),
            (Some("0"), None, None),
            (Some("false"), Some("4321"), None),
            (Some("nope"), None, Some(full(25_000))),
        ];
        for (skills, budget, expected) in cases {
            let env = PresetEnv {
                reinject_skills: skills.map(str::to_string),
                reinject_skill_budget: budget.map(str::to_string),
                ..PresetEnv::default()
            };
            assert_eq!(
                STANDARD
                    .resolve(&config, &env)
                    .context
                    .reinject_invoked_skills,
                expected,
                "skills={skills:?} budget={budget:?}"
            );
        }
    }

    // ── selection ─────────────────────────────────────────────────────────

    #[test]
    fn selection_prefers_the_explicit_id_then_env_then_standard() {
        let env = PresetEnv::default();
        assert_eq!(select(None, &env).expect("default").id, "standard");

        let env = PresetEnv {
            agent_preset: Some("standard".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(select(None, &env).expect("env").id, "standard");

        let env = PresetEnv {
            agent_preset: Some("standard".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(
            select(Some("standard"), &env).expect("explicit").id,
            "standard"
        );
    }

    #[test]
    fn an_unknown_preset_id_is_an_error_listing_the_known_ids() {
        let err = select(Some("nope"), &PresetEnv::default()).expect_err("must reject");
        assert_eq!(err.id, "nope");
        assert_eq!(err.known, vec!["standard"]);
        assert!(err.to_string().contains("standard"));

        let env = PresetEnv {
            agent_preset: Some("also-nope".to_string()),
            ..PresetEnv::default()
        };
        assert!(select(None, &env).is_err());
    }

    /// The inventory is only useful if it names the real switches: every
    /// toggled row must carry an env var the resolver actually reads, and
    /// every env var the resolver reads must be discoverable in a row.
    #[test]
    fn capability_inventory_names_every_toggle() {
        let toggled: Vec<&str> = STANDARD
            .capabilities
            .iter()
            .map(|c| c.toggle)
            .filter(|t| !t.is_empty())
            .collect();
        for env in [
            COMPACT_THRESHOLD_ENV,
            MICROCOMPACT_TRIGGER_ENV,
            MAX_TRANSCRIPT_CHARS_ENV,
            REINJECT_FILES_ENV,
            REINJECT_SKILLS_ENV,
        ] {
            assert!(toggled.contains(&env), "{env} must appear in a row");
        }

        // Names are unique, and the preset id itself is selectable by env.
        let mut names: Vec<&str> = STANDARD.capabilities.iter().map(|c| c.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "capability names must be unique");
        assert_eq!(
            select(None, &PresetEnv::default()).expect("default").id,
            STANDARD.id
        );
    }

    #[test]
    fn disabled_by_default_capabilities_are_listed_explicitly() {
        let disabled: Vec<&str> = STANDARD
            .capabilities
            .iter()
            .filter(|c| c.default == CapabilityDefault::Disabled)
            .map(|c| c.name)
            .collect();
        for expected in [
            "proactive-tool-result-pruning",
            "transcript-char-cap",
            "subagent-delegation",
            "self-scheduling-wakeup",
        ] {
            assert!(
                disabled.contains(&expected),
                "{expected} exists but is off by default — it must be discoverable"
            );
        }
    }

    // ── assembly ──────────────────────────────────────────────────────────

    #[test]
    fn apply_installs_the_declared_assembly_and_stamps_the_preset_id() {
        std::env::remove_var(COMPACT_THRESHOLD_ENV);
        let config = config_for("preset-test-model");
        let mut resolved = STANDARD.resolve(&config, &PresetEnv::default());
        resolved.context.microcompaction = Some(MicrocompactionSpec {
            trigger_tool_count: 33,
            keep_recent: 2,
        });
        resolved.context.max_transcript_chars = Some(4096);

        let read_state: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let builder = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new()
                .with_read_state(read_state)
                .with_skills(vec![crate::skills::skill_from_content(
                    "s",
                    "---\nname: s\ndescription: d\n---\n\nbody",
                    Vec::new(),
                )]),
            ChannelSupport { interactive: true },
        );

        assert_eq!(builder.preset_id(), Some("standard"));
        assert_eq!(builder.context_management_facts(), resolved.context);

        // A channel with no shared read state has nothing to re-inject: the
        // file half of the declaration is skipped rather than installed inert.
        let without_state = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new(),
            ChannelSupport { interactive: true },
        );
        let facts = without_state.context_management_facts();
        assert!(facts.reinject_recent_files.is_none());
        assert_eq!(
            facts.reinject_invoked_skills, resolved.context.reinject_invoked_skills,
            "the skill half only needs the catalog, which is always available"
        );
        assert_eq!(facts.compaction, resolved.context.compaction);
    }

    #[test]
    fn a_channel_that_cannot_prompt_never_gets_plan_mode_tools() {
        let config = config_for("preset-test-model");
        let resolved = STANDARD.resolve(&config, &PresetEnv::default());
        assert!(resolved.tools.plan_mode_tools, "the preset asks for them");

        // Non-interactive: the blocking plan tools stay out of the registry.
        let headless = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new(),
            ChannelSupport { interactive: false },
        );
        assert!(
            !headless.with_plan_mode_tools_for_test(),
            "a headless channel must not register blocking plan tools"
        );

        let interactive = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new(),
            ChannelSupport { interactive: true },
        );
        assert!(interactive.with_plan_mode_tools_for_test());
    }

    /// Acceptance 3: a new preset is one declaration, no builder branch.
    /// Two different declarations run through the same [`apply`] and produce
    /// exactly what they declare.
    #[test]
    fn a_new_preset_is_one_declaration_with_no_builder_branch() {
        let config = config_for("preset-test-model");
        let lean = AgentPreset {
            id: "lean-test-only",
            description: "test fixture",
            prompt: PromptProfile {
                persona_suffix: Some("be terse"),
                auto_skill_injection: false,
            },
            tools: ToolProfile {
                plan_mode_tools: false,
            },
            context: ContextProfile {
                compaction: Some(CompactionMode::Chars(1000)),
                microcompaction: Some(MicrocompactionSpec {
                    trigger_tool_count: 5,
                    keep_recent: 1,
                }),
                max_transcript_chars: Some(2048),
                reinject_recent_files: None,
                reinject_invoked_skills: None,
            },
            capabilities: &[],
        };
        let resolved = lean.resolve(&config, &PresetEnv::default());
        let read_state: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let builder = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new().with_read_state(read_state),
            ChannelSupport { interactive: true },
        );
        assert_eq!(builder.context_management_facts(), resolved.context);
        assert_eq!(builder.preset_id(), Some("lean-test-only"));
        assert!(!builder.with_plan_mode_tools_for_test());
        assert_eq!(
            apply_prompt("base".to_string(), &resolved),
            "base\nbe terse"
        );
        // ...and the standard preset is unaffected by that declaration.
        let standard_facts = STANDARD.resolve(&config, &PresetEnv::default()).context;
        assert_ne!(standard_facts, resolved.context);
    }

    #[test]
    fn prompt_suffix_is_a_no_op_for_the_standard_preset() {
        let config = config_for("preset-test-model");
        let resolved = STANDARD.resolve(&config, &PresetEnv::default());
        assert_eq!(apply_prompt("base".into(), &resolved), "base");
    }

    #[test]
    fn assets_are_taken_from_the_registry_read_state() {
        let registry = crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30);
        let assets = assets_from_registry(&registry, Vec::new());
        assert!(assets.read_state.is_some());
        let without = assets_from_registry(&crate::tools::ToolRegistry::default(), Vec::new());
        assert!(without.read_state.is_none());
    }
}
