//! The editor flow: resolve the editor command and hand a buffer to it
//! through a private temp file.
//!
//! The file lives in `<temp_dir>/anvil-<uid>/`, a 0700 directory that must be
//! ours; the file itself is created 0600 with `create_new`. After the editor
//! returns the file is overwritten with zeros and removed. The zeroing is best
//! effort only: an editor that writes a new file and renames it over the old
//! one leaves the original inode un-zeroed, and the filesystem (APFS) may keep
//! old blocks anyway. The real controls are the per-user 0700 directory, the
//! 0600 mode and `create_new`.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::document;
use crate::paths::{self, PrivateDirError};
use crate::term;

/// The editor when neither the config nor the environment names one.
pub const DEFAULT: &str = "nvim";

/// How often a temp file name may collide before giving up.
const NAME_ATTEMPTS: u32 = 16;

/// Editors that take vim's flags; in [`Buffer::Sensitive`] mode they get
/// [`VIM_PRIVATE`] so the buffer leaves no swap, undo, backup or history copy.
const VIM_FAMILY: [&str; 4] = ["vim", "nvim", "vi", "view"];

/// No swap file (`-n`), no viminfo/shada (`-i NONE`), and no undo or backup
/// file: set by `--cmd` before the vimrc and again by `-c` after it, so a vimrc
/// that turns them on does not win.
const VIM_PRIVATE: [&str; 7] = ["-n", "-i", "NONE", "--cmd", VIM_OPTIONS, "-c", VIM_OPTIONS];
const VIM_OPTIONS: &str = "set noswapfile noundofile nobackup nowritebackup";

/// What the buffer holds, which decides how much the editor may keep of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    /// A config document: the editor runs as configured.
    Document,
    /// A secret: vim-family editors are told to keep no copies; any other
    /// editor gets a warning that it might.
    Sensitive,
}

#[derive(Debug)]
pub enum EditorError {
    /// The command is blank.
    Empty,
    /// The program is neither absolute nor on `PATH`.
    NotFound(String),
    /// The temp directory exists but is not ours or is open to others.
    UnsafeDir {
        path: PathBuf,
        reason: &'static str,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Spawn {
        program: String,
        source: io::Error,
    },
    /// The editor exited non-zero (or on a signal: `None`); nothing is written.
    Aborted(Option<i32>),
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditorError::Empty => f.write_str("editor command is empty"),
            EditorError::NotFound(program) => {
                write!(f, "editor `{program}`: not absolute and not found on PATH")
            }
            EditorError::UnsafeDir { path, reason } => write!(
                f,
                "{}: refusing to use the temp directory: {reason}",
                path.display()
            ),
            EditorError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            EditorError::Spawn { program, source } => {
                write!(f, "cannot run editor `{program}`: {source}")
            }
            EditorError::Aborted(Some(code)) => {
                write!(f, "editor exited with status {code}, nothing written")
            }
            EditorError::Aborted(None) => f.write_str("editor was killed, nothing written"),
        }
    }
}

impl std::error::Error for EditorError {}

/// What [`Editor::resolve`] does when the configured editor cannot be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Fail.
    Strict,
    /// Warn and use `$VISUAL`, `$EDITOR` or the default instead: a typo in
    /// `[anvil].editor` must not lock the user out of `anvil config`.
    Lenient,
}

/// A resolved editor: the program as found on disk and its fixed arguments;
/// the file to edit is appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    program: PathBuf,
    args: Vec<String>,
}

impl Editor {
    /// The program as found on disk.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The editor from `preference` (`[anvil].editor`), else `$VISUAL`, else
    /// `$EDITOR`, else `nvim`, checked against the process `PATH`. When the
    /// preferred program cannot be found, `fallback` decides.
    pub fn resolve(preference: Option<&str>, fallback: Fallback) -> Result<Editor, EditorError> {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        let path = std::env::var_os("PATH");
        let resolve = |preference| {
            let command = command_for(preference, visual.as_deref(), editor.as_deref());
            Editor::from_command(&command, path.as_deref())
        };
        match resolve(preference) {
            Err(err @ (EditorError::NotFound(_) | EditorError::Empty))
                if fallback == Fallback::Lenient && preference.is_some() =>
            {
                // Logged under the commands' target, as `anvil config` always has.
                log::warn!(
                    target: "anvil::commands",
                    "{err}; falling back to $VISUAL, $EDITOR or `{DEFAULT}`"
                );
                resolve(None)
            }
            other => other,
        }
    }

    /// Splits `command` on whitespace; the program must be absolute or found
    /// on `path`.
    pub fn from_command(command: &str, path: Option<&OsStr>) -> Result<Editor, EditorError> {
        let mut words = command.split_whitespace();
        let Some(program) = words.next() else {
            return Err(EditorError::Empty);
        };
        let program = paths::find_program(Path::new(program), path)
            .ok_or_else(|| EditorError::NotFound(program.to_string()))?;
        Ok(Editor {
            program,
            args: words.map(str::to_string).collect(),
        })
    }

    /// Opens `initial` in the editor as an `.<ext>` file; `None` when the
    /// editor left it unchanged.
    pub fn edit(
        &self,
        initial: &[u8],
        ext: &str,
        buffer: Buffer,
    ) -> Result<Option<Vec<u8>>, EditorError> {
        let dir = private_dir()?;
        let (path, file) = create_temp(&dir, ext)?;
        let result = self.edit_at(&path, file, initial, buffer);
        scrub(&path);
        result
    }

    fn is_vim(&self) -> bool {
        self.program
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| VIM_FAMILY.contains(&name))
    }

    /// The editor's arguments for `path`: the configured ones, then for a
    /// sensitive buffer in a vim-family editor [`VIM_PRIVATE`], then the file.
    pub fn args_for(&self, path: &Path, buffer: Buffer) -> Vec<OsString> {
        let mut args: Vec<OsString> = self.args.iter().map(OsString::from).collect();
        if buffer == Buffer::Sensitive && self.is_vim() {
            args.extend(VIM_PRIVATE.iter().map(OsString::from));
        }
        args.push(path.as_os_str().to_owned());
        args
    }

    fn edit_at(
        &self,
        path: &Path,
        mut file: File,
        initial: &[u8],
        buffer: Buffer,
    ) -> Result<Option<Vec<u8>>, EditorError> {
        let io = |source| EditorError::Io {
            path: path.to_path_buf(),
            source,
        };
        file.write_all(initial).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);

        let args = self.args_for(path, buffer);
        let status = term::with_terminal(|| {
            // Always shown, not a log line: the user must know before typing a
            // secret. It runs with the real terminal (inside `with_terminal`),
            // so it reaches the user even under the TUI.
            if buffer == Buffer::Sensitive && !self.is_vim() {
                eprintln!("{}", copies_note(&self.program));
            }
            log::debug!(
                "spawn {} {}",
                self.program.display(),
                args.iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            Command::new(&self.program).args(&args).status()
        })
        .map_err(|source| EditorError::Spawn {
            program: self.program.display().to_string(),
            source,
        })?;
        if !status.success() {
            return Err(EditorError::Aborted(status.code()));
        }

        // Bounded read: an oversize buffer comes back truncated to one byte
        // past the limit, which the caller rejects as too large.
        let edited = document::read_capped(path).map_err(io)?;
        Ok((edited != initial).then_some(edited))
    }
}

/// The one-line note for a sensitive buffer in an editor that was not told to
/// keep no copies; it names the editor, never the buffer.
pub fn copies_note(program: &Path) -> String {
    format!(
        "note: editor `{}` may keep its own copies of the secret (swap, undo, backup or history files); \
         only vim, nvim, vi and view are told not to",
        program.display()
    )
}

/// `preference`, else `$VISUAL`, else `$EDITOR`, else the default; blank
/// values count as unset.
pub fn command_for(preference: Option<&str>, visual: Option<&str>, editor: Option<&str>) -> String {
    [preference, visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(DEFAULT)
        .to_string()
}

/// `<temp_dir>/anvil-<uid>/`, created 0700; refused unless it is a directory
/// we own that group and others cannot reach.
fn private_dir() -> Result<PathBuf, EditorError> {
    let uid = rustix::process::getuid().as_raw();
    let dir = std::env::temp_dir().join(format!("anvil-{uid}"));
    let io = |source| EditorError::Io {
        path: dir.clone(),
        source,
    };
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(io(err)),
    }
    let reason = match paths::check_private_dir(&dir, uid) {
        Ok(()) => return Ok(dir),
        Err(PrivateDirError::Io(source)) => return Err(io(source)),
        Err(PrivateDirError::NotDir) => "not a directory",
        Err(PrivateDirError::Owner(_)) => "owned by another user",
        Err(PrivateDirError::Mode(_)) => "accessible to group or others",
    };
    Err(EditorError::UnsafeDir { path: dir, reason })
}

/// A fresh `edit-<random>.<ext>` in `dir`, 0600, that did not exist before.
fn create_temp(dir: &Path, ext: &str) -> Result<(PathBuf, File), EditorError> {
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..NAME_ATTEMPTS {
        let path = dir.join(format!("edit-{:016x}.{ext}", random()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = err,
            Err(source) => return Err(EditorError::Io { path, source }),
        }
    }
    Err(EditorError::Io {
        path: dir.to_path_buf(),
        source: last,
    })
}

/// 64 random bits from `/dev/urandom`, else the clock mixed with the pid.
pub fn random() -> u64 {
    let mut bytes = [0u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .is_ok()
    {
        return u64::from_ne_bytes(bytes);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 40)
}

/// Overwrites the file with zeros and removes it; every step is best effort.
fn scrub(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.is_file()
        && let Ok(mut file) = OpenOptions::new().write(true).open(path)
    {
        let zeros = vec![0u8; meta.len() as usize];
        let _ = file.write_all(&zeros).and_then(|()| file.sync_all());
    }
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn system_path() -> Option<std::ffi::OsString> {
        std::env::var_os("PATH")
    }

    fn edits_in_dir(ext: &str) -> Vec<PathBuf> {
        let dir = private_dir().unwrap();
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == ext))
            .collect()
    }

    #[test]
    fn command_chain_skips_blank_values() {
        assert_eq!(command_for(Some("vi -u x"), Some("code"), None), "vi -u x");
        assert_eq!(command_for(None, Some(" code -w "), Some("vi")), "code -w");
        assert_eq!(command_for(Some(""), Some(" "), Some("vi")), "vi");
        assert_eq!(command_for(None, None, None), DEFAULT);
    }

    #[test]
    fn command_splits_on_whitespace_and_needs_a_program() {
        let editor = Editor::from_command("sh  -c\ttrue", system_path().as_deref()).unwrap();
        assert!(editor.program.is_absolute(), "{:?}", editor.program);
        assert!(editor.program.ends_with("sh"), "{:?}", editor.program);
        assert_eq!(editor.args, ["-c", "true"]);
        assert!(matches!(
            Editor::from_command("  ", system_path().as_deref()),
            Err(EditorError::Empty)
        ));
    }

    #[test]
    fn program_must_be_absolute_or_on_path() {
        let err = Editor::from_command("sh", None).unwrap_err();
        assert!(
            matches!(err, EditorError::NotFound(ref p) if p == "sh"),
            "{err}"
        );
        assert!(Editor::from_command("sh", Some(OsStr::new("/nonexistent"))).is_err());
        assert!(Editor::from_command("/bin/sh -e", None).is_ok());
        assert!(Editor::from_command("./sh", system_path().as_deref()).is_err());
        assert!(Editor::from_command("/nonexistent/sh", None).is_err());
    }

    #[test]
    fn unchanged_buffer_is_none_and_the_file_is_gone() {
        let _lock = term::test_lock();
        let editor = Editor::from_command("true", system_path().as_deref()).unwrap();
        let result = editor
            .edit(b"[hosts]\n", "unchanged", Buffer::Document)
            .unwrap();
        assert_eq!(result, None);
        assert!(edits_in_dir("unchanged").is_empty());
        let dir = private_dir().unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn failing_editor_aborts_with_its_status() {
        let _lock = term::test_lock();
        let editor = Editor::from_command("false", system_path().as_deref()).unwrap();
        let err = editor.edit(b"x", "aborted", Buffer::Document).unwrap_err();
        assert!(matches!(err, EditorError::Aborted(Some(1))), "{err}");
        assert!(err.to_string().contains("nothing written"));
        assert!(edits_in_dir("aborted").is_empty());
    }

    #[test]
    fn the_editor_runs_with_the_terminal_handed_back() {
        use crate::term::testing::{Event, Recorder};
        let _lock = term::test_lock();
        let (recorder, _guard) = Recorder::install();
        let editor = Editor::from_command("true", system_path().as_deref()).unwrap();
        editor.edit(b"x", "handoff", Buffer::Sensitive).unwrap();
        assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
    }

    fn editor_at(program: &str) -> Editor {
        Editor {
            program: PathBuf::from(program),
            args: vec!["-u".to_owned(), "x".to_owned()],
        }
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn a_sensitive_buffer_in_vim_or_nvim_keeps_no_copies() {
        let file = Path::new("/t/edit-1.txt");
        let options = "set noswapfile noundofile nobackup nowritebackup";
        for program in [
            "/usr/bin/vim",
            "/opt/homebrew/bin/nvim",
            "/usr/bin/vi",
            "/x/view",
        ] {
            assert_eq!(
                strings(editor_at(program).args_for(file, Buffer::Sensitive)),
                [
                    "-u",
                    "x",
                    "-n",
                    "-i",
                    "NONE",
                    "--cmd",
                    options,
                    "-c",
                    options,
                    "/t/edit-1.txt"
                ],
                "{program}"
            );
        }
    }

    #[test]
    fn documents_and_other_editors_get_no_extra_args() {
        let file = Path::new("/t/edit-1.txt");
        let plain = ["-u", "x", "/t/edit-1.txt"];
        assert_eq!(
            strings(editor_at("/usr/bin/nvim").args_for(file, Buffer::Document)),
            plain
        );
        for program in [
            "/usr/bin/nano",
            "/usr/local/bin/code",
            "/x/nvim-qt",
            "/x/gvim",
        ] {
            assert_eq!(
                strings(editor_at(program).args_for(file, Buffer::Sensitive)),
                plain,
                "{program}"
            );
        }
    }
}
