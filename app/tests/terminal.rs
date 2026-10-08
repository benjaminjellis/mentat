use ::terminal::pty::Session;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn wait_for(session: &Session, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let screen = session.parser.lock().unwrap().screen().contents();
        if screen.contains(text) {
            return;
        }
        assert!(Instant::now() < deadline, "Missing {text:?}: {screen:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn start(temp: &tempfile::TempDir) -> Session {
    let args = [
        "/usr/bin/env".into(),
        "-u".into(),
        "LINEAR_CLIENT_ID".into(),
        format!("XDG_CONFIG_HOME={}", temp.path().display()).into(),
        format!("XDG_STATE_HOME={}/state", temp.path().display()).into(),
        "SHELL=/bin/sh".into(),
        env!("CARGO_BIN_EXE_mentat").into(),
        "--shell".into(),
    ];
    Session::spawn(&args, &temp.path().join("repo"), 100, 30).unwrap()
}

fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let status = Command::new("jj")
        .args(["git", "init", "--no-colocate"])
        .arg(temp.path().join("repo"))
        .output()
        .unwrap();
    assert!(status.status.success(), "{:?}", status);
    let dir = temp.path().join("mentat");
    fs::create_dir(&dir).unwrap();
    let config = dir.join("config.toml");
    fs::write(&config, "[linear]\nclient_id = 'test'\n[editor]\nexecutable = '/bin/sh'\nargs = ['-c', 'printf EDITOR-READY; read line']\n").unwrap();
    fs::set_permissions(config, fs::Permissions::from_mode(0o600)).unwrap();
    let issues: Vec<_> = (1..=2).map(|id| serde_json::json!({
        "id": format!("ticket-{id}"), "identifier": format!("TEST-{id}"), "title": format!("Ticket {id}"),
        "description": "Body", "url": "https://linear.app/test", "state": {"name":"Started"}, "team":{"name":"Test"}
    })).collect();
    fs::write(
        dir.join("linear-cache.json"),
        serde_json::to_vec(&serde_json::json!(["test", {"viewer":"Test", "issues":issues}]))
            .unwrap(),
    )
    .unwrap();
    temp
}

fn quit(host: &mut Session, force: bool) {
    host.write(b"\x07q").unwrap();
    if force {
        wait_for(host, "Force quit");
        host.write(b"f").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !host.poll_exit().unwrap() {
        assert!(Instant::now() < deadline, "Mentat did not exit");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(host.status.as_ref().unwrap().success());
    assert!(!host.parser.lock().unwrap().screen().alternate_screen());
}

#[test]
fn ticket_tools_use_workspaces_and_restart_without_children() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    // Startup creates the database, but no ticket workspace or editor.
    assert!(temp.path().join("state/mentat/mentat.db").exists());
    host.write(b"c").unwrap();
    thread::sleep(Duration::from_millis(200));
    host.write(b"pwd; printf 'CHILD=%s\\n' $$\r").unwrap();
    wait_for(&host, "mentat/workspaces/");
    host.write(b"\x07e").unwrap();
    wait_for(&host, "EDITOR-READY");
    host.write(b"\x07j\r").unwrap();
    thread::sleep(Duration::from_millis(200));
    host.write(b"printf 'SECOND\\n'\r").unwrap();
    wait_for(&host, "SECOND");
    quit(&mut host, true);
    let mut restored = start(&temp);
    wait_for(&restored, "Ticket 2");
    // No tools start on restart, so quit needs no force confirmation.
    quit(&mut restored, false);
}

#[test]
fn termination_signal_exits_and_restores_terminal() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    // SAFETY: this test owns the live process.
    assert_eq!(unsafe { libc::kill(host.pid as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !host.poll_exit().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(host.status.as_ref().unwrap().success());
    assert!(!host.parser.lock().unwrap().screen().alternate_screen());
}

#[test]
fn closing_the_host_terminal_stops_mentat_and_its_child() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;
    let temp = fixture();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_mentat"));
    let pidfile = temp.path().join("child.pid");
    command.args([
        "--",
        "/bin/sh",
        "-c",
        &format!(
            "echo $$ > '{}'; trap '' HUP TERM; printf CHILD-READY; read line",
            pidfile.display()
        ),
    ]);
    command.cwd(temp.path().join("repo"));
    command.env("XDG_CONFIG_HOME", temp.path());
    command.env("XDG_STATE_HOME", temp.path().join("state"));
    command.env_remove("LINEAR_CLIENT_ID");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut opened = false;
    while !String::from_utf8_lossy(&output).contains("CHILD-READY") {
        assert!(
            Instant::now() < deadline,
            "{:?}",
            String::from_utf8_lossy(&output)
        );
        let mut fd = libc::pollfd {
            fd: pair.master.as_raw_fd().unwrap(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the master descriptor and the single poll entry remain valid.
        if unsafe { libc::poll(&mut fd, 1, 50) } > 0 {
            let mut bytes = [0; 8192];
            let count = reader.read(&mut bytes).unwrap();
            assert_ne!(count, 0);
            output.extend_from_slice(&bytes[..count]);
        }
        if !opened && String::from_utf8_lossy(&output).contains("TEST-1") {
            writer.write_all(b"c").unwrap();
            opened = true;
        }
    }
    let pid: i32 = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    drop(reader);
    drop(writer);
    drop(pair.master);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Mentat survived terminal closure");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
    // SAFETY: signal zero checks the child that this test created.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "Child survived terminal closure"
    );
}
