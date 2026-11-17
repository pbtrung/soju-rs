//! Logging, in the format of Go's standard log package.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const TIME_FORMAT: &str = "%Y/%m/%d %H:%M:%S";

struct Root {
    debug: AtomicBool,
}

/// A logger with an optional prefix. Cloning is cheap; all clones share the
/// debug flag.
#[derive(Clone)]
pub struct Logger {
    root: Arc<Root>,
    prefix: Arc<str>,
}

impl Logger {
    pub fn new(debug: bool) -> Logger {
        Logger {
            root: Arc::new(Root {
                debug: AtomicBool::new(debug),
            }),
            prefix: Arc::from(""),
        }
    }

    /// Returns a logger which prepends prefix to every line.
    pub fn with_prefix(&self, prefix: impl AsRef<str>) -> Logger {
        Logger {
            root: self.root.clone(),
            prefix: Arc::from(format!("{}{}", self.prefix, prefix.as_ref())),
        }
    }

    pub fn debug_enabled(&self) -> bool {
        self.root.debug.load(Ordering::Relaxed)
    }

    /// Sets the debug flag, returning the previous value.
    pub fn set_debug(&self, debug: bool) -> bool {
        self.root.debug.swap(debug, Ordering::Relaxed)
    }

    pub fn print(&self, args: Arguments) {
        let now = jiff::ZonedDateTime::now();
        let now = now.strftime(TIME_FORMAT);
        let line = format!("{now} {}{args}\n", self.prefix);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "Stderr is the final log sink; reporting its failure would recurse or panic during shutdown."
        )]
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }
}

/// Logs a line: `logf!(logger, "x = {}", x)`.
#[macro_export]
macro_rules! logf {
    ($logger:expr, $($arg:tt)*) => {
        $logger.print(format_args!($($arg)*))
    };
}

/// Logs a line if debug logging is enabled.
#[macro_export]
macro_rules! debugf {
    ($logger:expr, $($arg:tt)*) => {
        if $logger.debug_enabled() {
            $logger.print(format_args!($($arg)*))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::panic,
        reason = "An unexpected child-process timezone must fail the fixture."
    )]
    fn configured_local_time_and_dst() {
        const CHILD: &str = "SOJU_TEST_LOG_TIME_ZONE";
        if std::env::var_os(CHILD).is_some() {
            let zone = jiff::tz::TimeZone::system();
            let expected = match std::env::var("TZ").unwrap().as_str() {
                "America/New_York" => [
                    "2024/03/10 01:59:59",
                    "2024/03/10 03:00:00",
                    "2024/11/03 01:59:59",
                    "2024/11/03 01:00:00",
                ],
                "Asia/Tokyo" => [
                    "2024/03/10 15:59:59",
                    "2024/03/10 16:00:00",
                    "2024/11/03 14:59:59",
                    "2024/11/03 15:00:00",
                ],
                other => panic!("unexpected test timezone: {other}"),
            };
            for (input, expected) in [
                "2024-03-10T06:59:59Z",
                "2024-03-10T07:00:00Z",
                "2024-11-03T05:59:59Z",
                "2024-11-03T06:00:00Z",
            ]
            .into_iter()
            .zip(expected)
            {
                let t = input.parse::<jiff::Timestamp>().unwrap();
                assert_eq!(
                    t.to_zoned_datetime(zone.clone())
                        .strftime(TIME_FORMAT)
                        .to_string(),
                    expected
                );
            }
            return;
        }
        // Each subprocess resolves its own TZ without mutating the environment
        // of parallel tests or relying on Jiff's cached system timezone.
        for zone in ["America/New_York", "Asia/Tokyo"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "logger::tests::configured_local_time_and_dst",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("TZ", zone)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{zone}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
