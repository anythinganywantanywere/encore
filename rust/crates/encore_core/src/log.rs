//! Logging, mirroring `EncoreLog`.
//!
//! Verified from `jni/include/EncoreLog.hpp`:
//!
//! - The log file is `encore.log` under the config directory, with
//!   the line pattern `YYYY-MM-DD HH:MM:SS.mmm L message`, where `L`
//!   is the single-letter level.
//! - The initial level is trace, and `flush_on` is info: messages at
//!   info and above are flushed immediately. This logger flushes
//!   after every write, which is at least as durable and simpler to
//!   reason about for a daemon that may be killed.
//! - The config log level is an integer 0-5, mapped as: 0 critical,
//!   1 error, 2 warn, 3 info, 4 debug, 5 trace; any other value maps
//!   to info. The default config value is 4 (debug), so trace
//!   messages are excluded by default.
//! - `set_log_level` is a no-op when the level is unchanged, and
//!   otherwise logs an info message about the change.
//! - Log level changes are applied from the config on reload (see
//!   `jni/InotifyHandler.cpp`).
//!
//! Deliberate differences from C++, required by the task guide:
//!
//! - No size limit or rotation was verified in C++: the spdlog
//!   submodule is not present in this clone, so its sink behaviour
//!   could not be checked from source, and `EncoreLog.hpp` itself
//!   sets no limit. This logger therefore enforces a maximum file
//!   size and a maximum number of rotated files. When the current
//!   file would exceed the limit, it is renamed to `.1`, older
//!   rotated files shift up, and the oldest beyond the limit is
//!   removed, before the new entry is written.
//! - Messages are sanitised to a single line: carriage returns and
//!   newlines are replaced with spaces, so a value containing a
//!   newline cannot forge a log line (log injection).
//! - No sensitive data is logged by this module, and callers must
//!   not pass secrets, tokens, or credentials as tag or message.
//!   Package names, counts, and config paths are the only kinds of
//!   values the daemon commands log in Stage 3.
//! - Timestamps use only `std::time`, with a civil-date conversion
//!   from days since the epoch, so no new dependency is needed.
//!   The timestamp is in UTC, matching the C++ pattern's wall-clock
//!   fields without claiming a local timezone.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default maximum size of one log file: 1 MiB.
pub const DEFAULT_MAX_LOG_BYTES: u64 = 1_048_576;
/// Default number of rotated log files kept, besides the current one.
pub const DEFAULT_MAX_ROTATED_FILES: usize = 3;
/// Default log file permissions.
pub const LOG_FILE_PERMISSIONS: u32 = 0o644;

/// A log level, in spdlog order (trace most verbose, critical least).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Trace, the most verbose level.
    Trace,
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warn.
    Warn,
    /// Error.
    Error,
    /// Critical, the least verbose level.
    Critical,
}

impl LogLevel {
    /// The single-letter code used in log lines, as in spdlog `%L`.
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::Trace => 'T',
            Self::Debug => 'D',
            Self::Info => 'I',
            Self::Warn => 'W',
            Self::Error => 'E',
            Self::Critical => 'C',
        }
    }

    /// The name used in the level-change message.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    /// Map a config log level (0-5) to a level, as in
    /// `EncoreLog::set_log_level`. Any other value maps to info.
    #[must_use]
    pub fn from_config(value: i32) -> Self {
        match value {
            0 => Self::Critical,
            1 => Self::Error,
            2 => Self::Warn,
            // 3 is info, and it also falls through to the default
            // arm below, which is info for any other value.
            4 => Self::Debug,
            5 => Self::Trace,
            _ => Self::Info,
        }
    }

    /// Whether a message at `level` should be emitted when the
    /// current threshold is `self`.
    #[must_use]
    pub fn allows(self, level: Self) -> bool {
        level >= self
    }
}

/// Format a timestamp as `YYYY-MM-DD HH:MM:SS.mmm`, in UTC.
///
/// The date is computed from days since the Unix epoch with the
/// civil-date algorithm, so no date library is needed.
///
/// # Panics
///
/// This function does not panic: times before the epoch are clamped
/// to the epoch, and the algorithm works for the resulting range.
#[must_use]
pub fn format_timestamp(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = duration.as_secs();
    let millis = duration.subsec_millis();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let secs_of_day = seconds % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    // Civil from days, Howard Hinnant's algorithm.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millis:03}")
}

/// Format one log line, sanitising the message to a single line.
#[must_use]
pub fn format_log_line(time: SystemTime, level: LogLevel, tag: &str, message: &str) -> String {
    let clean_tag: String = tag
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let clean_message: String = message
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!(
        "{} {} {clean_tag}: {clean_message}\n",
        format_timestamp(time),
        level.letter()
    )
}

/// Rotate log files at `path`, keeping at most `max_rotated` backups.
///
/// `path` becomes `path.1`, `path.1` becomes `path.2`, and so on.
/// A backup beyond `max_rotated` is removed. Missing files are
/// skipped. Errors are returned so the caller can decide whether
/// logging may continue.
fn rotate(path: &Path, max_rotated: usize) -> io::Result<()> {
    if max_rotated == 0 {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    let oldest = format!("{}.{}", path.display(), max_rotated);
    let _ = fs::remove_file(&oldest);
    let mut index = max_rotated;
    while index > 1 {
        let from = format!("{}.{}", path.display(), index - 1);
        let to = format!("{}.{}", path.display(), index);
        if fs::rename(&from, &to).is_err() {
            // A missing source is normal; any other error is also
            // best-effort here, because rotation must not make a
            // single log write fail the whole daemon command.
            let _ = fs::metadata(&from);
        }
        index -= 1;
    }
    let first = format!("{}.1", path.display());
    fs::rename(path, &first)?;
    Ok(())
}

/// A file logger with a runtime-changeable level and size rotation.
#[derive(Debug)]
pub struct Logger {
    /// Path to the current log file.
    path: PathBuf,
    /// Maximum size of the current log file, in bytes.
    max_bytes: u64,
    /// Number of rotated files kept.
    max_rotated: usize,
    /// Current level threshold, changeable at runtime.
    level: RwLock<LogLevel>,
    /// Last config value passed to `set_level_from_config`, for the
    /// C++ no-op-on-unchanged behaviour.
    last_config_value: RwLock<Option<i32>>,
    /// The open log file, guarded so concurrent writers serialise.
    file: Mutex<Option<File>>,
}

impl Logger {
    /// Create a logger writing to `path`, appending to an existing
    /// file, with the given limits and initial level.
    ///
    /// Parent directories are created if missing. The file is
    /// created with `0644` permissions.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the parent directory cannot be
    /// created, or if the log file cannot be opened.
    pub fn new(
        path: &Path,
        max_bytes: u64,
        max_rotated: usize,
        level: LogLevel,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let _ = file.set_permissions(fs::Permissions::from_mode(LOG_FILE_PERMISSIONS));
        Ok(Self {
            path: path.to_owned(),
            max_bytes,
            max_rotated,
            level: RwLock::new(level),
            last_config_value: RwLock::new(None),
            file: Mutex::new(Some(file)),
        })
    }

    /// Create a logger with the default limits (1 MiB, 3 rotated
    /// files) and the C++ default level for a config value of 4
    /// (debug).
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the log file cannot be opened.
    pub fn with_defaults(path: &Path) -> io::Result<Self> {
        Self::new(
            path,
            DEFAULT_MAX_LOG_BYTES,
            DEFAULT_MAX_ROTATED_FILES,
            LogLevel::Debug,
        )
    }

    /// The path this logger writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current level threshold.
    #[must_use]
    pub fn level(&self) -> LogLevel {
        match self.level.read() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// Set the level from a config value (0-5, other values map to
    /// info), mirroring `EncoreLog::set_log_level`.
    ///
    /// If the config value is unchanged since the last call, this is
    /// a no-op and returns `false`. Otherwise it sets the level and
    /// logs an info message about the change (which may itself be
    /// filtered out if the new level is above info), and returns
    /// `true`.
    pub fn set_level_from_config(&self, value: i32) -> bool {
        let unchanged = match self.last_config_value.read() {
            Ok(guard) => *guard == Some(value),
            Err(poisoned) => *poisoned.into_inner() == Some(value),
        };
        if unchanged {
            return false;
        }
        if let Ok(mut guard) = self.last_config_value.write() {
            *guard = Some(value);
        }
        if let Ok(mut guard) = self.level.write() {
            *guard = LogLevel::from_config(value);
        }
        let _ = self.log(
            LogLevel::Info,
            "EncoreLog",
            &format!(
                "Log level changed to {}",
                LogLevel::from_config(value).name()
            ),
        );
        true
    }

    /// Write one log entry, rotating first if it would exceed the
    /// size limit. Entries below the current level are dropped.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if rotation or writing fails. Callers
    /// in the daemon should treat a log error as best-effort and
    /// continue, not as a fatal error.
    pub fn log(&self, level: LogLevel, tag: &str, message: &str) -> io::Result<()> {
        if !self.level().allows(level) {
            return Ok(());
        }
        let line = format_log_line(SystemTime::now(), level, tag, message);
        let mut guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(file) = guard.as_mut() else {
            return Ok(());
        };
        let current_size = fs::metadata(&self.path).map_or(0, |meta| meta.len());
        if current_size + line.len() as u64 > self.max_bytes && current_size > 0 {
            drop(guard);
            rotate(&self.path, self.max_rotated)?;
            let reopened = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            let _ = reopened.set_permissions(fs::Permissions::from_mode(LOG_FILE_PERMISSIONS));
            let mut guard = match self.file.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = Some(reopened);
            if let Some(file) = guard.as_mut() {
                file.write_all(line.as_bytes())?;
                file.flush()?;
            }
            return Ok(());
        }
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    /// Log at trace level, mirroring the C++ `LOGT` macro.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if writing fails.
    pub fn trace(&self, tag: &str, message: &str) -> io::Result<()> {
        self.log(LogLevel::Trace, tag, message)
    }
    /// Log at debug level.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if writing fails.
    pub fn debug(&self, tag: &str, message: &str) -> io::Result<()> {
        self.log(LogLevel::Debug, tag, message)
    }
    /// Log at info level.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if writing fails.
    pub fn info(&self, tag: &str, message: &str) -> io::Result<()> {
        self.log(LogLevel::Info, tag, message)
    }
    /// Log at warn level.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if writing fails.
    pub fn warn(&self, tag: &str, message: &str) -> io::Result<()> {
        self.log(LogLevel::Warn, tag, message)
    }
    /// Log at error level.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if writing fails.
    pub fn error(&self, tag: &str, message: &str) -> io::Result<()> {
        self.log(LogLevel::Error, tag, message)
    }
    /// Log at critical level.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if writing fails.
    pub fn critical(&self, tag: &str, message: &str) -> io::Result<()> {
        self.log(LogLevel::Critical, tag, message)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_log_line, format_timestamp, LogLevel, Logger};
    use std::fs;
    use std::io;
    use std::time::{Duration, UNIX_EPOCH};

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-log-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    #[test]
    fn timestamp_formats_epoch_and_known_date() {
        assert_eq!(format_timestamp(UNIX_EPOCH), "1970-01-01 00:00:00.000");
        let known = UNIX_EPOCH + Duration::from_hours(497_500);
        assert_eq!(format_timestamp(known), "2026-10-03 04:00:00.000");
    }

    #[test]
    fn config_level_mapping_matches_cpp() {
        assert_eq!(LogLevel::from_config(0), LogLevel::Critical);
        assert_eq!(LogLevel::from_config(1), LogLevel::Error);
        assert_eq!(LogLevel::from_config(2), LogLevel::Warn);
        assert_eq!(LogLevel::from_config(3), LogLevel::Info);
        assert_eq!(LogLevel::from_config(4), LogLevel::Debug);
        assert_eq!(LogLevel::from_config(5), LogLevel::Trace);
        assert_eq!(LogLevel::from_config(99), LogLevel::Info);
        assert_eq!(LogLevel::from_config(-1), LogLevel::Info);
    }

    #[test]
    fn level_filtering_drops_below_threshold() {
        assert!(LogLevel::Debug.allows(LogLevel::Info));
        assert!(!LogLevel::Debug.allows(LogLevel::Trace));
        assert!(LogLevel::Critical.allows(LogLevel::Critical));
        assert!(!LogLevel::Critical.allows(LogLevel::Error));
    }

    #[test]
    fn log_line_sanitises_newlines_and_uses_letter() {
        let line = format_log_line(UNIX_EPOCH, LogLevel::Info, "Encore", "hello\nforged line");
        assert_eq!(
            line,
            "1970-01-01 00:00:00.000 I Encore: hello forged line\n"
        );
    }

    #[test]
    fn logger_filters_and_set_level_is_noop_when_unchanged() -> io::Result<()> {
        let dir = temp_dir_for("filter");
        let path = dir.join("encore.log");
        let logger = Logger::new(&path, 1_048_576, 3, LogLevel::Info)?;
        logger.debug("Test", "should be dropped")?;
        logger.info("Test", "should be kept")?;
        assert!(logger.set_level_from_config(3));
        assert!(!logger.set_level_from_config(3));
        assert!(logger.set_level_from_config(4));
        logger.debug("Test", "now kept")?;
        let text = fs::read_to_string(&path).unwrap_or_default();
        assert!(!text.contains("should be dropped"));
        assert!(text.contains("should be kept"));
        assert!(text.contains("now kept"));
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn logger_rotates_when_exceeding_max_bytes() -> io::Result<()> {
        let dir = temp_dir_for("rotate");
        let path = dir.join("encore.log");
        let logger = Logger::new(&path, 80, 2, LogLevel::Info)?;
        logger.info("Test", "first entry that fills the file")?;
        logger.info("Test", "second entry that must rotate")?;
        logger.info("Test", "third entry")?;
        let current = fs::read_to_string(&path).unwrap_or_default();
        assert!(current.contains("third entry"));
        assert!(!current.contains("first entry"));
        assert!(dir.join("encore.log.1").exists());
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
