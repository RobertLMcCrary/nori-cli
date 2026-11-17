//! Verify the real terminal stream, including the OSC terminator.
#[cfg(unix)]
#[test]
fn reports_permission_work_completion_and_exit() {
    use std::time::Duration;
    use tui_pty_e2e::Key;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    let config = SessionConfig::new()
        .with_agent("mock-model".to_owned())
        .with_agent_env("NORI_TEST_REQUIRE_APPROVAL", "1")
        .with_agent_env("MOCK_AGENT_REQUEST_PERMISSION", "1");
    let mut session = TuiSession::spawn_with_config(24, 80, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();
    session.submit_input("Run a command").unwrap();
    session
        .wait_for_text("Would you like to allow", TIMEOUT)
        .unwrap();
    session.send_key(Key::Char('y')).unwrap();
    session
        .wait_for_text("Permission granted", TIMEOUT)
        .unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !session
        .output_bytes()
        .unwrap()
        .windows(b"\x1b]7501;state=done:app=nori\x1b\\".len())
        .any(|part| part == b"\x1b]7501;state=done:app=nori\x1b\\")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "missing completed status"
        );
        session.poll().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    session.submit_input("/quit").unwrap();
    assert!(session.wait_for_process_exit(Duration::from_secs(10)));
    let _ = session.poll();
    let output = String::from_utf8_lossy(session.output_bytes().unwrap());
    for sequence in [
        "\x1b]7501;state=working:app=nori\x1b\\",
        "\x1b]7501;state=blocked:app=nori:kind=permission",
        "\x1b]7501;state=done:app=nori\x1b\\",
        "\x1b]7501;state=clear\x1b\\",
    ] {
        assert!(
            output.contains(sequence),
            "missing {sequence:?} in PTY output"
        );
    }
}

#[cfg(unix)]
#[test]
fn reports_nested_children_independently_from_the_root() {
    use std::time::Duration;
    use tui_pty_e2e::Key;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/program_status_nested.jsonl"
    );
    let config = SessionConfig::new().with_agent_env("MOCK_AGENT_WIRE_REPLAY", fixture);
    let mut session = TuiSession::spawn_with_config(24, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();
    session.submit_input("Start nested work").unwrap();
    session
        .wait_for_text("Root work complete", TIMEOUT)
        .unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        session.poll().unwrap();
        let output = String::from_utf8_lossy(session.output_bytes().unwrap());
        if output.contains("\x1b]7501;state=done:app=nori\x1b\\") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing completed root status"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    session.send_key(Key::Char(' ')).unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        session.poll().unwrap();
        let output = String::from_utf8_lossy(session.output_bytes().unwrap());
        if let Some(done) = output.find("\x1b]7501;state=done:app=nori\x1b\\")
            && output[done..].contains("\x1b]7501;state=idle:app=nori\x1b\\")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing acknowledged idle status"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        session.poll().unwrap();
        let output = String::from_utf8_lossy(session.output_bytes().unwrap());
        if output.contains("\x1b]7501;state=clear:id=s1\x1b\\") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing closed child clear"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = String::from_utf8_lossy(session.output_bytes().unwrap());
    for expected in [
        "\x1b]7501;state=working:app=nori:id=s1:",
        "\x1b]7501;state=working:app=nori:id=s1/s2:",
        "\x1b]7501;state=done:app=nori\x1b\\",
        "\x1b]7501;state=idle:app=nori\x1b\\",
    ] {
        assert!(output.contains(expected), "missing {expected:?}");
    }
}

#[cfg(unix)]
#[test]
fn reports_and_clears_authentication_block_for_login() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tui_pty_e2e::Key;
    use tui_pty_e2e::SessionConfig;
    use tui_pty_e2e::TIMEOUT;
    use tui_pty_e2e::TIMEOUT_INPUT;
    use tui_pty_e2e::TuiSession;
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("claude");
    std::fs::write(
        &cli,
        "#!/bin/sh\nprintf 'AUTH_FIXTURE_READY\\n'\nread answer\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = SessionConfig::new().with_extra_path(dir.path().to_owned());
    let mut session = TuiSession::spawn_with_config(24, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();
    session.submit_input("/login claude-code").unwrap();
    session
        .wait_for_text("AUTH_FIXTURE_READY", TIMEOUT)
        .unwrap();
    session.send_key(Key::Escape).unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        session.poll().unwrap();
        let output = String::from_utf8_lossy(session.output_bytes().unwrap());
        if let Some(blocked) = output.find("\x1b]7501;state=blocked:app=nori:kind=auth")
            && output[blocked..].contains("\x1b]7501;state=idle:app=nori\x1b\\")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "authentication status did not clear: {output:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
