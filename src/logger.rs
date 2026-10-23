//! Stderr logger behind the `log` facade.
//!
//! Lines look like `<LEVEL> <target>: <message>`; no timestamps. Colour is used
//! only when stderr is a terminal. The level comes from the `-v` count.
//! Messages can carry text from a host (ssh errors, a script's stderr), so
//! their control characters are escaped: a host cannot drive the terminal.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

use log::{Level, LevelFilter, Log, Metadata, Record};

pub struct Logger {
    level: LevelFilter,
    colour: bool,
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
            level: level_for(verbosity),
            colour: std::io::stderr().is_terminal(),
        }
    }

    /// Installs the logger for the process; a second call is a no-op.
    pub fn install(verbosity: u8) {
        static LOGGER: OnceLock<Logger> = OnceLock::new();
        let logger = LOGGER.get_or_init(|| Self::new(verbosity));
        if log::set_logger(logger).is_ok() {
            log::set_max_level(logger.level);
        }
    }

    fn write_record(&self, out: &mut dyn Write, record: &Record) -> std::io::Result<()> {
        let level = record.level();
        let message = record.args().to_string();
        let message = escape_controls(&message);
        let level = paint(self.colour, colour_code(level), level.as_str());
        writeln!(out, "{level} {}: {message}", record.target())
    }
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
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(level: Level) -> Metadata<'static> {
        Metadata::builder().level(level).target("anvil").build()
    }

    fn plain(verbosity: u8) -> Logger {
        Logger {
            level: level_for(verbosity),
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
            level: LevelFilter::Error,
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
            level: LevelFilter::Error,
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
