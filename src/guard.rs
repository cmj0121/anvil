//! A run or plugin step's guards, checked on the target in the same round
//! trip as the step: `creates`, `removes`, `onlyif` and `unless`, in that
//! order, ahead of the step's script on stdin. A guard that skips prints one
//! line, marked with the step's fresh nonce, and exits 0; [`skip_reason`]
//! reads it back only as the last stdout line, so neither a noisy login shell
//! nor the step's own output can pass for a skip.

use crate::bytes::{fill_random, hex};
use crate::config::Guards;
use crate::remote::quote;
use crate::transport::Output;

/// What every skip line starts with, before the nonce.
pub(crate) const MARK: &str = "anvil-guard-";

/// A fresh nonce for one guarded step: 32 hex digits from `/dev/urandom`.
pub(crate) fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes).map_err(|err| format!("cannot read /dev/urandom: {err}"))?;
    Ok(hex(&bytes))
}

/// `body` behind `guards`, as one brace group; `label` is `run` or
/// `plugin NAME`. Guard commands read `/dev/null`, since the script itself
/// is on stdin, and their stdout is dropped; their stderr is the step's. A
/// guard command killed by a signal ends the step with its status.
pub(crate) fn wrap(guards: &Guards, label: &str, nonce: &str, body: &str) -> String {
    let skip = |reason: &str| quote(&format!("{MARK}{nonce} skipped {label}: {reason}"));
    let mut script = String::from("{ :\n");
    if guards.onlyif.is_some() || guards.unless.is_some() {
        script.push_str(
            "anvil_guard() { if [ \"$1\" -gt 128 ]; then \
             echo \"$2 was killed by signal $(($1 - 128))\" >&2; exit \"$1\"; fi; }\n",
        );
    }
    if let Some(path) = &guards.creates {
        let (p, line) = (quote(path), skip(&format!("{path} exists")));
        script.push_str(&format!(
            "if [ -e {p} ] || [ -L {p} ]; then printf '%s\\n' {line}; exit 0; fi\n"
        ));
    }
    if let Some(path) = &guards.removes {
        let (p, line) = (quote(path), skip(&format!("{path} is absent")));
        script.push_str(&format!(
            "if [ ! -e {p} ] && [ ! -L {p} ]; then printf '%s\\n' {line}; exit 0; fi\n"
        ));
    }
    if let Some(command) = &guards.onlyif {
        script.push_str(&format!(
            "(\n{command}\n) </dev/null >/dev/null; anvil_s=$?; anvil_guard \"$anvil_s\" onlyif\n\
             if [ \"$anvil_s\" -ne 0 ]; then printf '%s%s\\n' {} \"$anvil_s\"; exit 0; fi\n",
            skip("onlyif exited ")
        ));
    }
    if let Some(command) = &guards.unless {
        script.push_str(&format!(
            "(\n{command}\n) </dev/null >/dev/null; anvil_s=$?; anvil_guard \"$anvil_s\" unless\n\
             if [ \"$anvil_s\" -eq 0 ]; then printf '%s\\n' {}; exit 0; fi\n",
            skip("unless exited 0")
        ));
    }
    if guards.onlyif.is_some() || guards.unless.is_some() {
        script.push_str("unset -f anvil_guard; unset anvil_s\n");
    }
    script.push_str(body);
    if !body.ends_with('\n') {
        script.push('\n');
    }
    script.push_str("}\n");
    script
}

/// The skip reason when `output` is a guard skip: exit 0 and the last stdout
/// line (one trailing `\n` allowed) is `anvil-guard-{nonce} skipped {label}:
/// {reason}` for a reason one of `guards` can give. That line is removed from
/// `output.stdout`, the rest kept. Any other marker line is ordinary output.
pub(crate) fn skip_reason(
    guards: &Guards,
    label: &str,
    nonce: &str,
    output: &mut Output,
) -> Option<String> {
    if output.code != Some(0) {
        return None;
    }
    let stdout = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let start = stdout
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let line = std::str::from_utf8(&stdout[start..]).ok()?;
    let reason = line
        .strip_prefix(MARK)?
        .strip_prefix(nonce)?
        .strip_prefix(" skipped ")?
        .strip_prefix(label)?
        .strip_prefix(": ")?;
    if !expected(guards, reason) {
        return None;
    }
    let reason = reason.to_owned();
    output.stdout.truncate(start);
    Some(reason)
}

/// Whether `reason` is one a guard in `guards` prints.
fn expected(guards: &Guards, reason: &str) -> bool {
    let onlyif = || {
        reason
            .strip_prefix("onlyif exited ")
            .and_then(|n| n.parse::<u8>().ok().filter(|code| n == code.to_string()))
            .is_some_and(|code| (1..=128).contains(&code))
    };
    guards
        .creates
        .as_ref()
        .is_some_and(|path| reason == format!("{path} exists"))
        || guards
            .removes
            .as_ref()
            .is_some_and(|path| reason == format!("{path} is absent"))
        || guards.onlyif.is_some() && onlyif()
        || guards.unless.is_some() && reason == "unless exited 0"
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::transport::{Local, Transport, TransportError};

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn guards(
        creates: Option<&str>,
        removes: Option<&str>,
        onlyif: Option<&str>,
        unless: Option<&str>,
    ) -> Guards {
        Guards {
            creates: creates.map(str::to_owned),
            removes: removes.map(str::to_owned),
            onlyif: onlyif.map(str::to_owned),
            unless: unless.map(str::to_owned),
        }
    }

    fn run(guards: &Guards, body: &str) -> Result<(Output, Option<String>), TransportError> {
        let script = wrap(guards, "run", NONCE, body);
        let mut output = Local.run(&script, Duration::from_secs(10))?;
        let reason = skip_reason(guards, "run", NONCE, &mut output);
        Ok((output, reason))
    }

    fn reason(guards: &Guards) -> Option<String> {
        let (output, reason) = run(guards, "echo ran").unwrap();
        assert_eq!(output.code, Some(0), "{output:?}");
        match &reason {
            Some(_) => assert_eq!(output.stdout, b"", "the skip line is stripped"),
            None => assert_eq!(output.stdout, b"ran\n"),
        }
        reason
    }

    /// A temp dir removed when dropped.
    struct Temp(std::path::PathBuf);

    impl Temp {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp() -> Temp {
        Temp(crate::test_util::temp_dir("guard"))
    }

    #[test]
    fn creates_skips_only_when_the_path_exists_or_is_a_link() {
        let dir = temp();
        let path = dir.path().join("x");
        let path = path.to_str().unwrap();
        assert_eq!(reason(&guards(Some(path), None, None, None)), None);
        std::os::unix::fs::symlink("/nonexistent", path).unwrap();
        assert_eq!(
            reason(&guards(Some(path), None, None, None)),
            Some(format!("{path} exists")),
            "a dangling link exists"
        );
    }

    #[test]
    fn removes_runs_only_when_the_path_exists() {
        let dir = temp();
        let path = dir.path().join("x");
        let path = path.to_str().unwrap();
        assert_eq!(
            reason(&guards(None, Some(path), None, None)),
            Some(format!("{path} is absent"))
        );
        std::fs::write(path, "").unwrap();
        assert_eq!(reason(&guards(None, Some(path), None, None)), None);
    }

    #[test]
    fn onlyif_runs_only_when_it_exits_0_and_names_its_status() {
        assert_eq!(reason(&guards(None, None, Some("true"), None)), None);
        assert_eq!(
            reason(&guards(None, None, Some("exit 3"), None)),
            Some("onlyif exited 3".to_owned())
        );
        assert_eq!(
            reason(&guards(None, None, Some("exit 128"), None)),
            Some("onlyif exited 128".to_owned())
        );
    }

    #[test]
    fn unless_skips_when_it_exits_0() {
        assert_eq!(
            reason(&guards(None, None, None, Some("true"))),
            Some("unless exited 0".to_owned())
        );
        assert_eq!(reason(&guards(None, None, None, Some("false"))), None);
    }

    #[test]
    fn guard_commands_read_no_stdin_drop_stdout_and_keep_stderr() {
        let g = guards(None, None, Some("cat; echo noise; echo warn >&2"), None);
        let (output, reason) = run(&g, "echo ran").unwrap();
        assert_eq!(reason, None);
        assert_eq!(output.stdout, b"ran\n", "the script was not read as stdin");
        assert_eq!(output.stderr, b"warn\n");
    }

    #[test]
    fn several_guards_combine_with_and_in_order() {
        let dir = temp();
        let (made, absent) = (dir.path().join("made"), dir.path().join("absent"));
        std::fs::write(&made, "").unwrap();
        let (made, absent) = (made.to_str().unwrap(), absent.to_str().unwrap());
        let trace = dir.path().join("trace");
        let mark = |name: &str| format!("echo {name} >> {}; ", trace.display());
        let onlyif = format!("{}false", mark("onlyif"));
        let unless = format!("{}true", mark("unless"));

        let all = guards(Some(made), Some(absent), Some(&onlyif), Some(&unless));
        assert_eq!(
            reason(&all),
            Some(format!("{made} exists")),
            "creates first"
        );
        assert!(!trace.exists(), "no command ran after creates skipped");

        let rest = guards(Some(absent), Some(absent), Some(&onlyif), Some(&unless));
        assert_eq!(reason(&rest), Some(format!("{absent} is absent")));
        assert!(!trace.exists(), "removes before onlyif");

        let commands = guards(Some(absent), Some(made), Some(&onlyif), Some(&unless));
        assert_eq!(reason(&commands), Some("onlyif exited 1".to_owned()));
        assert_eq!(std::fs::read_to_string(&trace).unwrap(), "onlyif\n");

        let last = format!("{}true", mark("onlyif"));
        let unless_only = guards(Some(absent), Some(made), Some(&last), Some(&unless));
        assert_eq!(reason(&unless_only), Some("unless exited 0".to_owned()));
        assert_eq!(
            std::fs::read_to_string(&trace).unwrap(),
            "onlyif\nonlyif\nunless\n"
        );

        let none = guards(Some(absent), Some(made), Some("true"), Some("false"));
        assert_eq!(reason(&none), None, "every guard allows it");
    }

    #[test]
    fn a_killed_guard_fails_the_step_with_its_status() {
        for (onlyif, unless, name) in [
            (Some("sh -c 'kill -9 $$'"), None, "onlyif"),
            (None, Some("sh -c 'kill -TERM $$'"), "unless"),
        ] {
            let (output, reason) = run(&guards(None, None, onlyif, unless), "echo ran").unwrap();
            assert_eq!(reason, None);
            let signal = if name == "onlyif" { 9 } else { 15 };
            assert_eq!(output.code, Some(128 + signal), "{output:?}");
            assert_eq!(output.stdout, b"", "the step did not run");
            let stderr = String::from_utf8(output.stderr).unwrap();
            let line = format!("{name} was killed by signal {signal}\n");
            assert!(
                stderr.ends_with(&line),
                "the shell may say so first: {stderr}"
            );
        }
    }

    #[test]
    fn a_hanging_guard_times_the_step_out() {
        let script = wrap(
            &guards(None, None, Some("sleep 5"), None),
            "run",
            NONCE,
            "true",
        );
        let err = Local.run(&script, Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, TransportError::Timeout { .. }), "{err:?}");
    }

    #[test]
    fn the_step_sees_no_helper_of_the_guards_in_every_shell() {
        let g = guards(None, None, Some("true"), Some("false"));
        let body = "command -v anvil_guard || echo no-function; echo \"s=${anvil_s-unset}\"";
        let script = wrap(&g, "run", NONCE, body);
        let shells = [
            "/bin/sh",
            "/bin/dash",
            "/bin/bash",
            "/usr/bin/dash",
            "/usr/bin/bash",
        ];
        for shell in shells
            .into_iter()
            .filter(|s| std::path::Path::new(s).exists())
        {
            let mut command = std::process::Command::new(shell);
            command.arg("-s");
            let output =
                crate::transport::run_with_script(&mut command, &script, Duration::from_secs(10))
                    .unwrap();
            assert_eq!(output.code, Some(0), "{shell}: {output:?}");
            assert_eq!(output.stdout, b"no-function\ns=unset\n", "{shell}");
        }
    }

    #[test]
    fn the_step_keeps_its_exit_status_and_output() {
        let g = guards(None, None, Some("true"), None);
        let (output, reason) = run(&g, "echo out; exit 4").unwrap();
        assert_eq!((output.code, reason), (Some(4), None));
        assert_eq!(output.stdout, b"out\n");
    }

    fn output(stdout: &str, code: Option<i32>) -> Output {
        Output {
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            code,
            truncated: false,
            abandoned: false,
        }
    }

    #[test]
    fn a_skip_is_only_the_last_line_with_this_nonce_and_label() {
        let g = guards(Some("/x"), None, Some("c"), Some("c"));
        let line = |nonce: &str, rest: &str| format!("{MARK}{nonce} skipped run: {rest}");
        let mut noisy = output(&format!("motd\n{}\n", line(NONCE, "/x exists")), Some(0));
        assert_eq!(
            skip_reason(&g, "run", NONCE, &mut noisy).as_deref(),
            Some("/x exists")
        );
        assert_eq!(noisy.stdout, b"motd\n", "what came before is kept");
        let mut bare = output(&line(NONCE, "unless exited 0"), Some(0));
        assert_eq!(
            skip_reason(&g, "run", NONCE, &mut bare).as_deref(),
            Some("unless exited 0")
        );
        assert_eq!(bare.stdout, b"");

        let other = "f".repeat(32);
        for (stdout, code) in [
            (format!("{}\nafter\n", line(NONCE, "/x exists")), Some(0)),
            (format!("{}\n", line(&other, "/x exists")), Some(0)),
            (format!("{}\n", line(NONCE, "/x exists")), Some(1)),
            (format!("{}\n\n", line(NONCE, "/x exists")), Some(0)),
            (format!("{}\n", line(NONCE, "/y exists")), Some(0)),
            (format!("{}\n", line(NONCE, "onlyif exited 0")), Some(0)),
            (format!("{}\n", line(NONCE, "onlyif exited 129")), Some(0)),
            (format!("{}\n", line(NONCE, "onlyif exited 03")), Some(0)),
            (format!("{}\n", line(NONCE, "/x is absent")), Some(0)),
            (
                format!("{MARK}{NONCE} skipped plugin p: /x exists\n"),
                Some(0),
            ),
        ] {
            let mut out = output(&stdout, code);
            assert_eq!(skip_reason(&g, "run", NONCE, &mut out), None, "{stdout:?}");
            assert_eq!(out.stdout, stdout.as_bytes(), "left as it was");
        }
    }

    #[test]
    fn a_plugin_skip_names_the_plugin() {
        let g = guards(None, None, None, Some("true"));
        let script = wrap(&g, "plugin p", NONCE, &crate::run::wrap("echo ran"));
        let mut output = Local.run(&script, Duration::from_secs(10)).unwrap();
        assert_eq!(
            skip_reason(&g, "plugin p", NONCE, &mut output).as_deref(),
            Some("unless exited 0")
        );
        assert_eq!(skip_reason(&g, "run", NONCE, &mut output), None);
    }

    #[test]
    fn a_path_with_a_quote_is_quoted() {
        let dir = temp();
        let path = dir.path().join("it's");
        std::fs::write(&path, "").unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(
            reason(&guards(Some(path), None, None, None)),
            Some(format!("{path} exists"))
        );
    }

    #[test]
    fn nonces_are_32_fresh_hex_digits() {
        let (a, b) = (nonce().unwrap(), nonce().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
