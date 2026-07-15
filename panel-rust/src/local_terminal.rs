//! Client-local PTY terminal -- distinct from the agent-created
//! `terminal/create` relay (`terminal_card.slint`'s read-only view of a
//! *backend*-spawned process, streamed over `acpx/terminal_output`).
//! This is a real interactive terminal the **client itself** spawns (a
//! local shell), matching the "close to a real terminal UI" requirement
//! called out for a client-initiated terminal in this plan's Phase 1
//! terminal-view note -- real PTY (`portable-pty`, the same crate
//! `wezterm`/`zed` use), real VT100 screen state (`vt100::Parser`,
//! cursor-tracked, not raw byte passthrough), typed input write-through,
//! and live resize.
//!
//! Deliberately monochrome rendering (`Screen::contents()`, which
//! strips SGR color/attribute codes and returns plain text) rather than
//! per-cell ANSI color reproduction -- this project's established
//! design language is monochrome/no-status-color throughout (see
//! `component-style-system.md`), and `vt100::Parser` still gives real
//! cursor tracking, line-wrapping, and control-sequence handling
//! (clear-screen, cursor movement, etc.) even though color attributes
//! are discarded at render time. A future increment could read
//! `Screen::cell(row, col).fgcolor()`/`bgcolor()` per cell for a colored
//! render without changing this module's PTY/parser plumbing at all.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

fn to_io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}

/// One live client-local PTY session. Owns the master side of the PTY,
/// a background reader thread that feeds every byte the shell produces
/// into a shared `vt100::Parser`, and the child process handle.
pub struct LocalTerminal {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    cols: u16,
    rows: u16,
}

impl LocalTerminal {
    /// Spawns `$SHELL` (falling back to `/bin/sh`) attached to a fresh
    /// PTY of the given size. Real OS process, real PTY -- no
    /// simulation. The background reader thread runs for the lifetime
    /// of this value and exits on its own once the PTY's read side
    /// returns EOF or an error (shell exited, PTY closed).
    pub fn spawn(cols: u16, rows: u16) -> std::io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io_err)?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new(shell))
            .map_err(to_io_err)?;
        // Drop our copy of the slave fd once the child has it -- without
        // this, the master's read side never sees EOF after the child
        // exits (a second open slave fd, this process's own, keeps the
        // PTY "held open").
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(to_io_err)?;
        let writer = pair.master.take_writer().map_err(to_io_err)?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 4000)));
        let parser_for_reader = Arc::clone(&parser);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut parser) = parser_for_reader.lock() {
                            parser.process(&buf[..n]);
                        } else {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            cols,
            rows,
        })
    }

    /// Writes raw bytes to the PTY's input side, exactly as a real
    /// terminal emulator forwards keystrokes -- callers are expected to
    /// translate a Slint key event into the right byte sequence first
    /// (plain UTF-8 for printable characters, `\r` for Enter, `\x7f` for
    /// Backspace, `\x03` for Ctrl-C, etc. -- see `lib.rs`'s
    /// `translate_key_event`).
    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    /// Live resize -- both the OS-level PTY (so the shell's own
    /// `SIGWINCH`-driven redraw, e.g. a re-wrapped `$PS1` or a running
    /// TUI program, sees the real new size) and the VT100 parser's own
    /// screen buffer (so `screen_text()` reflects the new dimensions
    /// immediately, not just after the next byte arrives).
    pub fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io_err)?;
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_size(rows, cols);
        }
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    /// The parser's rendered visible screen as plain text (one line per
    /// row, `\n`-joined, colors/attributes stripped -- see this module's
    /// doc comment). Real cursor-tracked VT100 state (line wrapping,
    /// cursor movement, clear-screen, etc. all actually interpreted),
    /// not a raw-byte passthrough.
    pub fn screen_text(&self) -> String {
        match self.parser.lock() {
            Ok(parser) => parser.screen().contents(),
            Err(_) => String::new(),
        }
    }

    /// 0-indexed `(row, col)` cursor position within the current
    /// screen, for a caret indicator in the UI.
    pub fn cursor_position(&self) -> (u16, u16) {
        match self.parser.lock() {
            Ok(parser) => parser.screen().cursor_position(),
            Err(_) => (0, 0),
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Non-blocking check for whether the shell process has exited.
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for LocalTerminal {
    /// Kills the shell process when this terminal is closed/dropped --
    /// without this, closing the card (removing it from `AgentBridge`'s
    /// map) would leak a real orphaned shell process for the lifetime
    /// of the panel process. Best-effort: a process that already exited
    /// on its own (e.g. the user typed `exit`) has nothing left to
    /// kill, and `kill()` on an already-dead process is a documented
    /// no-op/error this doesn't need to propagate.
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Real PTY, real shell process, real typed input -- proves a
    /// command sent through `write_input` actually reaches the shell
    /// and its own real stdout reaches back into `screen_text()` through
    /// the real VT100 parser, not a mocked/simulated echo.
    #[test]
    fn write_input_reaches_a_real_shell_and_its_output_reaches_the_screen() {
        let mut term = LocalTerminal::spawn(80, 24).expect("spawn local terminal");
        term.write_input(b"echo LOCAL_PTY_MARKER_12345\r")
            .expect("write_input");
        let seen = wait_for(
            || term.screen_text().contains("LOCAL_PTY_MARKER_12345"),
            Duration::from_secs(10),
        );
        assert!(
            seen,
            "expected the real shell's own echoed output to reach the VT100 screen, got: {:?}",
            term.screen_text()
        );
    }

    /// Proves resize reaches the real PTY (a real shell command,
    /// `stty size`, only ever reports the OS-level PTY's own idea of the
    /// terminal size -- not a client-side property this test could fake)
    /// and the parser's own screen dimensions track it.
    #[test]
    fn resize_reaches_the_real_pty_and_the_parser() {
        let mut term = LocalTerminal::spawn(80, 24).expect("spawn local terminal");
        // Let the shell's prompt render before resizing, so the
        // subsequent `stty size` command doesn't race the shell's own
        // startup.
        wait_for(|| !term.screen_text().trim().is_empty(), Duration::from_secs(5));
        term.resize(100, 40).expect("resize");
        assert_eq!(term.cols(), 100);
        assert_eq!(term.rows(), 40);
        term.write_input(b"stty size\r").expect("write_input");
        let seen = wait_for(
            || term.screen_text().contains("40 100"),
            Duration::from_secs(10),
        );
        assert!(
            seen,
            "expected the real PTY's own stty size to report the resized dimensions, got: {:?}",
            term.screen_text()
        );
    }

    #[test]
    fn has_exited_reflects_a_real_process_exit() {
        let mut term = LocalTerminal::spawn(80, 24).expect("spawn local terminal");
        assert!(!term.has_exited(), "freshly spawned shell should not have exited yet");
        term.write_input(b"exit\r").expect("write_input");
        let exited = wait_for(|| term.has_exited(), Duration::from_secs(10));
        assert!(exited, "expected the real shell process to have exited after `exit`");
    }
}
