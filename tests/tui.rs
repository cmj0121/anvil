//! `anvil tui` on the built binary, driven on a pseudo-terminal by
//! `pty_tui.py`: an editor handoff from the TUI and an apply run, each
//! ending with `q` and the terminal restored. Skipped without python3
//! 3.11 or newer, which has `os.login_tty`.

#![cfg(feature = "tui")]

mod common;

use std::fs;
use std::process::Command;

use common::{Sandbox, fixture, text};

/// python3 with `os.login_tty` (3.11 or newer), which the terminal script
/// needs.
fn python() -> bool {
    let ok = Command::new("python3")
        .args(["-c", "import os; os.login_tty"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !ok {
        eprintln!("skip: no python3 with os.login_tty");
    }
    ok
}

/// What the editor prints on the terminal before it edits.
const MARKER: &str = "marked-editor: editing";

/// A sandbox with an empty inventory (`local` only) and a config whose
/// `[anvil].editor` prints [`MARKER`] and runs the fake editor, plus
/// `extra` config lines.
fn tui_sandbox(extra: &str) -> Sandbox {
    let sandbox = Sandbox::new("anvt");
    sandbox.write_config("inventory.toml", "");
    let editor = sandbox.root().join("marked-editor.sh");
    let script = format!(
        "printf '{MARKER} %s\\n' \"$1\"\nexec {} \"$1\"\n",
        fixture("fake-editor.sh").display()
    );
    fs::write(&editor, script).unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[anvil]\neditor = \"/bin/sh {}\"\n{extra}",
            editor.display()
        ),
    );
    sandbox
}

/// Runs `anvil --secret-store memory tui` under `pty_tui.py` with `steps`
/// (`--wait REGEX` / `--send KEYS` pairs); asserts the script's checks
/// passed and returns its transcript (the last screen and the checks).
fn drive(sandbox: &Sandbox, vars: &[(&str, &str)], steps: &[&str]) -> String {
    let script = fixture("pty_tui.py");
    let mut args = vec![script.to_str().unwrap()];
    args.extend(steps);
    args.extend([
        "--",
        env!("CARGO_BIN_EXE_anvil"),
        "--secret-store",
        "memory",
        "tui",
    ]);
    let out = sandbox
        .command("python3")
        .args(&args)
        .env_remove("EDITOR")
        .envs(vars.iter().copied())
        .output()
        .unwrap();
    let transcript = text(&out.stdout);
    assert!(out.status.success(), "{transcript}\n{}", text(&out.stderr));
    transcript
}

#[test]
fn an_inventory_edit_hands_the_terminal_to_the_editor_and_back() {
    if !python() {
        return;
    }
    let sandbox = tui_sandbox("");
    let content = sandbox.root().join("inventory.new");
    fs::write(&content, "[hosts.pi]\nhost = \"pi.lan\"\n").unwrap();
    let inventory = sandbox.config_dir().join("inventory.toml");
    let wrote = format!("wrote {}", inventory.display());

    let transcript = drive(
        &sandbox,
        &[("FAKE_EDITOR_CONTENT", content.to_str().unwrap())],
        &[
            "--entries",
            "2",
            "--main",
            &format!("^{MARKER} .*\\.toml$"),
            "--wait",
            r"\[ \] local local",
            "--send",
            "i",
            "--wait",
            &regex_escape(&wrote),
            "--wait",
            r"\[ \] pi    pi\.lan",
            "--send",
            "q",
        ],
    );
    assert!(transcript.contains(&wrote), "{transcript}");
    let saved = fs::read_to_string(&inventory).unwrap();
    assert!(saved.contains("[hosts.pi]"), "{saved}");
}

#[test]
fn apply_shows_each_step_and_the_ok_block() {
    if !python() {
        return;
    }
    let sandbox = tui_sandbox("\n[[steps]]\nrun = \"printf 'out-%s\\\\n' tui\"\n");

    let transcript = drive(
        &sandbox,
        &[],
        &[
            "--wait",
            r"\[ \] local",
            "--send",
            "a",
            "--wait",
            r"✔ ok · 1 step\b",
            "--send",
            "q",
        ],
    );
    for line in [
        "▶ local",
        "$ printf 'out-%s\\n' tui",
        "    out-tui",
        "1 ok, 0 failed",
    ] {
        assert!(transcript.contains(line), "{line:?} in:\n{transcript}");
    }
}

/// `text` as a Python regex matching it literally.
fn regex_escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escape = !c.is_alphanumeric() && !matches!(c, ' ' | '_');
            escape.then_some('\\').into_iter().chain([c])
        })
        .collect()
}
