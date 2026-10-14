// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/fortran.py, graphify/llm.py
//! Native bounded subprocess execution for upstream command integrations.
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    time::Duration,
};
use wait_timeout::ChildExt;

pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn capture(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    capture_with_input(command, timeout, None)
}

pub fn capture_with_input(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
) -> io::Result<Output> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let writer = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || stdin.write_all(&bytes))
    });
    // Drain both pipes concurrently, including during the deadline wait.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = child.wait_timeout(timeout);
    if !matches!(status, Ok(Some(_))) {
        #[cfg(unix)]
        {
            // cpp can be a wrapper that spawns a compiler. Kill that command's
            // private process group so inherited output pipes cannot hang us.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = out
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if let Some(writer) = writer {
        // communicate() tolerates a child closing stdin before consuming it.
        if let Err(error) = writer
            .join()
            .map_err(|_| io::Error::other("stdin writer panicked"))?
            && error.kind() != io::ErrorKind::BrokenPipe
            && matches!(status, Ok(Some(_)))
        {
            return Err(error);
        }
    }
    let Some(status) = status? else {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
    };
    Ok(Output {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn drains_both_streams_and_stops_timed_out_child() {
        let input = "stdin 😀 payload\n".repeat(20000).into_bytes();
        let echoed = capture_with_input(
            &mut Command::new("/bin/cat"),
            Duration::from_secs(5),
            Some(input.clone()),
        )
        .unwrap();
        assert!(echoed.success);
        assert_eq!(echoed.stdout, input);
        let result = capture(
            Command::new("/bin/sh").args([
                "-c",
                "i=0; while [ $i -lt 15000 ]; do echo out; echo err >&2; i=$((i+1)); done",
            ]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(result.success);
        assert_eq!(result.stdout.len(), 60000);
        assert_eq!(result.stderr.len(), 60000);
        assert_eq!(
            capture(
                Command::new("/bin/sleep").arg("2"),
                Duration::from_millis(30)
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::TimedOut
        );
        let started = std::time::Instant::now();
        assert_eq!(
            capture(
                Command::new("/bin/sh").args(["-c", "sleep 10 & wait"]),
                Duration::from_millis(30)
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
