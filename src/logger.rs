//! Stderr logger behind the `log` facade (D11).
//!
//! Lines look like `<LEVEL> <target>: <message>`; no timestamps. Colour is used
//! only when stderr is a terminal. The level comes from the `-v` count.

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
        if self.colour {
            writeln!(
                out,
                "\x1b[{}m{level}\x1b[0m {}: {}",
                colour_code(level),
                record.target(),
                record.args()
            )
        } else {
            writeln!(out, "{level} {}: {}", record.target(), record.args())
        }
    }
}

fn colour_code(level: Level) -> u8 {
    match level {
        Level::Error => 31,
        Level::Warn => 33,
        Level::Info => 32,
        Level::Debug | Level::Trace => 36,
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
}
