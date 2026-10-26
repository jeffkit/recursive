//! Issue #245 batch 3 (CI run 40256596759): behaviour pins for the mutants
//! inside `run_init`'s wizard body (`crates/recursive-cli/src/cli/init.rs`).
//!
//! The wizard is only observable through a real process — it reads prompts
//! from stdin, renders the provider catalog to stdout, and writes
//! config.toml / secrets.env / providers.d under the pinned RECURSIVE_HOME —
//! so every pin here drives the `recursive` binary as a subprocess, same rig
//! as cli_init_keeps_env.rs: cleared environment, everything rooted in a
//! tempdir, and the providers-catalog URL pointed at a refused loopback port
//! so the fail-soft refresh lands on the bundled catalog immediately.
//!
//! The expected wizard lines are recomputed from `recursive::all_presets()`
//! (the same compile-time catalog the binary renders — no disk, no env), so
//! the section pins stay exact across catalog drift.

use std::io::Write;
use std::process::{Command, Output, Stdio};

struct Rig {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            workspace: tempfile::tempdir().expect("workspace tempdir"),
        }
    }

    /// A `recursive` invocation with a cleared environment and everything
    /// rooted in the pinned home (see cli_init_keeps_env.rs for the
    /// providers-URL rationale).
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
        cmd.env_clear();
        cmd.env("HOME", self.home.path());
        cmd.env("RECURSIVE_HOME", self.home.path());
        cmd.env(
            "RECURSIVE_PROVIDERS_URL",
            "http://127.0.0.1:9/providers.json",
        );
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.current_dir(self.workspace.path());
        cmd.arg("--log").arg("error");
        cmd.arg("--workspace").arg(self.workspace.path());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    }

    /// Run `recursive <args>` answering the wizard's prompts from
    /// `stdin_script` (one prompt per line; EOF answers anything left with
    /// the empty default). Extra `env` entries ride on the cleared child
    /// environment.
    fn feed_with_env(&self, args: &[&str], stdin_script: &str, env: &[(&str, &str)]) -> Output {
        let mut cmd = self.cmd();
        cmd.args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn recursive");
        if let Some(stdin) = child.stdin.as_mut() {
            // A broken pipe here means the wizard died before finishing its
            // prompts — exactly the signal the pins below assert on, so the
            // write error itself is not fatal.
            let _ = stdin.write_all(stdin_script.as_bytes());
        }
        drop(child.stdin.take());
        child.wait_with_output().expect("wait for recursive")
    }

    fn feed(&self, args: &[&str], stdin_script: &str) -> Output {
        self.feed_with_env(args, stdin_script, &[])
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Mirror of `format_models_brief` (MAX_SHOWN = 2) — the integration test
/// cannot import the bin-internal helper, and the section pins below match
/// the wizard's rendered lines byte for byte.
fn brief(p: &recursive::providers::ProviderPreset) -> String {
    let total = p.models.len();
    if total <= 1 {
        return p.default_model.clone();
    }
    let others: Vec<&str> = p
        .models
        .iter()
        .filter(|m| m.name != p.default_model)
        .take(2)
        .map(|m| m.name.as_str())
        .collect();
    let more = total.saturating_sub(1 + others.len());
    if others.is_empty() {
        p.default_model.clone()
    } else if more == 0 {
        format!("{} (+{} more)", p.default_model, others.len())
    } else {
        format!(
            "{} (+{} more, {} extra)",
            p.default_model,
            others.len(),
            more
        )
    }
}

/// The wizard's full expected entry lines: International, then Mainland
/// China (minus ollama), then Local, rendered in the production
/// `    {num:>2}) {:<22} {:<46} {}` format.
///
/// The numbering mirrors run_init exactly: International uses `i + 1`,
/// while the mainland and local sections use `all_entries.len() + i + 1`
/// **while pushing into that same vector** — the running length grows
/// mid-loop, so those numbers step by two (7, 9, 11, … with the bundled
/// catalog), not continuously.
fn expected_wizard_lines() -> Vec<String> {
    fn wizard_line(num: usize, p: &recursive::providers::ProviderPreset) -> String {
        let hint = if p.key_env.is_empty() {
            "(no key needed)".to_string()
        } else {
            format!("[{}]", p.key_env)
        };
        format!("    {num:>2}) {:<22} {:<46} {}", p.name, brief(p), hint)
    }

    let catalog = recursive::all_presets();
    let international: Vec<_> = catalog.iter().filter(|p| !p.mainland_accessible).collect();
    let mainland: Vec<_> = catalog
        .iter()
        .filter(|p| p.mainland_accessible && p.id != "ollama")
        .collect();
    let local: Vec<_> = catalog.iter().filter(|p| p.id == "ollama").collect();

    let mut lines = Vec::new();
    for (i, p) in international.iter().enumerate() {
        lines.push(wizard_line(i + 1, p));
    }
    let mut all_entries = international.len();
    for (i, p) in mainland.iter().enumerate() {
        lines.push(wizard_line(all_entries + i + 1, p));
        all_entries += 1;
    }
    for (i, p) in local.iter().enumerate() {
        lines.push(wizard_line(all_entries + i + 1, p));
        all_entries += 1;
    }
    lines
}

#[test]
fn wizard_lists_catalog_sections_with_running_numbers() {
    let rig = Rig::new();
    let out = rig.feed(&["init"], "");
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "wizard exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    let international = stdout.find("International:").expect("international header");
    let mainland = stdout
        .find("Mainland China (直连):")
        .expect("mainland header");
    let local = stdout.find("Local:").expect("local header");
    assert!(
        international < mainland && mainland < local,
        "section order drifted:\n{stdout}"
    );
    for line in expected_wizard_lines() {
        assert!(
            stdout.contains(&line),
            "missing wizard line {line:?}\nfull stdout:\n{stdout}"
        );
    }
}

#[test]
fn wizard_typed_key_is_routed_to_the_secrets_file() {
    let rig = Rig::new();
    let out = rig.feed(&["init"], "\n\nsk-wizard-pinned-key\n");
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "wizard exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    assert!(
        stdout.contains("Get your key at: https://console.anthropic.com/settings/keys"),
        "the preset key_url hint must print before the key prompt:\n{stdout}"
    );
    assert!(
        stdout.contains("API key saved to"),
        "a typed non-empty key must be persisted:\n{stdout}"
    );
    let secrets_body =
        std::fs::read_to_string(rig.home.path().join(".recursive").join("secrets.env"))
            .expect("secrets.env written for a typed key");
    assert!(
        secrets_body.contains("sk-wizard-pinned-key"),
        "key value must reach secrets.env:\n{secrets_body}"
    );
    let config = std::fs::read_to_string(rig.home.path().join(".recursive").join("config.toml"))
        .expect("config.toml written");
    assert!(
        config.contains("preset = \"anthropic\""),
        "default choice persists provider.preset:\n{config}"
    );
}

#[test]
fn wizard_empty_env_var_falls_through_to_the_prompt() {
    let rig = Rig::new();
    let out = rig.feed_with_env(
        &["init"],
        "\n\nsk-typed-fallback\n",
        &[("ANTHROPIC_API_KEY", "")],
    );
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "wizard exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    assert!(
        !stdout.contains("detected"),
        "an empty env var must not count as detected:\n{stdout}"
    );
    assert!(
        stdout.contains("API key saved to"),
        "the key typed at the prompt must be persisted:\n{stdout}"
    );
    let secrets_body =
        std::fs::read_to_string(rig.home.path().join(".recursive").join("secrets.env"))
            .expect("secrets.env written for the typed key");
    assert!(
        secrets_body.contains("sk-typed-fallback"),
        "typed key value must reach secrets.env:\n{secrets_body}"
    );
}

#[test]
fn wizard_env_prefilled_key_skips_the_secrets_write() {
    let rig = Rig::new();
    let out = rig.feed_with_env(&["init"], "\n\n", &[("ANTHROPIC_API_KEY", "sk-env-canary")]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "wizard exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    assert!(
        stdout.contains("ANTHROPIC_API_KEY detected, using it"),
        "the env-prefilled key must be detected:\n{stdout}"
    );
    assert!(
        !stdout.contains("API key saved to"),
        "an env-prefilled key must NOT be rewritten to secrets.env \
         (the 'skipping write' contract):\n{stdout}"
    );
    assert!(
        !rig.home
            .path()
            .join(".recursive")
            .join("secrets.env")
            .exists(),
        "no secrets.env may appear for an env-prefilled key"
    );
}

#[test]
fn manual_declined_persist_keeps_config_inline() {
    let rig = Rig::new();
    let script = "0\nhttps://wizard-b3.example.com/v1\nb3vendor\n\n\nn\nb3model\n";
    let out = rig.feed(&["init"], script);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "wizard exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    assert!(
        !stdout.contains("Preset saved to"),
        "a declined persist must not write providers.d:\n{stdout}"
    );
    assert!(
        !rig.home
            .path()
            .join("providers.d")
            .join("b3vendor.toml")
            .exists(),
        "declined persist must leave no preset file"
    );
    assert!(
        stdout.contains("Warning: no API key set"),
        "empty key at the prompt degrades to the warning:\n{stdout}"
    );
    let config = std::fs::read_to_string(rig.home.path().join(".recursive").join("config.toml"))
        .expect("config.toml written");
    assert!(
        config.contains("type = \"openai\""),
        "manual flow keeps explicit provider.type:\n{config}"
    );
    assert!(
        config.contains("api_base = \"https://wizard-b3.example.com/v1\""),
        "manual flow keeps the typed api_base:\n{config}"
    );
    assert!(
        config.contains("model = \"b3model\""),
        "typed model persisted:\n{config}"
    );
    assert!(
        !config.contains("preset"),
        "declined persist must not resolve via provider.preset:\n{config}"
    );
}

#[test]
fn manual_accepted_preset_carries_the_typed_model() {
    let rig = Rig::new();
    let script = "0\nhttps://wizard-b3b.example.com/v1\nb3vendorb\n\n\ny\nb3modelb\n";
    let out = rig.feed(&["init"], script);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "wizard exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    assert!(
        stdout.contains("Preset saved to"),
        "an accepted persist writes the preset file:\n{stdout}"
    );
    let preset_body =
        std::fs::read_to_string(rig.home.path().join("providers.d").join("b3vendorb.toml"))
            .expect("persisted preset file");
    assert!(
        preset_body.contains("[[providers.models]]"),
        "the typed model must be pushed into models \
         (a skipped push leaves the models array empty):\n{preset_body}"
    );
    assert!(
        preset_body.contains("b3modelb"),
        "typed model name in the preset file:\n{preset_body}"
    );
    let config = std::fs::read_to_string(rig.home.path().join(".recursive").join("config.toml"))
        .expect("config.toml written");
    assert!(
        config.contains("preset = \"b3vendorb\""),
        "accepted persist resolves the config via provider.preset:\n{config}"
    );
}
