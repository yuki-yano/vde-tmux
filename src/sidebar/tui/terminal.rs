use std::io::{self, Write};
use std::panic;
use std::sync::Once;

use anyhow::Result;
use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

#[cfg(test)]
mod tests;

static PANIC_RESTORE_HOOK: Once = Once::new();
pub(super) struct TerminalRestoreGuard {
    pub(super) active: bool,
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let mut stderr = io::stderr();
            let _ = restore_terminal_after_panic(&mut stderr);
        }
    }
}

pub(super) fn install_panic_restore_hook() {
    PANIC_RESTORE_HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let mut stderr = io::stderr();
            let _ = restore_terminal_after_panic(&mut stderr);
            previous(info);
        }));
    });
}

fn restore_terminal_after_panic<W: Write>(writer: &mut W) -> Result<()> {
    let _ = disable_raw_mode();
    execute!(writer, DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}
