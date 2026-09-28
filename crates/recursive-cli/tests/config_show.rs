//! Locked format for issue #43: `recursive config show` prints both the
//! effective `model:` and `preset resolves to: ... default_model=<preset.default_model>`,
//! so the preset line's model is explicitly labeled as the preset default and
//! cannot contradict the effective model when an explicit model overrides it.

use std::process::Command;

fn run_config_show(config_toml: &str) -> String {
    let home = tempfile::tempdir().expect("tempdir");
    // RECURSIVE_HOME points at the parent; config.toml lives in `<home>/.recursive/`.
    let dir = home.path().join(".recursive");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("config.toml"), config_toml).expect("write config");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
    cmd.arg("config").arg("show");
    cmd.env("RECURSIVE_HOME", home.path());
    // Strip ambient env overrides so the config file decides the model.
    for var in [
        "RECURSIVE_MODEL",
        "RECURSIVE_API_BASE",
        "RECURSIVE_PROVIDER_TYPE",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
    ] {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("spawn recursive");
    assert!(out.status.success(), "config show failed: {out:?}");
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

#[test]
fn config_show_labels_preset_model_as_default_not_effective() {
    // Bundled `deepseek` preset has default_model = "deepseek-v4-flash";
    // the explicit model must win and the preset line must not re-print
    // its default under an ambiguous `model=` label.
    let stdout =
        run_config_show("[provider]\npreset = \"deepseek\"\nmodel = \"custom-explicit-model\"\n");

    assert!(
        stdout.contains("model:         custom-explicit-model"),
        "effective model line missing or wrong:\n{stdout}"
    );
    // Buggy line: `preset resolves to: type=openai, model=deepseek-v4-flash, ...`
    assert!(
        !stdout.contains("preset resolves to: type=openai, model="),
        "buggy ambiguous `model=` in preset line still present:\n{stdout}"
    );
    // Fixed line must explicitly mark it as the preset's default.
    assert!(
        stdout.contains("default_model=deepseek-v4-flash"),
        "preset line does not label its model as default_model:\n{stdout}"
    );
}

#[test]
fn config_show_preset_line_absent_when_no_preset_matches() {
    // A non-preset api_base and no preset key: the preset resolution line
    // must not appear at all (and therefore cannot contradict the model).
    let stdout = run_config_show(
        "[provider]\nmodel = \"some-model\"\napi_base = \"https://example.invalid/v1\"\n",
    );
    assert!(
        !stdout.contains("preset resolves to:"),
        "preset line printed without any preset:\n{stdout}"
    );
}
