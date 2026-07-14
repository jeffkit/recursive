//! Interactive setup wizard.

use std::io::{self, Write};
use std::path::Path;

use recursive::providers::ProviderPreset;

/// Sanitize a user-typed string into a preset id slug. Lowercases,
/// replaces anything non-`[a-z0-9-_]` with `-`, collapses runs, and
/// trims leading/trailing `-`. Returns `"manual"` if the result is empty
/// so we never write a file named `.toml`.
fn slugify_preset_id(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for c in input.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() || lc == '-' || lc == '_' {
            if lc == '-' && last_dash {
                continue;
            }
            out.push(lc);
            last_dash = lc == '-';
        } else if !out.is_empty() && !last_dash {
            // Both whitespace and "other" characters collapse to the
            // same dash separator — keeps the slug readable from any
            // user-typed id (vendor name, URL path, …).
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "manual".to_string()
    } else {
        trimmed
    }
}

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

/// Look up the default model for a manually-typed API base URL by matching
/// it against the bundled catalog. Replaces a previous string-contains
/// heuristic that guessed at models from URL substrings (deepseek,
/// bigmodel, anthropic, localhost/11434) — fragile and drift-prone.
///
/// Returns an empty string when no bundled preset matches the URL; the
/// wizard's existing model prompt will then ask the user instead of
/// silently writing a guessed name.
pub(crate) fn detect_model_from_api_base(api_base: &str) -> String {
    recursive::providers::find_preset_by_api_base(api_base)
        .map(|p| p.default_model.clone())
        .unwrap_or_default()
}

/// One-line summary of the models available under `preset`, designed
/// for the wizard's provider list so the user can see more than just
/// `default_model`. Examples:
///
///   claude-sonnet-4-6 (+2 more)
///   MiniMax-M3 (+1 more)
///   gpt-5.4 (and 5 more)
///
/// Truncated at `max_models_shown` to keep the table aligned. Omitted
/// when the preset only declares `default_model` (e.g. user overrides
/// written by `recursive init`'s manual flow before the user has filled
/// in `models[]`).
pub(crate) fn format_models_brief(preset: &recursive::providers::ProviderPreset) -> String {
    const MAX_SHOWN: usize = 2;
    let total = preset.models.len();
    if total <= 1 {
        return preset.default_model.clone();
    }
    let others: Vec<&str> = preset
        .models
        .iter()
        .filter(|m| m.name != preset.default_model)
        .take(MAX_SHOWN)
        .map(|m| m.name.as_str())
        .collect();
    let more = total.saturating_sub(1 + others.len());
    if others.is_empty() {
        preset.default_model.clone()
    } else if more == 0 {
        format!("{} (+{} more)", preset.default_model, others.len())
    } else {
        format!(
            "{} (+{} more, {} extra)",
            preset.default_model,
            others.len(),
            more
        )
    }
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

    // Goal 351 onboarding-smooth: refresh the remote provider catalog
    // up front so the list the user sees below includes the latest
    // upstream models instead of the bundled build-time snapshot.
    // Fail-soft: 5 second timeout, offline / partial-failure falls back
    // to the cached or bundled catalog without surfacing the error.
    // `recursive init` is an explicit, interactive invocation — unlike
    // one-shot commands, the user *expects* to wait for a refresh.
    if recursive::providers_cache::needs_update() {
        let url = recursive::providers_cache::configured_url();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            recursive::providers_cache::fetch_and_save(&url),
        )
        .await;
        match outcome {
            Ok(Ok(cache)) => {
                println!(
                    "  Refreshed {} preset(s) from upstream catalog.\n",
                    cache.providers.len()
                );
            }
            Ok(Err(_)) | Err(_) => {
                // Don't surface the error — offline / 5xx / SSRF-guard
                // rejection are all expected on at least some networks.
                // The bundled catalog is a complete-enough fallback; the
                // user can run `recursive providers update` later.
                println!(
                    "  Note: could not refresh remote provider catalog \
                     (using bundled list). Run `recursive providers update` \
                     later to retry.\n"
                );
            }
        }
    }

    // 1. Vendor selection. Either use the --provider prefill (must match a
    // known preset id), or walk the user through the interactive list.
    let mut input = String::new();
    let mut manual_preset: Option<recursive::providers::ProviderPreset> = None;
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
                        "    {num:>2}) {:<22} {:<46} {}",
                        p.name,
                        format_models_brief(p),
                        key_hint
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
                        "    {num:>2}) {:<22} {:<46} {}",
                        p.name,
                        format_models_brief(p),
                        key_hint
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
                        "    {num:>2}) {:<22} {:<46} {}",
                        p.name,
                        format_models_brief(p),
                        key_hint
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
                            // No bundled preset matches — collect enough
                            // information to (a) configure this run and (b)
                            // optionally persist a reusable user override
                            // under `~/.recursive/providers.d/<id>.toml`.
                            // Collecting the id / display_name / key_env up
                            // front lets the persistence step at the bottom
                            // happen without re-prompting.
                            let manual_provider_type = if manual_base.contains("anthropic") {
                                "anthropic"
                            } else {
                                "openai"
                            };
                            let manual_default_model = detect_model_from_api_base(&manual_base);

                            print!("\nVendor id (lowercase, e.g. \"myvendor\"): ");
                            io::stdout().flush()?;
                            input.clear();
                            io::stdin().read_line(&mut input)?;
                            let vendor_id = slugify_preset_id(input.trim());

                            print!("Display name (e.g. \"My Vendor\"): ");
                            io::stdout().flush()?;
                            input.clear();
                            io::stdin().read_line(&mut input)?;
                            let display_name = if input.trim().is_empty() {
                                vendor_id.clone()
                            } else {
                                input.trim().to_string()
                            };

                            print!("Env var name for the API key [MYVENDOR_API_KEY]: ");
                            io::stdout().flush()?;
                            input.clear();
                            io::stdin().read_line(&mut input)?;
                            let manual_key_env = if input.trim().is_empty() {
                                format!("{}_API_KEY", vendor_id.to_uppercase())
                            } else {
                                input.trim().to_string()
                            };

                            println!(
                                "\n  Save as a reusable preset under \
                                 ~/.recursive/providers.d/{vendor_id}.toml? \
                                 (You can edit or delete it later.) [Y/n]"
                            );
                            print!("Persist preset [Y]: ");
                            io::stdout().flush()?;
                            input.clear();
                            io::stdin().read_line(&mut input)?;
                            let persist_preset =
                                !matches!(input.trim().to_ascii_lowercase().as_str(), "n" | "no");

                            if persist_preset {
                                manual_preset = Some(recursive::providers::ProviderPreset {
                                    id: vendor_id.clone(),
                                    name: display_name,
                                    provider_type: manual_provider_type.to_string(),
                                    api_base: manual_base.clone(),
                                    anthropic_api_base: None,
                                    default_model: manual_default_model.clone(),
                                    models: Vec::new(),
                                    mainland_accessible: false,
                                    key_env: manual_key_env.clone(),
                                    key_url: String::new(),
                                });
                            }

                            (
                                manual_provider_type.to_string(),
                                manual_base,
                                manual_default_model,
                                manual_key_env,
                                String::new(),
                                if persist_preset {
                                    Some(vendor_id)
                                } else {
                                    None
                                },
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
    // interactive prompt. The flag is written to ~/.recursive/secrets.env
    // (mode 0600, shell-sourceable) — NOT to ~/.recursive/config.toml.
    // A key on disk in config.toml can be `cat`'d by an agent's
    // `run_shell` tool and end up in a tracked .dev/journal/*.md; the
    // secrets.env file is never read by the binary, only sourced by the
    // user's shell, so the key only lives in process env. (L1 fix.)
    let mut api_key_was_env_prefilled = false;
    let api_key = if let Some(k) = api_key_prefill {
        println!("  API key (from --api-key): set (won't echo)");
        k
    } else if key_env.is_empty() {
        String::new()
    } else {
        match std::env::var(&key_env) {
            Ok(existing) if !existing.is_empty() => {
                println!("\n  ✓ {key_env} detected, using it (skipping write to secrets file).");
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
                    println!("    recursive config set-secret {key_env} <KEY>");
                    println!("    export {key_env}=<KEY>");
                }
                key
            }
        }
    };

    // Persist the user override (manual flow with persist_preset=Y) before
    // we touch the config file: a successful write proves the id is unique
    // and the file is on disk, so `provider.preset = <id>` below resolves
    // against the just-written catalog. Failures here are surfaced but do
    // not abort the run — without the toml the wizard still writes the
    // explicit provider.type/api_base keys so the user's `recursive`
    // invocation works on this machine.
    if let Some(preset) = manual_preset.as_ref() {
        let mut snapshot = preset.clone();
        snapshot.default_model = model.clone();
        if !snapshot.models.iter().any(|m| m.name == model) {
            snapshot.models.push(recursive::providers::ModelSpec {
                name: model.clone(),
                context_window: 0,
                pricing: None,
            });
        }
        match recursive::providers::write_user_preset(&snapshot) {
            Ok(written) => {
                println!("\n  ✓ Preset saved to {}", written.path.display());
                println!("    Edit it any time to add models, change pricing, etc.");
            }
            Err(e) => {
                println!("\n  Warning: could not save preset to providers.d: {e}");
                println!("    Falling back to inline config (this run still works).");
            }
        }
    }

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
        // Route the secret to ~/.recursive/secrets.env (mode 0600),
        // not to ~/.recursive/config.toml. The binary reads the env
        // at runtime, never the file. (L1 fix.)
        recursive::config_file::set_secret(&key_env, &api_key)?;
        let secrets_path = recursive::config_file::secrets_env_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.recursive/secrets.env".to_string());
        println!("\n  ✓ API key saved to {secrets_path}");
        println!("    Add to your shell rc:  source ~/.recursive/secrets.env");
        println!("    (or `export {key_env}=<key>` directly)");
    }

    println!("\n  Config saved to: {}", config_path.display());
    println!("\n  You can now run:");
    println!("    recursive                — interactive REPL");
    println!("    recursive -p \"hello\"     — one-shot");
    println!("    recursive config show    — verify settings");

    println!("\n  Manage providers:");
    println!("    recursive providers list    — show bundled + your custom presets");
    println!("    recursive providers update  — pull the latest upstream catalog");
    println!("    ~/.recursive/providers.d/  — drop a <id>.toml to add a vendor");

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

    #[test]
    fn init_default_model_detect_from_api_base_deepseek() {
        // Previously the heuristic returned the hardcoded "deepseek-chat";
        // the catalog now lists "deepseek-v4-flash". The helper must
        // follow the catalog.
        let from_helper = detect_model_from_api_base("https://api.deepseek.com/v1");
        let from_catalog = recursive::find_preset("deepseek")
            .unwrap()
            .default_model
            .clone();
        assert_eq!(from_helper, from_catalog);
        assert!(!from_helper.is_empty());
    }

    #[test]
    fn init_default_model_detect_from_api_base_openai() {
        // Previously the heuristic fell through to "gpt-4o-mini" for
        // OpenAI; the catalog now lists "gpt-5.4". The helper must
        // follow the catalog.
        let from_helper = detect_model_from_api_base("https://api.openai.com/v1");
        let from_catalog = recursive::find_preset("openai")
            .unwrap()
            .default_model
            .clone();
        assert_eq!(from_helper, from_catalog);
        assert!(!from_helper.is_empty());
    }

    #[test]
    fn init_default_model_detect_from_api_base_unknown_is_empty() {
        // No catalog match → empty string. The wizard's prompt handles
        // empty defaults by asking the user.
        assert_eq!(detect_model_from_api_base("https://example.com/v1"), "");
    }

    // Goal-351: slugify_preset_id sanitises user input into a filesystem-
    // safe preset id. The wizard's "save as reusable preset?" flow
    // calls this before writing to providers.d/<id>.toml, so weird
    // inputs (spaces, slashes, uppercase, leading dashes) must not
    // produce a path-traversal vector or an empty filename.

    #[test]
    fn slugify_simple_id() {
        assert_eq!(slugify_preset_id("myvendor"), "myvendor");
    }

    #[test]
    fn slugify_lowercases_and_replaces_spaces() {
        assert_eq!(slugify_preset_id("My Vendor"), "my-vendor");
    }

    #[test]
    fn slugify_replaces_path_traversal_chars() {
        assert_eq!(slugify_preset_id("../../etc/passwd"), "etc-passwd");
        assert_eq!(slugify_preset_id(".."), "manual");
    }

    #[test]
    fn slugify_trims_leading_and_trailing_dashes() {
        assert_eq!(slugify_preset_id("---foo---"), "foo");
        assert_eq!(slugify_preset_id("!!!"), "manual");
    }

    #[test]
    fn slugify_keeps_underscore() {
        assert_eq!(slugify_preset_id("my_vendor"), "my_vendor");
    }

    #[test]
    fn slugify_preserves_digits() {
        assert_eq!(slugify_preset_id("vendor7"), "vendor7");
    }

    // Goal-351: format_models_brief labels each provider with the
    // default model plus a "+N more" hint so the user can see the
    // vendor offers more than one model without leaving the wizard.
    // The helper is pure and just reads the preset, so test it
    // directly without dragging in the bundled catalog.

    use recursive::providers::ProviderPreset as PP;
    use recursive::providers::{ModelPricingSpec, ModelSpec};

    fn preset_with(id: &str, models: &[(&str, usize)], default: &str) -> PP {
        PP {
            id: id.to_string(),
            name: id.to_string(),
            provider_type: "openai".to_string(),
            api_base: format!("https://{id}.example.com/v1"),
            anthropic_api_base: None,
            default_model: default.to_string(),
            models: models
                .iter()
                .map(|(name, ctx)| ModelSpec {
                    name: (*name).to_string(),
                    context_window: *ctx,
                    pricing: Some(ModelPricingSpec {
                        input_per_million: 0.0,
                        output_per_million: 0.0,
                        cache_hit_input_per_million: None,
                    }),
                })
                .collect(),
            mainland_accessible: false,
            key_env: "K".to_string(),
            key_url: "".to_string(),
        }
    }

    #[test]
    fn format_models_brief_only_default() {
        let p = preset_with("fresh", &[("fresh-1", 8_000)], "fresh-1");
        // No "more" when there's only the default — keeps the table
        // column from buzzing with redundant text.
        assert_eq!(format_models_brief(&p), "fresh-1");
    }

    #[test]
    fn format_models_brief_two_models_one_default_one_extra() {
        let p = preset_with(
            "two",
            &[("two-default", 8_000), ("two-mini", 4_000)],
            "two-default",
        );
        assert_eq!(format_models_brief(&p), "two-default (+1 more)");
    }

    #[test]
    fn format_models_brief_many_models_shows_extra_tail() {
        let p = preset_with(
            "many",
            &[
                ("many-default", 8_000),
                ("m1", 4_000),
                ("m2", 4_000),
                ("m3", 4_000),
            ],
            "many-default",
        );
        // MAX_SHOWN is 2 extras, so we report "2 more" plus "1 extra"
        // for the third+ fourth non-default models.
        assert_eq!(format_models_brief(&p), "many-default (+2 more, 1 extra)");
    }
}
