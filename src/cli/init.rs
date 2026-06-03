//! Interactive setup wizard.

/// Interactive setup wizard: walk the user through provider/model/key config.
pub(crate) async fn run_init() -> anyhow::Result<()> {
    use std::io::{self, Write};

    println!("recursive init — interactive setup\n");

    let config_path = recursive::config_file::config_file_path()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    if config_path.exists() {
        println!("  Existing config: {}\n", config_path.display());
    }

    // 1. Vendor selection via preset catalog
    let presets = recursive::all_presets();
    let anthropic_preset = recursive::find_preset("anthropic").unwrap();

    println!("Select a provider (or press Enter for Anthropic):\n");

    // Group by mainland_accessible
    let international: Vec<_> = presets.iter().filter(|p| !p.mainland_accessible).collect();
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
            "    {num:>2}) {:<20} {:<30} {}",
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
            "    {num:>2}) {:<20} {:<30} {}",
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
            "    {num:>2}) {:<20} {:<30} {}",
            p.name, p.default_model, key_hint
        );
        all_entries.push(p);
    }

    println!("\n  Other: enter 0 to specify custom API base manually");
    print!("\nChoice [1]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();

    let (provider_type, api_base, default_model, key_env, key_url) = if trimmed == "0" {
        // Manual fallback
        println!("\nAPI base URL");
        print!("API base: ");
        io::stdout().flush()?;
        input.clear();
        io::stdin().read_line(&mut input)?;
        let manual_base = input.trim().to_string();

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
            "claude-sonnet-4-20250514"
        } else if manual_base.contains("localhost") || manual_base.contains("11434") {
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
        )
    } else if trimmed.is_empty() {
        // Default: Anthropic
        (
            anthropic_preset.provider_type.clone(),
            anthropic_preset.api_base.clone(),
            anthropic_preset.default_model.clone(),
            anthropic_preset.key_env.clone(),
            anthropic_preset.key_url.clone(),
        )
    } else {
        // Try numeric selection
        let idx: usize = trimmed.parse().unwrap_or(1);
        let preset = if idx > 0 && idx <= all_entries.len() {
            all_entries[idx - 1]
        } else {
            // Try matching by id
            recursive::find_preset(trimmed).unwrap_or(anthropic_preset)
        };
        (
            preset.provider_type.clone(),
            preset.api_base.clone(),
            preset.default_model.clone(),
            preset.key_env.clone(),
            preset.key_url.clone(),
        )
    };

    // 2. Model (with preset default)
    print!("\nModel [{}]: ", default_model);
    io::stdout().flush()?;
    input.clear();
    io::stdin().read_line(&mut input)?;
    let model = if input.trim().is_empty() {
        default_model.to_string()
    } else {
        input.trim().to_string()
    };

    // 3. API key (skip if key_env is empty, e.g. Ollama)
    let api_key = if key_env.is_empty() {
        String::new()
    } else {
        if !key_url.is_empty() {
            println!("\n  Get your key at: {key_url}");
        }
        print!("\nAPI key ({}): ", key_env);
        io::stdout().flush()?;
        input.clear();
        io::stdin().read_line(&mut input)?;
        let key = input.trim().to_string();
        if key.is_empty() {
            println!("\n  Warning: no API key set. You can add it later:");
            println!("    recursive config set provider.api_key <KEY>");
        }
        key
    };

    // Write config
    recursive::config_file::set_value("provider.type", &provider_type)?;
    recursive::config_file::set_value("provider.api_base", &api_base)?;
    recursive::config_file::set_value("provider.model", &model)?;
    if !api_key.is_empty() {
        recursive::config_file::set_value("provider.api_key", &api_key)?;
    }

    println!("\n  Config saved to: {}", config_path.display());
    println!("\n  You can now run:");
    println!("    recursive                — interactive REPL");
    println!("    recursive -p \"hello\"     — one-shot");
    println!("    recursive config show    — verify settings");

    Ok(())
}
