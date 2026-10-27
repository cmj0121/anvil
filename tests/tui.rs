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
            r"targets: all \(1\)",
            "--send",
            "i",
            "--wait",
            &regex_escape(&wrote),
            "--wait",
            r"targets: all \(2\)",
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
            r"1 printf 'out-%s\\n' tui",
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
        "✔ 1  ✘ 0",
    ] {
        assert!(transcript.contains(line), "{line:?} in:\n{transcript}");
    }
}

#[test]
fn b_rolls_back_after_y_and_words_a_conflict_for_the_tui() {
    if !python() {
        return;
    }
    let sandbox = tui_sandbox("");
    let dest = sandbox.root().join("out.conf").display().to_string();
    fs::write(sandbox.config_dir().join("motd.txt"), "from apply\n").unwrap();
    let config = fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{config}\n[[files]]\nsrc = \"motd.txt\"\ndest = \"{dest}\"\nmode = \"0644\"\n\
             sudo = false\n"
        ),
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    fs::write(&dest, "edited by hand\n").unwrap();

    let refusal = format!(
        "files: {dest} changed since the last apply; run anvil rollback --force {dest} in a \
         terminal to restore it anyway"
    );
    let transcript = drive(
        &sandbox,
        &[],
        &[
            "--cols",
            "250",
            "--wait",
            r"targets: all \(1\)",
            "--send",
            "b",
            "--wait",
            r"roll back 1 target: local\?",
            // Enter does not answer it: the question stays and nothing runs.
            "--send",
            "\\r",
            "--absent",
            r"rollback · run",
            "--wait",
            r"y roll back · n/Esc cancel",
            "--send",
            "y",
            "--wait",
            &regex_escape(&refusal),
            "--send",
            "q",
        ],
    );
    assert!(transcript.contains("rollback · run 1/1"), "{transcript}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "edited by hand\n");
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
