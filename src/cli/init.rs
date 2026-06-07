//! Interactive setup wizard.

use std::io::{self, Write};
use std::path::Path;

use recursive::providers::ProviderPreset;

/// Result of resolving a user's provider selection in the wizard.
#[derive(Debug)]
pub(crate) enum PresetChoice<'a> {
    /// User picked a catalog preset (by number, id, or default).
    Preset(&'a ProviderPreset),
    /// User typed "0" — wants to specify a custom API base. The wizard
    /// handles the follow-up prompt; this variant carries no data.
    Manual,
}

/// Pure function: map the user's input string to a preset or manual override.
///
/// Resolution rules (kept in one place so the wizard and any future
/// non-interactive caller stay aligned):
/// - Empty / "1" / unknown / out-of-range number → `default_preset`
/// - "0" → caller is responsible for prompting for the API base; the
///   `Manual` variant here is a marker for the wizard to take that path
/// - "deepseek" (a string id) → matched preset
/// - "2", "3", ... → the corresponding entry in `presets`
pub(crate) fn resolve_preset_choice<'a>(
    input: &str,
    presets: &[&'a ProviderPreset],
    default_preset: &'a ProviderPreset,
) -> PresetChoice<'a> {
    if input == "0" {
        return PresetChoice::Manual;
    }
    if input.is_empty() {
        return PresetChoice::Preset(default_preset);
    }
    if let Ok(idx) = input.parse::<usize>() {
        if idx >= 1 && idx <= presets.len() {
            return PresetChoice::Preset(presets[idx - 1]);
        }
    }
    if let Some(p) = recursive::find_preset(input) {
        return PresetChoice::Preset(p);
    }
    PresetChoice::Preset(default_preset)
}

/// Try to recover the user's current preset from an existing config file.
/// Used to pre-select when re-running `recursive init`. Returns a
/// one-line human-readable summary like `deepseek (deepseek-chat)` or None.
fn detect_current_preset(config_path: &Path) -> Option<String> {
    if !config_path.exists() {
        return None;
    }
    let cfg = recursive::config_file::FileConfig::load_from(config_path)
        .ok()
        .flatten()?;
    let provider = cfg.provider?;
    if let Some(preset_id) = provider.preset.as_deref() {
        if let Some(preset) = recursive::find_preset_extended(preset_id) {
            return Some(format!(
                "preset={} (model={}, api_base={})",
                preset.id, preset.default_model, preset.api_base
            ));
        }
        return Some(format!("preset={} (not in catalog)", preset_id));
    }
    if let Some(api_base) = provider.api_base.as_deref() {
        if let Some(preset) = recursive::providers::find_preset_by_api_base(api_base) {
            return Some(format!(
                "preset={} (by api_base match, model={})",
                preset.id,
                provider.model.as_deref().unwrap_or(&preset.default_model)
            ));
        }
    }
    Some(format!(
        "model={}, api_base={}",
        provider.model.as_deref().unwrap_or("(none)"),
        provider.api_base.as_deref().unwrap_or("(none)")
    ))
}

/// Interactive setup wizard: walk the user through provider/model/key config.
///
/// `provider_prefill` / `model_prefill` / `api_key_prefill` come from the
/// non-interactive CLI flags (`--provider` / `--model` / `--api-key`). When
/// all three are `Some`, the wizard writes the config directly without
/// prompting. When only some are set, the supplied values pre-fill the
/// prompts and the user is asked for the rest.
pub(crate) async fn run_init(
    provider_prefill: Option<String>,
    model_prefill: Option<String>,
    api_key_prefill: Option<String>,
) -> anyhow::Result<()> {
    let config_path = recursive::config_file::config_file_path()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    if let Some(current) = detect_current_preset(&config_path) {
        println!("  Existing config: {}", config_path.display());
        println!("  Current: {current}\n");
    } else {
        println!();
    }

    // 1. Vendor selection. Either use the --provider prefill (must match a
    // known preset id), or walk the user through the interactive list.
    let mut input = String::new();
    let (provider_type, api_base, default_model, key_env, key_url, resolved_preset_id) =
        match provider_prefill.as_deref() {
            Some(id) => {
                let preset = recursive::find_preset_extended(id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--provider {:?} not found in providers catalog. Valid ids: {}",
                        id,
                        recursive::all_presets_dynamic()
                            .iter()
                            .map(|p| p.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                println!("  Provider (from --provider): {}", preset.name);
                (
                    preset.provider_type.clone(),
                    preset.api_base.clone(),
                    preset.default_model.clone(),
                    preset.key_env.clone(),
                    preset.key_url.clone(),
                    Some(preset.id.clone()),
                )
            }
            None => {
                // Interactive selection from the preset catalog.
                let presets = recursive::all_presets();
                // Default to the user override of "anthropic" if one
                // exists in providers.d/; otherwise fall back to the
                // bundled anthropic preset. resolve_preset_choice only
                // uses this for the empty-input / unknown-input case,
                // so a runtime-owned `ProviderPreset` is fine — we borrow
                // it when passing into the helper.
                let anthropic_preset = recursive::find_preset_extended("anthropic")
                    .or_else(|| recursive::find_preset("anthropic").cloned())
                    .ok_or_else(|| {
                        anyhow::anyhow!("anthropic preset must be present in bundled catalog")
                    })?;

                println!("Select a provider (or press Enter for Anthropic):\n");

                let international: Vec<_> =
                    presets.iter().filter(|p| !p.mainland_accessible).collect();
                let mainland: Vec<_> = presets
                    .iter()
                    .filter(|p| p.mainland_accessible && p.id != "ollama")
                    .collect();
                let local: Vec<_> = presets.iter().filter(|p| p.id == "ollama").collect();

                let mut all_entries: Vec<&recursive::ProviderPreset> = Vec::new();

                println!("  International:");
                for (i, p) in international.iter().enumerate() {
                    let num = i + 1;
                    let key_hint = if p.key_env.is_empty() {
                        "(no key needed)".to_string()
                    } else {
                        format!("[{}]", p.key_env)
                    };
                    println!(
                        "    {num:>2}) {:<22} {:<26} {}",
                        p.name, p.default_model, key_hint
                    );
                    all_entries.push(p);
                }

                println!("\n  Mainland China (直连):");
                for (i, p) in mainland.iter().enumerate() {
                    let num = all_entries.len() + i + 1;
                    let key_hint = if p.key_env.is_empty() {
                        "(no key needed)".to_string()
                    } else {
                        format!("[{}]", p.key_env)
                    };
                    println!(
                        "    {num:>2}) {:<22} {:<26} {}",
                        p.name, p.default_model, key_hint
                    );
                    all_entries.push(p);
                }

                println!("\n  Local:");
                for (i, p) in local.iter().enumerate() {
                    let num = all_entries.len() + i + 1;
                    let key_hint = if p.key_env.is_empty() {
                        "(no key needed)".to_string()
                    } else {
                        format!("[{}]", p.key_env)
                    };
                    println!(
                        "    {num:>2}) {:<22} {:<26} {}",
                        p.name, p.default_model, key_hint
                    );
                    all_entries.push(p);
                }

                println!("\n  Other: enter 0 to specify custom API base manually");
                print!("\nChoice [1]: ");
                io::stdout().flush()?;
                io::stdin().read_line(&mut input)?;
                let trimmed = input.trim();

                // Manual-mode path: prompt for the API base, then look up
                // the preset by URL so the catalog drives type/default_model
                // instead of brittle substring matching. The hardcoded
                // substring heuristic is kept only as a last-resort fallback.
                let choice = resolve_preset_choice(trimmed, &all_entries, &anthropic_preset);
                match choice {
                    PresetChoice::Preset(p) => (
                        p.provider_type.clone(),
                        p.api_base.clone(),
                        p.default_model.clone(),
                        p.key_env.clone(),
                        p.key_url.clone(),
                        Some(p.id.clone()),
                    ),
                    PresetChoice::Manual => {
                        println!("\nAPI base URL");
                        print!("API base: ");
                        io::stdout().flush()?;
                        input.clear();
                        io::stdin().read_line(&mut input)?;
                        let manual_base = input.trim().to_string();

                        if let Some(preset) =
                            recursive::providers::find_preset_by_api_base(&manual_base)
                        {
                            println!(
                                "  Matched preset: {} (default_model={})",
                                preset.id, preset.default_model
                            );
                            (
                                preset.provider_type.clone(),
                                manual_base,
                                preset.default_model.clone(),
                                preset.key_env.clone(),
                                preset.key_url.clone(),
                                Some(preset.id.clone()),
                            )
                        } else {
                            let manual_provider_type = if manual_base.contains("anthropic") {
                                "anthropic"
                            } else {
                                "openai"
                            };
                            let manual_default_model = if manual_base.contains("deepseek") {
                                "deepseek-chat"
                            } else if manual_base.contains("bigmodel") {
                                "glm-4-flash"
                            } else if manual_base.contains("anthropic") {
                                "claude-sonnet-4-6"
                            } else if manual_base.contains("localhost")
                                || manual_base.contains("11434")
                            {
                                "qwen2.5-coder"
                            } else {
                                "gpt-4o-mini"
                            };
                            (
                                manual_provider_type.to_string(),
                                manual_base,
                                manual_default_model.to_string(),
                                String::new(),
                                String::new(),
                                None,
                            )
                        }
                    }
                }
            }
        };

    // 2. Model — prefill wins, otherwise prompt with the preset's default.
    let model = if let Some(m) = model_prefill {
        println!("  Model (from --model): {m}");
        m
    } else {
        print!("\nModel [{}]: ", default_model);
        io::stdout().flush()?;
        input.clear();
        io::stdin().read_line(&mut input)?;
        if input.trim().is_empty() {
            default_model.clone()
        } else {
            input.trim().to_string()
        }
    };

    // 3. API key. Precedence: --api-key flag > preset's key_env env var >
    // interactive prompt. The flag IS written to the file (explicit user
    // choice); env-var prefill is NOT (the env wins at runtime; persisting
    // would risk staleness if the env later changes).
    let mut api_key_was_env_prefilled = false;
    let api_key = if let Some(k) = api_key_prefill {
        println!("  API key (from --api-key): set (won't echo)");
        k
    } else if key_env.is_empty() {
        String::new()
    } else {
        match std::env::var(&key_env) {
            Ok(existing) if !existing.is_empty() => {
                println!("\n  ✓ {key_env} detected, using it (skipping write to config file).");
                println!("    To override, pass --api-key or unset {key_env}.");
                api_key_was_env_prefilled = true;
                existing
            }
            _ => {
                if !key_url.is_empty() {
                    println!("\n  Get your key at: {key_url}");
                }
                print!("\nAPI key ({}): ", key_env);
                io::stdout().flush()?;
                input.clear();
                io::stdin().read_line(&mut input)?;
                let key = input.trim().to_string();
                if key.is_empty() {
                    println!("\n  Warning: no API key set. Add it later with one of:");
                    println!("    recursive config set provider.api_key <KEY>");
                    println!("    export {key_env}=<KEY>");
                }
                key
            }
        }
    };

    // Write config. When the provider came from a known preset we persist
    // `provider.preset` so subsequent runs can re-resolve api_base / type
    // from the catalog (and `Config::from_env` won't silently fall through
    // to the hardcoded defaults). Manual paths keep the 4 explicit fields.
    if let Some(preset_id) = resolved_preset_id.as_deref() {
        recursive::config_file::set_value("provider.preset", preset_id)?;
    } else {
        recursive::config_file::set_value("provider.type", &provider_type)?;
        recursive::config_file::set_value("provider.api_base", &api_base)?;
    }
    recursive::config_file::set_value("provider.model", &model)?;
    if !api_key.is_empty() && !api_key_was_env_prefilled {
        recursive::config_file::set_value("provider.api_key", &api_key)?;
    }

    println!("\n  Config saved to: {}", config_path.display());
    println!("\n  You can now run:");
    println!("    recursive                — interactive REPL");
    println!("    recursive -p \"hello\"     — one-shot");
    println!("    recursive config show    — verify settings");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_presets() -> Vec<&'static ProviderPreset> {
        // Caller (resolve_preset_choice) accepts `&ProviderPreset`;
        // the `&'static` here is just a stricter bound that still
        // satisfies it.
        recursive::all_presets().iter().collect()
    }
    fn default_preset() -> &'static ProviderPreset {
        recursive::find_preset("anthropic").unwrap()
    }

    #[test]
    fn empty_input_returns_default() {
        let presets = all_presets();
        match resolve_preset_choice("", &presets, default_preset()) {
            PresetChoice::Preset(p) => assert_eq!(p.id, "anthropic"),
            other => panic!("expected Preset(anthropic), got {other:?}"),
        }
    }

    #[test]
    fn numeric_one_returns_first() {
        let presets = all_presets();
        let first = presets[0].id.clone();
        match resolve_preset_choice("1", &presets, default_preset()) {
            PresetChoice::Preset(p) => assert_eq!(p.id, first),
            other => panic!("expected Preset({first}), got {other:?}"),
        }
    }

    #[test]
    fn numeric_out_of_range_falls_back_to_default() {
        let presets = all_presets();
        match resolve_preset_choice("999", &presets, default_preset()) {
            PresetChoice::Preset(p) => assert_eq!(p.id, "anthropic"),
            other => panic!("expected Preset(anthropic), got {other:?}"),
        }
    }

    #[test]
    fn string_id_match() {
        let presets = all_presets();
        match resolve_preset_choice("deepseek", &presets, default_preset()) {
            PresetChoice::Preset(p) => assert_eq!(p.id, "deepseek"),
            other => panic!("expected Preset(deepseek), got {other:?}"),
        }
    }

    #[test]
    fn string_id_no_match_falls_back_to_default() {
        let presets = all_presets();
        match resolve_preset_choice("nonsense", &presets, default_preset()) {
            PresetChoice::Preset(p) => assert_eq!(p.id, "anthropic"),
            other => panic!("expected Preset(anthropic), got {other:?}"),
        }
    }

    #[test]
    fn zero_triggers_manual() {
        let presets = all_presets();
        match resolve_preset_choice("0", &presets, default_preset()) {
            PresetChoice::Manual => {}
            other => panic!("expected Manual, got {other:?}"),
        }
    }
}
