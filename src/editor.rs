//! The editor flow (D9): resolve the editor command and hand a buffer to it
//! through a private temp file.
//!
//! The file lives in `<temp_dir>/anvil-<uid>/`, a 0700 directory that must be
//! ours; the file itself is created 0600 with `create_new`. After the editor
//! returns the file is overwritten with zeros and removed. The zeroing is best
//! effort only: an editor that writes a new file and renames it over the old
//! one leaves the original inode un-zeroed, and the filesystem (APFS) may keep
//! old blocks anyway. The real controls are the per-user 0700 directory, the
//! 0600 mode and `create_new` (D9).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::document::MAX_BYTES;

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
    /// `$EDITOR`, else `nvim`, checked against the process `PATH`.
    pub fn resolve(preference: Option<&str>) -> Result<Editor, EditorError> {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        let command = command_for(preference, visual.as_deref(), editor.as_deref());
        Editor::from_command(&command, std::env::var_os("PATH").as_deref())
    }

    /// Splits `command` on whitespace; the program must be absolute or found
    /// on `path`.
    pub fn from_command(command: &str, path: Option<&OsStr>) -> Result<Editor, EditorError> {
        let mut words = command.split_whitespace();
        let Some(program) = words.next() else {
            return Err(EditorError::Empty);
        };
        let program = find_program(Path::new(program), path)
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

        // Always shown, not a log line: the user must know before typing a secret.
        if buffer == Buffer::Sensitive && !self.is_vim() {
            eprintln!("{}", copies_note(&self.program));
        }
        let args = self.args_for(path, buffer);
        log::debug!(
            "spawn {} {}",
            self.program.display(),
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let status = Command::new(&self.program)
            .args(&args)
            .status()
            .map_err(|source| EditorError::Spawn {
                program: self.program.display().to_string(),
                source,
            })?;
        if !status.success() {
            return Err(EditorError::Aborted(status.code()));
        }

        // Bounded read: an oversize buffer comes back truncated to one byte
        // past the limit, which the caller rejects as too large.
        let mut edited = Vec::new();
        File::open(path)
            .map_err(io)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut edited)
            .map_err(io)?;
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

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

/// The executable `program` names: itself when absolute, else the first
/// match on `path`. Relative paths with a directory part are refused, and so
/// are relative `path` entries (`.`, `bin`), which would resolve against the
/// current directory.
pub fn find_program(program: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_absolute() {
        return is_executable(program).then(|| program.to_path_buf());
    }
    if program.components().count() != 1 {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
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
    let meta = fs::symlink_metadata(&dir).map_err(io)?;
    let unsafe_dir = |reason| EditorError::UnsafeDir {
        path: dir.clone(),
        reason,
    };
    if !meta.is_dir() {
        return Err(unsafe_dir("not a directory"));
    }
    if meta.uid() != uid {
        return Err(unsafe_dir("owned by another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(unsafe_dir("accessible to group or others"));
    }
    Ok(dir)
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
fn random() -> u64 {
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
        let editor = Editor::from_command("false", system_path().as_deref()).unwrap();
        let err = editor.edit(b"x", "aborted", Buffer::Document).unwrap_err();
        assert!(matches!(err, EditorError::Aborted(Some(1))), "{err}");
        assert!(err.to_string().contains("nothing written"));
        assert!(edits_in_dir("aborted").is_empty());
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

    #[test]
    fn relative_path_entries_are_skipped() {
        let dir = std::env::temp_dir().join(format!("anvil-path-{}", std::process::id()));
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("anvil-path-probe");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let program = Path::new("anvil-path-probe");
        // A relative entry naming the same directory from the cwd is skipped.
        let relative = pathdiff(&bin, &std::env::current_dir().unwrap());
        assert!(fs::metadata(relative.join(program)).is_ok());
        assert_eq!(find_program(program, Some(relative.as_os_str())), None);
        assert_eq!(
            find_program(program, Some(bin.as_os_str())),
            Some(tool.clone())
        );
        let mixed = format!("{}:{}", relative.display(), bin.display());
        assert_eq!(find_program(program, Some(OsStr::new(&mixed))), Some(tool));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `to` relative to `from` (both absolute), via enough `..` to reach `/`.
    fn pathdiff(to: &Path, from: &Path) -> PathBuf {
        let mut rel = PathBuf::new();
        for _ in from.components().skip(1) {
            rel.push("..");
        }
        rel.join(to.strip_prefix("/").unwrap())
    }
}
