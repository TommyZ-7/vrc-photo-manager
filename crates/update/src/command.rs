//! Bounded execution of external commands.
//!
//! Package managers and `pkexec` can wait indefinitely for authentication or
//! emit large amounts of output. Callers use [`run_bounded`] so a command has
//! an overall deadline and its pipes are drained into a bounded tail instead
//! of an unbounded `Vec`.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Result, UpdateError};

/// Maximum number of bytes captured from each stream. stdout and stderr
/// together are bounded to twice this amount.
pub(crate) const OUTPUT_LIMIT: usize = 32 * 1024;

/// A captured stdout or stderr stream.
#[derive(Debug, Clone, Default)]
pub(crate) struct CapturedStream {
    bytes: Vec<u8>,
    omitted: u64,
}

impl CapturedStream {
    pub(crate) fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        if self.omitted == 0 {
            text.into_owned()
        } else {
            format!("... ({} bytes omitted)\n{text}", self.omitted)
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// The status and bounded output of a finished command.
#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: CapturedStream,
    pub(crate) stderr: CapturedStream,
}

impl CommandOutput {
    pub(crate) fn success(&self) -> bool {
        self.status.success()
    }

    /// stdout followed by stderr, matching what the user-facing errors show.
    pub(crate) fn combined(&self) -> String {
        combine(&self.stdout, &self.stderr)
    }
}

fn combine(stdout: &CapturedStream, stderr: &CapturedStream) -> String {
    let stdout_text = stdout.text();
    let stderr_text = stderr.text();
    if stdout_text.is_empty() || stderr_text.is_empty() {
        return format!("{stdout_text}{stderr_text}");
    }
    format!("{stdout_text}\n{stderr_text}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

fn read_bounded(mut reader: impl Read) -> CapturedStream {
    let mut captured = Vec::new();
    let mut omitted = 0u64;
    let mut buffer = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                captured.extend_from_slice(&buffer[..read]);
                if captured.len() > OUTPUT_LIMIT {
                    let excess = captured.len() - OUTPUT_LIMIT;
                    captured.drain(..excess);
                    omitted += excess as u64;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    CapturedStream {
        bytes: captured,
        omitted,
    }
}

fn collect_output(
    receiver: &mpsc::Receiver<(Stream, CapturedStream)>,
) -> (CapturedStream, CapturedStream) {
    // The child has exited or been killed, so its pipe ends must reach EOF.
    // Do not wait forever if a grandchild escaped the process group and still
    // holds the pipe; the captured tail is still useful for the error.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stdout = CapturedStream::default();
    let mut stderr = CapturedStream::default();
    let mut received = 0;
    while received < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok((Stream::Stdout, stream)) => {
                stdout = stream;
                received += 1;
            }
            Ok((Stream::Stderr, stream)) => {
                stderr = stream;
                received += 1;
            }
            Err(_) => break,
        }
    }
    (stdout, stderr)
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child was placed in its own process group. Kill the whole group
        // so package manager subprocesses cannot keep the pipes open.
        let pid = child.id();
        if pid > 0 && pid <= i32::MAX as u32 {
            // SAFETY: `kill` only takes a pid and a signal; a negative pid
            // addresses the process group created for this child.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

/// Runs a command with an overall deadline, draining stdout and stderr into
/// bounded buffers.
///
/// On timeout the child and its process group are killed and reaped before a
/// [`UpdateError::CommandTimeout`] is returned.
pub(crate) fn run_bounded(command: &mut Command, timeout: Duration) -> Result<CommandOutput> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| UpdateError::io(program.as_str(), error))?;
    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let sender = sender.clone();
        thread::spawn(move || {
            let _ = sender.send((Stream::Stdout, read_bounded(stdout)));
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = sender.clone();
        thread::spawn(move || {
            let _ = sender.send((Stream::Stderr, read_bounded(stderr)));
        });
    }
    drop(sender);

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate(&mut child);
                let _ = child.wait();
                return Err(UpdateError::io(program.as_str(), error));
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate(&mut child);
            let _ = child.wait();
            let (stdout, stderr) = collect_output(&receiver);
            return Err(UpdateError::CommandTimeout {
                program,
                timeout,
                output: combine(&stdout, &stderr),
            });
        }
        thread::sleep(Duration::from_millis(20).min(remaining));
    };

    let (stdout, stderr) = collect_output(&receiver);
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_exceeds_the_deadline_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow.sh");
        let pid_file = dir.path().join("pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        make_executable(&script);

        let mut command = Command::new(&script);
        let started = Instant::now();
        let error = run_bounded(&mut command, Duration::from_secs(1)).unwrap_err();

        assert!(
            matches!(error, UpdateError::CommandTimeout { .. }),
            "unexpected error: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::helper::process_alive(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !crate::helper::process_alive(pid),
            "the timed out process must be killed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_is_bounded_but_the_pipes_are_drained() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("noisy.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             i=0\n\
             while [ \"$i\" -lt 128 ]; do\n\
               printf '%1024s' ''\n\
               printf '%1024s' '' >&2\n\
               i=$((i + 1))\n\
             done\n\
             printf 'STDOUT-TAIL'\n\
             printf 'STDERR-TAIL' >&2\n",
        )
        .unwrap();
        make_executable(&script);

        let mut command = Command::new(&script);
        let output = run_bounded(&mut command, Duration::from_secs(10)).unwrap();

        assert!(output.success());
        assert!(output.stdout.len() <= OUTPUT_LIMIT);
        assert!(output.stderr.len() <= OUTPUT_LIMIT);
        assert!(output.stdout.text().contains("STDOUT-TAIL"));
        assert!(output.stderr.text().contains("STDERR-TAIL"));
        assert!(output.stdout.text().contains("bytes omitted"));
        assert!(output.stderr.text().contains("bytes omitted"));
    }
}
