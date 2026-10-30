use super::*;

#[test]
fn reused_pid_is_not_terminated_by_automatic_migration() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let unrelated_home = TempDir::new()?;
    // A real process owned by another home stands in for an unrelated process
    // that inherited the PID from a crashed legacy daemon.
    let (mut unrelated, _) = start_server(&binary, &unrelated_home, "agent", None, |_| {})?;
    for args in [vec!["--server", "on"], vec!["--server", "--stdio"]] {
        let home = TempDir::new()?;
        let mut guard = ServerGuard {
            binary: binary.clone(),
            home: home.path().to_owned(),
            listen: format!("127.0.0.1:{}", available_port()?),
            token_file: None,
            active: true,
        };
        let mut stale = instance_record(unrelated_home.path())?;
        let fields = stale.as_object_mut().ok_or("instance record object")?;
        fields.remove("daemonProtocolVersion");
        fields.remove("listen");
        stale["startedAt"] = json!(1);
        stale["updatedAt"] = json!(1);
        stale["serverUrl"] = json!(format!("http://{}/mcp", guard.listen));
        std::fs::create_dir(home.path().join("daemon"))?;
        write_record(home.path(), &stale)?;
        let output = cli(&guard, &args)
            .args(["--listen", &guard.listen])
            .stdin(Stdio::null())
            .output()?;
        let survived = assert_healthy(&unrelated.listen).is_ok();
        // Do not let cleanup signal the PID under test through the stale record.
        if instance_record(home.path())?["pid"] == stale["pid"] {
            guard.active = false;
        }
        assert!(
            survived,
            "migration terminated an unrelated process: {output:?}"
        );
        assert!(
            !output.status.success(),
            "unverified owner must block migration"
        );
        assert_eq!(instance_record(home.path())?, stale);
    }
    assert_command_success(&unrelated.stop()?);
    Ok(())
}

#[test]
fn occupied_configured_target_preserves_the_legacy_daemon() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let legacy = legacy_record(home.path())?;
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let config_home = home.path().join(".zvec-grep");
    std::fs::create_dir(&config_home)?;
    std::fs::write(
        config_home.join("config.json"),
        serde_json::to_vec(&json!({
            "version": 1, "server": { "host": "127.0.0.1", "port": occupied.local_addr()?.port() }
        }))?,
    )?;
    let output = cli(&guard, &["--server", "on"]).output()?;
    assert_failure(&output, "is already in use");
    assert!(
        assert_healthy(&guard.listen).is_ok(),
        "migration stopped the healthy old daemon"
    );
    assert_eq!(instance_record(home.path())?, legacy);
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn unreachable_legacy_endpoint_does_not_force_termination() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let original = instance_record(home.path())?;
    let mut legacy = legacy_record(home.path())?;
    let unavailable = format!("127.0.0.1:{}", available_port()?);
    legacy["serverUrl"] = json!(format!("http://{unavailable}/mcp"));
    write_record(home.path(), &legacy)?;
    let output = cli(&guard, &["--server", "on", "--listen", &unavailable]).output()?;
    let survived = assert_healthy(&guard.listen).is_ok();
    let after = instance_record(home.path())?;
    if survived {
        write_record(home.path(), &original)?;
    }
    assert!(
        survived,
        "migration must not signal a process when shutdown is unreachable"
    );
    assert!(!output.status.success(), "unexpected success: {output:?}");
    // A closed loopback port can reach the HTTP deadline on Windows instead
    // of immediately refusing the connection. Both are failed contact attempts.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon I/O failed")
            || stderr.contains("timed out waiting for zvec-grep server to contact"),
        "expected a connection failure or contact timeout: {output:?}"
    );
    assert_eq!(after, legacy);
    assert_command_success(&guard.stop()?);
    Ok(())
}

struct DecoyServer {
    stop: Arc<AtomicBool>,
    shutdown_requests: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl DecoyServer {
    fn start(listener: TcpListener, status: u16) -> std::io::Result<Self> {
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown_requests = Arc::new(AtomicUsize::new(0));
        let worker_stop = Arc::clone(&stop);
        let worker_requests = Arc::clone(&shutdown_requests);
        let thread = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                if line.starts_with("POST /control/shutdown ") {
                    worker_requests.fetch_add(1, Ordering::SeqCst);
                }
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        Ok(Self {
            stop,
            shutdown_requests,
            thread: Some(thread),
        })
    }
}

impl Drop for DecoyServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn localhost_migration_reaches_ipv6_without_shutting_down_an_ipv4_service()
-> Result<(), Box<dyn Error>> {
    check_localhost_migration(418, true)
}

#[test]
fn ambiguous_localhost_preserves_both_services() -> Result<(), Box<dyn Error>> {
    check_localhost_migration(200, false)
}

fn check_localhost_migration(decoy_status: u16, migrate: bool) -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let reservation = match TcpListener::bind("[::1]:0") {
        Ok(listener) => listener,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            eprintln!("IPv6 loopback is unavailable: {error}");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let listen = reservation.local_addr()?.to_string();
    let port = reservation.local_addr()?.port();
    let decoy = DecoyServer::start(
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?,
        decoy_status,
    )?;
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let mut guard = ServerGuard {
        binary,
        home: home.path().to_owned(),
        listen: listen.clone(),
        token_file: None,
        active: true,
    };
    drop(reservation);
    assert_command_success(&cli(&guard, &["--server", "on", "--listen", &listen]).output()?);
    let original = instance_record(home.path())?;
    let mut legacy = legacy_record(home.path())?;
    legacy["serverUrl"] = json!(format!("http://localhost:{port}/mcp"));
    write_record(home.path(), &legacy)?;
    let target = format!("127.0.0.1:{}", available_port()?);
    let output = cli(&guard, &["--server", "on", "--listen", &target]).output()?;
    let after = instance_record(home.path())?;
    // Restore the actual IPv6 address on failure so fixture cleanup remains safe.
    if !output.status.success() {
        write_record(home.path(), &original)?;
    }
    assert_eq!(decoy.shutdown_requests.load(Ordering::SeqCst), 0);
    if migrate {
        assert_command_success(&output);
        assert_ne!(after["pid"], original["pid"]);
        assert_eq!(after["daemonProtocolVersion"], 1);
        assert_healthy(&target)?;
    } else {
        assert_failure(&output, "cannot identify a unique healthy daemon");
        assert_eq!(after, legacy);
        assert_healthy(&listen)?;
    }
    assert_command_success(&guard.stop()?);
    Ok(())
}
