//! Where every `Command` is built, so a child never comes from execvp's `PATH`
//! search, which treats an empty or relative entry as the current directory.
//! `clippy.toml` disallows `Command::new` everywhere else.

use std::path::Path;
use std::process::Command;

use crate::paths;

/// A command for `name`, found by [`paths::find_program`] on the process's `PATH`.
pub fn program(name: &str) -> Result<Command, String> {
    paths::find_program(Path::new(name), std::env::var_os("PATH").as_deref())
        .map(resolved)
        .ok_or_else(|| format!("{name} not found on PATH"))
}

/// A command for `path`, absolute: a fixed location, or what
/// [`paths::find_program`] found earlier or on another `PATH`.
pub fn resolved(path: impl AsRef<Path>) -> Command {
    let path = path.as_ref();
    // Not a debug_assert: a relative path in a release build would reach the search.
    assert!(path.is_absolute(), "{} is relative", path.display());
    // The one place allowed to: the path is absolute, so execvp does not search.
    #[allow(clippy::disallowed_methods)]
    Command::new(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_named() {
        assert_eq!(
            program("anvil-no-such-program").unwrap_err(),
            "anvil-no-such-program not found on PATH"
        );
    }

    #[test]
    fn a_found_program_is_absolute() {
        let command = program("/bin/sh").unwrap();
        assert_eq!(command.get_program(), "/bin/sh");
    }

    #[test]
    #[should_panic(expected = "ssh is relative")]
    fn a_relative_path_is_refused() {
        resolved("ssh");
    }
}
