//! Fallible stdout for CLI hosts. A closed downstream pipe is a normal stop,
//! not a panic from `println!`.
use std::{
    io::{self, BufWriter, Stdout, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub struct CliOutput {
    out: Mutex<BufWriter<Stdout>>,
    broken: AtomicBool,
}

impl CliOutput {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            out: Mutex::new(BufWriter::new(io::stdout())),
            broken: AtomicBool::new(false),
        })
    }

    pub fn broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn mark_broken(&self) {
        self.broken.store(true, Ordering::Relaxed);
    }

    pub fn line(&self, value: &str) -> io::Result<()> {
        self.write(value.as_bytes(), true)
    }

    pub fn text(&self, value: &str) -> io::Result<()> {
        self.write(value.as_bytes(), false)
    }

    fn write(&self, bytes: &[u8], newline: bool) -> io::Result<()> {
        if self.broken() {
            return Ok(());
        }
        let mut out = self.out.lock().expect("stdout lock poisoned");
        let result = (|| {
            out.write_all(bytes)?;
            if newline {
                out.write_all(b"\n")?;
            }
            out.flush()
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.broken.store(true, Ordering::Relaxed);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub fn flush(&self) -> io::Result<()> {
        if self.broken() {
            return Ok(());
        }
        self.out
            .lock()
            .expect("stdout lock poisoned")
            .flush()
            .or_else(|error| {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    self.broken.store(true, Ordering::Relaxed);
                    Ok(())
                } else {
                    Err(error)
                }
            })
    }
}

/// Print a line without turning a closed downstream pipe into a panic.
/// Prompts still flush explicitly; model output uses `CliOutput`.
#[macro_export]
macro_rules! cli_println {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(error) = writeln!(std::io::stdout(), $($arg)*) {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                let _ = writeln!(std::io::stderr(), "stdout: {error}");
            }
        }
    }};
}

#[macro_export]
macro_rules! cli_print {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(error) = write!(std::io::stdout(), $($arg)*) {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                let _ = writeln!(std::io::stderr(), "stdout: {error}");
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_pipe_is_recorded_instead_of_panicking() {
        let output = CliOutput::new();
        // A real pipe close is covered by process tests; this verifies the flag
        // short-circuits later writes without panicking.
        output.broken.store(true, Ordering::Relaxed);
        assert!(output.line("ignored").is_ok());
        assert!(output.text("ignored").is_ok());
        assert!(output.broken());
    }
}
