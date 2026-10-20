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

pub fn run(mut command: Command, actions: &[(&str, &[u8])]) -> Output {
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
    for (marker, keys) in actions {
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
        writer.write_all(keys).expect("terminal input");
        writer.flush().expect("flush terminal input");
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
