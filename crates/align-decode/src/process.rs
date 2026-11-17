//! Helper processes (FFmpeg, the AAF module) started and awaited uniformly.

use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A child process that never opens a console window. The Windows desktop
/// app has no console, so each helper would otherwise flash one; elsewhere
/// this is `Command::new`.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = command;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    command
}

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("Cancelled.")]
    Cancelled,
    #[error("Cannot wait for a helper process: {0}")]
    Io(#[from] std::io::Error),
}

/// Waits for `child`, killing and reaping it as soon as `cancel` is set.
pub fn wait_cancellable(child: &mut Child, cancel: &AtomicBool) -> Result<ExitStatus, WaitError> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WaitError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WaitError::Io(error));
            }
        }
    }
}
