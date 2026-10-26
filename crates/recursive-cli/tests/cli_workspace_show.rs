//! Issue #245 batch 3 (CI run 40256596759): `workspace show` must print the
//! record block through `print_workspace` (`crates/recursive-cli/src/cli/
//! workspace.rs`). The renderer writes to process stdout, so the pin drives
//! the `recursive` binary as a subprocess against a registry rooted in a
//! pinned RECURSIVE_HOME — `add` then `show`, and the record block (id,
//! name, active state, not-indexed hint) must all be present.

use std::process::{Command, Output, Stdio};

fn recursive_cmd(home: &std::path::Path, work: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("RECURSIVE_HOME", home);
    cmd.env(
        "RECURSIVE_PROVIDERS_URL",
        "http://127.0.0.1:9/providers.json",
    );
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.current_dir(work);
    cmd.arg("--log").arg("error");
    cmd.arg("--workspace").arg(work);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn workspace_show_prints_the_record_block() {
    let home = tempfile::tempdir().expect("home tempdir");
    let work = tempfile::tempdir().expect("workspace tempdir");
    let project = work.path().join("proj");
    std::fs::create_dir_all(&project).expect("project dir");

    let out = recursive_cmd(home.path(), work.path())
        .args(["workspace", "add", project.to_str().expect("utf8 path")])
        .output()
        .expect("spawn recursive workspace add");
    let add_stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "workspace add failed:\n{add_stdout}\n{}",
        stderr_of(&out)
    );
    let id = add_stdout
        .split("Registered ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .expect("registered workspace id in add output")
        .to_string();

    let out = recursive_cmd(home.path(), work.path())
        .args(["workspace", "show", &id])
        .output()
        .expect("spawn recursive workspace show");
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "workspace show failed:\n{stdout}\n{}",
        stderr_of(&out)
    );
    assert!(stdout.contains(&id), "record id printed:\n{stdout}");
    assert!(
        stdout.contains("name:"),
        "record name line printed:\n{stdout}"
    );
    assert!(
        stdout.contains("state:      active"),
        "record state line printed:\n{stdout}"
    );
    assert!(
        stdout.contains("sessions:   (not indexed"),
        "un-indexed workspace reports the index hint:\n{stdout}"
    );
}
