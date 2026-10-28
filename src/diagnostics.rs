//! Keep the TUI's terminal surface free of normal runtime diagnostics.
//! Non-TUI behavior is unchanged; no global logger or framework is needed.
use std::sync::atomic::{AtomicBool, Ordering};

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn quiet() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

pub struct TuiLogGuard;

impl TuiLogGuard {
    pub fn enter() -> Self {
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for TuiLogGuard {
    fn drop(&mut self) {
        TUI_ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[macro_export]
macro_rules! runtime_log {
    ($($arg:tt)*) => {{
        if !$crate::diagnostics::quiet() { std::eprintln!($($arg)*); }
    }};
}
