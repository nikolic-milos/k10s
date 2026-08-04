//! The local terminal transport: the user's shell on a PTY behind the same
//! [`ExecSession`] seam the cluster exec uses.
//!
//! One terminal implementation serves both transports on purpose -- the view
//! consumes labelled [`ExecEvent`]s and writes bytes, and never learns
//! whether the far side is a kube exec websocket or a forked shell. The PTY
//! comes from `alacritty_terminal`'s tty module (spawn, setsid, SIGHUP on
//! drop are its battle-tested problems, not ours); only the transport lives
//! here: a reader thread that forwards output until the shell exits, a dup'd
//! writer for keystrokes, and a resize that reaches the kernel's window size.
//! Dropping the session hangs up and reaps the child.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, Options, Pty, Shell};

use crate::provider::{ExecEvent, ExecSession, NullExecSession};

// The kernel tracks a pixel size alongside rows and columns; nothing reads
// it here, but zero would be a lie some programs trip over.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

/// The user's shell, `$SHELL` or `/bin/sh`, as an [`ExecSession`]. Failures
/// arrive as the same labelled events a refused exec produces.
pub fn spawn_local_shell(on_event: Box<dyn Fn(ExecEvent) + Send + Sync>) -> Box<dyn ExecSession> {
    let program = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    spawn_shell(Shell::new(program, Vec::new()), on_event)
}

pub fn spawn_shell(
    shell: Shell,
    on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
) -> Box<dyn ExecSession> {
    let on_event: Arc<dyn Fn(ExecEvent) + Send + Sync> = Arc::from(on_event);
    match start(shell, on_event.clone()) {
        Ok(session) => Box::new(session),
        Err(why) => {
            on_event(ExecEvent::Failed(why));
            Box::new(NullExecSession)
        }
    }
}

struct LocalSession {
    // Resize needs `&mut Pty`; everything else works on dup'd fds.
    pty: Mutex<Pty>,
    writer: File,
}

impl ExecSession for LocalSession {
    fn write(&self, bytes: &[u8]) {
        let _ = (&self.writer).write_all(bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        if let Ok(mut pty) = self.pty.lock() {
            pty.on_resize(WindowSize {
                num_lines: rows.max(2),
                num_cols: cols.max(2),
                cell_width: CELL_WIDTH,
                cell_height: CELL_HEIGHT,
            });
        }
    }
}

fn start(
    shell: Shell,
    on_event: Arc<dyn Fn(ExecEvent) + Send + Sync>,
) -> Result<LocalSession, String> {
    let label = |what: &str, error: &dyn std::fmt::Display| {
        format!("cannot start a local shell: {what} ({error})")
    };
    let size = WindowSize {
        num_lines: 24,
        num_cols: 80,
        cell_width: CELL_WIDTH,
        cell_height: CELL_HEIGHT,
    };
    let options = Options {
        shell: Some(shell),
        working_directory: None,
        drain_on_exit: false,
        env: HashMap::from([
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ]),
    };
    let pty = tty::new(&options, size, 0).map_err(|error| label("spawn failed", &error))?;

    // tty::new hands the master back non-blocking for alacritty's own event
    // loop; this transport reads on a dedicated thread, so blocking is the
    // correct mode -- otherwise the reader would spin on WouldBlock.
    let flags = rustix::fs::fcntl_getfl(pty.file())
        .map_err(|error| label("cannot read the PTY flags", &error))?;
    rustix::fs::fcntl_setfl(pty.file(), flags.difference(rustix::fs::OFlags::NONBLOCK))
        .map_err(|error| label("cannot make the PTY blocking", &error))?;

    let mut reader = pty
        .file()
        .try_clone()
        .map_err(|error| label("cannot clone the PTY fd", &error))?;
    let writer = pty
        .file()
        .try_clone()
        .map_err(|error| label("cannot clone the PTY fd", &error))?;

    std::thread::Builder::new()
        .name("k10s-local-pty".to_string())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => on_event(ExecEvent::Output(buffer[..read].to_vec())),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    // EIO is how a Linux PTY master says the shell is gone;
                    // every read error after SIGHUP means the same thing.
                    Err(_) => break,
                }
            }
            on_event(ExecEvent::Ended("the shell exited".to_string()));
        })
        .map_err(|error| label("cannot spawn the reader thread", &error))?;

    Ok(LocalSession {
        pty: Mutex::new(pty),
        writer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn session_with(command: &str) -> (Box<dyn ExecSession>, mpsc::Receiver<ExecEvent>) {
        let (tx, rx) = mpsc::channel();
        let session = spawn_shell(
            Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), command.to_string()],
            ),
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        );
        (session, rx)
    }

    // Accumulate output until the needle shows up; panics with everything
    // seen so far if the session goes quiet or ends first.
    fn wait_for(rx: &mpsc::Receiver<ExecEvent>, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for {needle:?} in {seen:?}"));
            match rx.recv_timeout(left) {
                Ok(ExecEvent::Output(bytes)) => {
                    seen.extend_from_slice(&bytes);
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if text.contains(needle) {
                        return text;
                    }
                }
                Ok(other) => panic!("the session ended before {needle:?}: {other:?}, {seen:?}"),
                Err(_) => panic!("timed out waiting for {needle:?} in {seen:?}"),
            }
        }
    }

    #[test]
    fn a_local_shell_round_trips_bytes_through_the_pty() {
        let (session, rx) = session_with("printf ready; cat");
        wait_for(&rx, "ready");
        session.write(b"ping\n");
        wait_for(&rx, "ping");
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("dropping the session must end it");
            match rx.recv_timeout(left) {
                Ok(ExecEvent::Ended(_)) => break,
                Ok(_) => continue,
                Err(_) => panic!("dropping the session must end it"),
            }
        }
    }

    #[test]
    fn a_resize_reaches_the_kernels_idea_of_the_window() {
        let (session, rx) = session_with("printf ready; read _line; stty size");
        wait_for(&rx, "ready");
        session.resize(120, 40);
        session.write(b"\n");
        wait_for(&rx, "40 120");
    }

    #[test]
    fn a_shell_that_cannot_spawn_fails_as_a_labelled_event() {
        let (tx, rx) = mpsc::channel();
        let session = spawn_shell(
            Shell::new("/no/such/shell-anywhere".to_string(), Vec::new()),
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        );
        session.write(b"into the void");
        session.resize(80, 24);
        let event = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a spawn failure must be reported");
        match event {
            ExecEvent::Failed(why) => assert!(why.contains("cannot start"), "{why}"),
            // A PTY can also spawn first and die at exec time, which arrives
            // as an immediate end; either is a labelled outcome, not silence.
            ExecEvent::Ended(_) | ExecEvent::Output(_) => {}
            ExecEvent::Denied(_) => panic!("a local shell has no RBAC to deny"),
        }
    }
}
