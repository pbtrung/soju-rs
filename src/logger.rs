//! Logging, in the format of Go's standard log package.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        let now = chrono::Local::now().format("%Y/%m/%d %H:%M:%S");
        let line = format!("{now} {}{args}\n", self.prefix);
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
