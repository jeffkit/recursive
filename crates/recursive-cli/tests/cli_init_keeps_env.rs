//! Issue #223 (CI run 39922642466): `async_main`'s startup credential scrub
//! must keep its `Cmd::Init` exemption (`if !matches!(effective_cmd,
//! Cmd::Init { .. })`). The `delete !` mutant there makes `init` the *only*
//! command that scrubs, so init's own env re-read sees the freshly scrubbed
//! process environment instead of the operator's.
//!
//! The observable: `recursive init` re-reads the preset's `key_env` variable
//! by name (`cli/init.rs` step 3) and reports `✓ <VAR> detected, using it`.
//! A `providers.d` preset whose `key_env` is `RECURSIVE_*`-shaped — i.e. one
//! the scrub removes — turns that line into a probe of the process
//! environment: under the mutant the variable is gone before init re-reads
//! it, init falls through to the interactive prompt (stdin is /dev/null
//! here), and the "detected" line never prints. Bundled presets can't serve
//! as the probe: their `key_env` names are third-party (`DEEPSEEK_API_KEY`,
//! …), which the scrub deliberately keeps.

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
    /// rooted in the pinned home. The providers-catalog URL points at a
    /// refused loopback port so `init`'s fail-soft refresh lands on the
    /// bundled+providers.d catalog immediately instead of the network.
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

    /// A custom vendor preset whose `key_env` is exactly the shape the
    /// startup scrub removes (`…_KEY` in the `RECURSIVE_` namespace).
    fn write_probe_preset(&self) {
        let dir = self.home.path().join("providers.d");
        std::fs::create_dir_all(&dir).expect("providers.d dir");
        std::fs::write(
            dir.join("b2probe.toml"),
            r#"[[providers]]
id = "b2probe"
name = "B2 Probe Vendor"
provider_type = "openai"
api_base = "http://127.0.0.1:9/v1"
default_model = "probe-model"
models = [{name = "probe-model", context_window = 1024}]
mainland_accessible = false
key_env = "RECURSIVE_INIT_PROBE_KEY"
"#,
        )
        .expect("write probe preset");
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("spawn recursive")
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The contract under test: with `RECURSIVE_INIT_PROBE_KEY` in the child's
/// environment, `recursive init --provider b2probe` must still find it —
/// init keeps its environment because it builds no execution surface.
#[test]
fn init_keeps_its_environment_so_the_preset_key_env_survives() {
    let rig = Rig::new();
    rig.write_probe_preset();
    let out = rig
        .cmd()
        .env("RECURSIVE_INIT_PROBE_KEY", "b2-canary-value")
        .args(["init", "--provider", "b2probe", "--model", "probe-model"])
        .output()
        .expect("spawn recursive init");
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "init exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        stderr_of(&out)
    );
    assert!(
        stdout.contains("RECURSIVE_INIT_PROBE_KEY detected"),
        "init must re-read the preset key env after the startup scrub \
         exempted Init; stdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&out)
    );
}

/// Control row for the probe itself: with the variable *absent*, init falls
/// through to the prompt and must NOT claim it detected anything — this is
/// exactly the output the `delete !` mutant would produce, so the kill
/// assertion above is discriminating and not vacuously true.
#[test]
fn init_without_the_key_env_falls_through_to_the_prompt() {
    let rig = Rig::new();
    rig.write_probe_preset();
    let out = rig.run(&["init", "--provider", "b2probe", "--model", "probe-model"]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "init failed:\n{stdout}\n{}",
        stderr_of(&out)
    );
    assert!(
        !stdout.contains("RECURSIVE_INIT_PROBE_KEY detected"),
        "the probe must be absent when the variable is not set:\n{stdout}"
    );
    assert!(
        stdout.contains("no API key set"),
        "stdin is /dev/null, so the prompt path degrades to the warning:\n{stdout}"
    );
}
