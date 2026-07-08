//! Crossterm commands for the terminal's alternate-scroll mode.
//!
//! Vanilla crossterm has no alternate-scroll command, so these emit DECSET/DECRST
//! `?1007` directly. While the alternate screen is active, the terminal translates
//! mouse-wheel rotation into `Up`/`Down` arrow-key sequences. Combined with keeping
//! mouse capture off, this gives wheel scrolling of the chat without sacrificing the
//! terminal's native click-drag text selection. Drop these if a future crossterm
//! release adds native support.

use std::fmt;

use ratatui::crossterm::Command;

/// Enable alternate-scroll mode (DECSET `?1007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "tried to execute EnableAlternateScroll using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Disable alternate-scroll mode (DECRST `?1007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "tried to execute DisableAlternateScroll using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}
