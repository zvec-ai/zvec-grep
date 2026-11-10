//! Runs the real CLI against a pseudoterminal and verifies terminal restoration.

use rustix::{
    pty::{self, OpenptFlags},
    termios,
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub fn run(command: Command, actions: &[(&str, &[u8])]) -> Output {
    run_actions(
        command,
        actions
            .iter()
            .map(|(marker, keys)| (*marker, Action::Keys(keys))),
    )
}

/// Interrupt after a complete frame and confirmation that the operation is gated.
pub fn run_with_sigint(command: Command, ready: mpsc::Receiver<()>) -> Output {
    run_actions(command, [("\x1b[K", Action::Interrupt(ready))])
}

enum Action<'a> {
    Keys(&'a [u8]),
    Interrupt(mpsc::Receiver<()>),
}

impl Action<'_> {
    fn apply(self, writer: &mut File, child_id: u32, observed: &str) {
        match self {
            Self::Keys(keys) => {
                writer.write_all(keys).expect("terminal input");
                writer.flush().expect("flush terminal input");
            }
            Self::Interrupt(ready) => {
                // Explicit indexing has no raw input reader. Sending a literal
                // Ctrl-C byte through this PTY would not reliably target the
                // child, which does not own it as a controlling terminal.
                assert!(
                    [
                        "Scanning workspace",
                        "Indexing files",
                        "Preparing model",
                        "Finalizing index",
                    ]
                    .iter()
                    .any(|label| observed.contains(label)),
                    "expected a complete progress frame before SIGINT: {observed}"
                );
                // The test's loopback embedding server holds the operation
                // open. Do not race normal completion between the first frame
                // and spawning kill, even on a fast or heavily loaded runner.
                ready
                    .recv_timeout(Duration::from_secs(60))
                    .expect("index operation reached its fixture gate");
                let signalled = Command::new("/bin/kill")
                    .args(["-s", "INT", &child_id.to_string()])
                    .output()
                    .expect("send SIGINT to CLI");
                assert!(
                    signalled.status.success(),
                    "could not interrupt CLI: {}\n{observed}",
                    String::from_utf8_lossy(&signalled.stderr)
                );
            }
        }
    }
}

fn run_actions<'a>(
    mut command: Command,
    actions: impl IntoIterator<Item = (&'a str, Action<'a>)>,
) -> Output {
    let master = pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open terminal");
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).expect("terminal close-on-exec");
    let flags = rustix::fs::fcntl_getfl(&master).expect("terminal flags");
    rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK)
        .expect("nonblocking terminal");
    pty::grantpt(&master).expect("grant terminal");
    pty::unlockpt(&master).expect("unlock terminal");
    let path = pty::ptsname(&master, Vec::new()).expect("terminal path");
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::ffi::OsStr::from_bytes(path.to_bytes()))
        .expect("open terminal slave");
    let before = termios::tcgetattr(&slave).expect("initial terminal attributes");
    termios::tcsetwinsize(
        &slave,
        termios::Winsize {
            ws_row: 30,
            ws_col: 110,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .expect("terminal size");
    let mut writer = File::from(master);
    let reader = writer.try_clone().expect("terminal reader");
    command
        .stdin(slave.try_clone().expect("stdin"))
        .stdout(Stdio::piped())
        .stderr(slave.try_clone().expect("stderr"))
        .env("TERM", "xterm-256color");
    let mut child = command.spawn().expect("spawn interactive CLI");
    // Command retains its configured descriptors after spawning.
    drop(command);
    let (sender, receiver) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let capture = capture_terminal(reader, sender, stopped.clone());
    let mut observed = String::new();
    let mut cursor = 0;
    for (marker, action) in actions {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(offset) = observed[cursor..].find(marker) {
                cursor += offset + marker.len();
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("interactive prompt {marker:?} not observed: {observed}");
            }
            if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(100)) {
                observed.push_str(&String::from_utf8_lossy(&chunk));
            } else if let Some(status) = child.try_wait().expect("CLI state") {
                panic!("CLI exited before prompt {marker:?}: {status}\n{observed}");
            }
        }
        action.apply(&mut writer, child.id(), &observed[..cursor]);
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().expect("CLI state").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("interactive CLI did not finish: {observed}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut output = child.wait_with_output().expect("interactive CLI result");
    let after = termios::tcgetattr(&slave).expect("restored terminal attributes");
    assert_eq!(
        before.local_modes, after.local_modes,
        "terminal mode must be restored"
    );
    // Drain remaining output without relying on platform-specific PTY EOF behavior.
    stopped.store(true, Ordering::Release);
    output.stderr = capture.join().expect("terminal capture");
    drop(slave);
    drop(writer);
    output
}

fn capture_terminal(
    mut reader: File,
    sender: mpsc::Sender<Vec<u8>>,
    stopped: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    bytes.extend_from_slice(&chunk[..count]);
                    let _ = sender.send(chunk[..count].to_vec());
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        bytes
    })
}
