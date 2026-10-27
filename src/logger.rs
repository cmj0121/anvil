//! Stderr logger behind the `log` facade.
//!
//! Lines look like `<LEVEL> <target>: <message>`; no timestamps. Colour is used
//! only when stderr is a terminal. The level comes from the `-v` count and can
//! change at runtime ([`set_verbosity`]). A front end that owns the terminal
//! installs a [`Sink`] instead, and then nothing is written to stderr.
//! Messages can carry text from a host (ssh errors, a script's stderr), so
//! their control characters are escaped: a host cannot drive the terminal.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use log::{Level, LevelFilter, Log, Metadata, Record};

pub struct Logger {
    verbosity: AtomicU8,
    colour: bool,
}

/// Receives each record's line, already escaped with escape_controls, no colour.
pub trait Sink: Send + Sync {
    fn line(&self, level: log::Level, target: &str, message: &str);
}

static LOGGER: OnceLock<Logger> = OnceLock::new();
static SINK: RwLock<Option<Arc<dyn Sink>>> = RwLock::new(None);

/// Sends every log line to `sink` instead of stderr; `None`: back to stderr.
#[cfg_attr(
    not(any(test, feature = "tui")),
    expect(dead_code, reason = "called by the TUI")
)]
pub fn set_sink(sink: Option<Arc<dyn Sink>>) {
    *SINK.write().unwrap_or_else(PoisonError::into_inner) = sink;
}

/// Sets the level from a `-v` count, as [`level_for`] maps it.
#[cfg_attr(
    not(any(test, feature = "tui")),
    expect(dead_code, reason = "called by the TUI")
)]
pub fn set_verbosity(verbosity: u8) {
    LOGGER
        .get_or_init(|| Logger::new(verbosity))
        .verbosity
        .store(verbosity, Ordering::Relaxed);
    log::set_max_level(level_for(verbosity));
}

/// Maps the `-v` count to a level: 0 Error, 1 Warn, 2 Info, 3 and more Debug.
pub fn level_for(verbosity: u8) -> LevelFilter {
    match verbosity {
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        _ => LevelFilter::Debug,
    }
}

impl Logger {
    pub fn new(verbosity: u8) -> Self {
        Self {
            verbosity: AtomicU8::new(verbosity),
            colour: std::io::stderr().is_terminal(),
        }
    }

    /// Installs the logger for the process; a second call is a no-op.
    pub fn install(verbosity: u8) {
        let logger = LOGGER.get_or_init(|| Self::new(verbosity));
        if log::set_logger(logger).is_ok() {
            log::set_max_level(logger.level());
        }
    }

    fn level(&self) -> LevelFilter {
        level_for(self.verbosity.load(Ordering::Relaxed))
    }

    fn write_record(&self, out: &mut dyn Write, record: &Record) -> std::io::Result<()> {
        let message = record.args().to_string();
        let line = format_line(self.colour, record.level(), record.target(), &message);
        out.write_all(line.as_bytes())
    }
}

/// One log line, as the logger writes it: `message` has its controls
/// escaped, and the level is painted when `colour`. Takes no lock.
pub fn format_line(colour: bool, level: Level, target: &str, message: &str) -> String {
    let message = escape_controls(message);
    let level = paint(colour, colour_code(level), level.as_str());
    format!("{level} {target}: {message}\n")
}

/// `text` with every control character but `\n` and `\t` shown as a visible
/// `\xNN` escape (ESC becomes `\x1b`), so text from a host cannot move the
/// cursor, set the title or emit OSC sequences on the terminal. For human
/// output only: `--json` is escaped by serde.
pub fn escape_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_escaped) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if is_escaped(c) {
            let _ = write!(out, "\\x{:02x}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

pub const BOLD_CYAN: &str = "1;36";
pub const CYAN: &str = "36";
pub const DIM: &str = "2";
pub const YELLOW: &str = "33";
pub const GREEN: &str = "32";
pub const RED: &str = "31";

/// `text` in the SGR colour `code` when `colour` is on, else as it is.
pub fn paint(colour: bool, code: &str, text: &str) -> String {
    if colour {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Whether [`escape_controls`] rewrites `c`: every control but `\n` and `\t`.
pub fn is_escaped(c: char) -> bool {
    c.is_control() && c != '\n' && c != '\t'
}

fn colour_code(level: Level) -> &'static str {
    match level {
        Level::Error => RED,
        Level::Warn => YELLOW,
        Level::Info => GREEN,
        Level::Debug | Level::Trace => CYAN,
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let sink = SINK.read().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(sink) = sink {
            let message = record.args().to_string();
            sink.line(record.level(), record.target(), &escape_controls(&message));
            return;
        }
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        let _ = self.write_record(&mut out, record);
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Serializes the tests that touch the process-wide sink or level.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(level: Level) -> Metadata<'static> {
        Metadata::builder().level(level).target("anvil").build()
    }

    fn plain(verbosity: u8) -> Logger {
        Logger {
            verbosity: AtomicU8::new(verbosity),
            colour: false,
        }
    }

    fn render(logger: &Logger, level: Level, args: std::fmt::Arguments) -> String {
        let record = Record::builder()
            .metadata(metadata(level))
            .args(args)
            .build();
        let mut buf = Vec::new();
        logger.write_record(&mut buf, &record).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[derive(Default)]
    struct Lines(std::sync::Mutex<Vec<(Level, String, String)>>);

    impl Sink for Lines {
        fn line(&self, level: Level, target: &str, message: &str) {
            let line = (level, target.to_owned(), message.to_owned());
            self.0.lock().unwrap().push(line);
        }
    }

    fn log_to(logger: &Logger, level: Level, args: std::fmt::Arguments) {
        let record = Record::builder()
            .metadata(metadata(level))
            .args(args)
            .build();
        logger.log(&record);
    }

    #[test]
    fn an_installed_sink_gets_escaped_uncoloured_lines() {
        let _lock = test_lock();
        let lines = Arc::new(Lines::default());
        set_sink(Some(lines.clone()));
        let logger = Logger {
            verbosity: AtomicU8::new(1),
            colour: true,
        };
        log_to(&logger, Level::Warn, format_args!("host said \x1b[2Jhi"));
        log_to(&logger, Level::Info, format_args!("below the level"));
        set_sink(None);
        log_to(
            &logger,
            Level::Error,
            format_args!("to stderr, not the sink"),
        );
        assert_eq!(
            *lines.0.lock().unwrap(),
            [(
                Level::Warn,
                "anvil".to_owned(),
                "host said \\x1b[2Jhi".to_owned()
            )]
        );
    }

    #[test]
    fn set_verbosity_changes_the_level_at_runtime() {
        let _lock = test_lock();
        let before = log::max_level();
        set_verbosity(0);
        let logger = LOGGER.get().unwrap();
        assert!(!logger.enabled(&metadata(Level::Info)));
        assert_eq!(log::max_level(), LevelFilter::Error);
        set_verbosity(2);
        assert!(logger.enabled(&metadata(Level::Info)));
        assert!(!logger.enabled(&metadata(Level::Debug)));
        assert_eq!(log::max_level(), LevelFilter::Info);
        log::set_max_level(before);
    }

    #[test]
    fn level_follows_the_verbose_count() {
        assert_eq!(level_for(0), LevelFilter::Error);
        assert_eq!(level_for(1), LevelFilter::Warn);
        assert_eq!(level_for(2), LevelFilter::Info);
        assert_eq!(level_for(3), LevelFilter::Debug);
        assert_eq!(level_for(7), LevelFilter::Debug);
    }

    #[test]
    fn warn_hidden_without_v_shown_with_one() {
        assert!(!plain(0).enabled(&metadata(Level::Warn)));
        assert!(plain(0).enabled(&metadata(Level::Error)));
        assert!(plain(1).enabled(&metadata(Level::Warn)));
        assert!(!plain(1).enabled(&metadata(Level::Info)));
    }

    #[test]
    fn info_from_two_debug_from_three_never_trace() {
        assert!(plain(2).enabled(&metadata(Level::Info)));
        assert!(!plain(2).enabled(&metadata(Level::Debug)));
        assert!(plain(3).enabled(&metadata(Level::Debug)));
        assert!(!plain(3).enabled(&metadata(Level::Trace)));
        assert!(!plain(9).enabled(&metadata(Level::Trace)));
    }

    #[test]
    fn line_is_level_target_message() {
        assert_eq!(
            render(&plain(1), Level::Warn, format_args!("hello {}", 42)),
            "WARN anvil: hello 42\n"
        );
    }

    #[test]
    fn colour_wraps_only_the_level() {
        let logger = Logger {
            verbosity: AtomicU8::new(0),
            colour: true,
        };
        assert_eq!(
            render(&logger, Level::Error, format_args!("boom")),
            "\x1b[31mERROR\x1b[0m anvil: boom\n"
        );
    }

    #[test]
    fn control_characters_but_newline_and_tab_are_escaped() {
        assert!(matches!(escape_controls("plain\ttext\n"), Cow::Borrowed(_)));
        assert_eq!(
            escape_controls("a\x1b]0;pwned\x07b\x1b[2J\r\n\tc\x7f\u{9b}31m"),
            "a\\x1b]0;pwned\\x07b\\x1b[2J\\x0d\n\tc\\x7f\\x9b31m"
        );
        assert_eq!(escape_controls("caf\u{e9} \u{2603}"), "caf\u{e9} \u{2603}");
    }

    #[test]
    fn log_messages_are_escaped_but_the_colour_is_not() {
        let logger = Logger {
            verbosity: AtomicU8::new(0),
            colour: true,
        };
        assert_eq!(
            render(
                &logger,
                Level::Error,
                format_args!("cannot connect: \x1b[2Jx")
            ),
            "\x1b[31mERROR\x1b[0m anvil: cannot connect: \\x1b[2Jx\n"
        );
    }
}
