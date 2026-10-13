use ::terminal::pty::Session;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn wait_for(session: &Session, text: &str) {
    wait_for_with_diagnostics(session, text, String::new);
}

fn wait_for_with_diagnostics(session: &Session, text: &str, diagnostics: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let screen = session.parser.lock().unwrap().screen().contents();
        if screen.contains(text) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Missing {text:?}: {screen:?}\n{}",
            diagnostics()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn start(temp: &tempfile::TempDir) -> Session {
    start_with_env(temp, &[])
}

fn start_with_env(temp: &tempfile::TempDir, extra: &[std::ffi::OsString]) -> Session {
    let mut args: Vec<std::ffi::OsString> = vec![
        "/usr/bin/env".into(),
        "-u".into(),
        "LINEAR_CLIENT_ID".into(),
        "-u".into(),
        "NO_COLOR".into(),
        format!("XDG_CONFIG_HOME={}", temp.path().display()).into(),
        format!("XDG_STATE_HOME={}/state", temp.path().display()).into(),
        "SHELL=/bin/sh".into(),
    ];
    args.extend_from_slice(extra);
    args.extend([env!("CARGO_BIN_EXE_mentat").into(), "--shell".into()]);
    Session::spawn(&args, &temp.path().join("repo"), 100, 30).unwrap()
}

fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new("jj")
        .args(["git", "init", "--no-colocate"])
        .arg(temp.path().join("repo"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let dir = temp.path().join("mentat");
    fs::create_dir(&dir).unwrap();
    let config = dir.join("config.toml");
    fs::write(&config, format!("[workspace]\nkind = 'jj'\nroot = '{}'\n[linear]\nclient_id = 'test'\n[editor]\nexecutable = '/bin/sh'\nargs = ['-c', 'printf EDITOR-READY; read line']\n", temp.path().join("checkouts").display())).unwrap();
    fs::set_permissions(config, fs::Permissions::from_mode(0o600)).unwrap();
    let issues: Vec<_> = (1..=2).map(|id| serde_json::json!({
        "id": format!("ticket-{id}"), "identifier": format!("TEST-{id}"), "title": format!("Ticket {id}"),
        "description": "Body", "url": format!("https://linear.app/test/issue/TEST-{id}"), "state": {"name":"Started"}, "team":{"name":"Test"}
    })).collect();
    let snapshot: linear::Snapshot =
        serde_json::from_value(serde_json::json!({"viewer":"Test", "issues":issues})).unwrap();
    let state = temp.path().join("state/mentat");
    fs::create_dir_all(&state).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(state.join("mentat.db"))
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let store = linear::Store::for_repository(
            pool.clone(),
            temp.path()
                .join("repo")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        store
            .bind_workspace(
                &linear::Workspace {
                    id: "test-workspace".into(),
                    name: "Test".into(),
                },
                true,
            )
            .await
            .unwrap();
        store
            .save_cache(
                &linear::Config {
                    client_id: "test".into(),
                    api_key: None,
                },
                &snapshot,
            )
            .await
            .unwrap();
        pool.close().await;
    });
    temp
}

#[test]
fn startup_failures_write_private_structured_logs_without_config_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("mentat");
    fs::create_dir(&config).unwrap();
    let path = config.join("config.toml");
    fs::write(&path, "[linear]\napi_key = 'secret-config-value").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mentat"))
        .env("XDG_CONFIG_HOME", temp.path())
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MENTAT_LOG", "debug")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let logs = temp.path().join("state/mentat/logs");
    assert_eq!(
        fs::metadata(&logs).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let path = fs::read_dir(logs).unwrap().next().unwrap().unwrap().path();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("Mentat starting"));
    assert!(text.contains("Operation failed"));
    assert!(text.contains("Mentat stopped"));
    assert!(!text.contains("secret-config-value"));
    for line in text.lines() {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(event["timestamp"].is_string());
        assert!(event["span"]["pid"].is_number());
    }
}

#[test]
fn pane_navigation_focuses_overview_before_any_tool_is_open() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    let focus_color = host
        .parser
        .lock()
        .unwrap()
        .screen()
        .cell(1, 0)
        .unwrap()
        .fgcolor();
    assert_ne!(
        focus_color,
        host.parser
            .lock()
            .unwrap()
            .screen()
            .cell(1, 28)
            .unwrap()
            .fgcolor()
    );
    let wait_focus = |column, stage| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if host
                .parser
                .lock()
                .unwrap()
                .screen()
                .cell(1, column)
                .unwrap()
                .fgcolor()
                == focus_color
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Pane at column {column} did not receive focus: {stage}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    wait_focus(0, "startup");
    host.write(b"\x0c").unwrap(); // Ctrl-L, before any ticket session exists.
    wait_focus(28, "ctrl-l");
    host.write(b"\x08").unwrap(); // Legacy Ctrl-H; plain Backspace is 0x7f.
    wait_focus(0, "ctrl-h");
    host.write(b"\x0c").unwrap();
    wait_focus(28, "second ctrl-l");
    host.write(b"\x07").unwrap(); // Configured return shortcut also leaves overview.
    wait_focus(0, "ctrl-g");
    quit(&mut host, false);
}

#[test]
fn workspace_picker_opens_from_manager_and_escape_returns_to_tickets() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    host.write(b"l").unwrap();
    wait_for(&host, "Linear workspace");
    wait_for(&host, "Current: Test");
    wait_for(&host, "Sign in with Linear");
    host.write(b"\x1b").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while host
        .parser
        .lock()
        .unwrap()
        .screen()
        .contents()
        .contains("Linear workspace")
    {
        assert!(Instant::now() < deadline, "Workspace picker did not close");
        thread::sleep(Duration::from_millis(20));
    }
    host.write(b"?").unwrap();
    wait_for(&host, "Keyboard shortcuts");
    wait_for(&host, "Choose Linear workspace");
    host.write(b"?").unwrap();
    quit(&mut host, false);
}

#[test]
fn opens_selected_linear_issue_and_reports_opener_errors() {
    let temp = fixture();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let opener = bin.join(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    fs::write(
        &opener,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$OPENED_URL\"\n",
    )
    .unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o700)).unwrap();
    let capture = temp.path().join("opened-url");
    let mut host = start_with_env(
        &temp,
        &[
            format!("PATH={}:{}", bin.display(), std::env::var("PATH").unwrap()).into(),
            format!("OPENED_URL={}", capture.display()).into(),
        ],
    );
    wait_for(&host, "TEST-1");
    host.write(b"jo").unwrap();
    wait_for(&host, "Opened issue in Linear.");
    assert_eq!(
        fs::read_to_string(&capture).unwrap(),
        "https://linear.app/test/issue/TEST-2\n"
    );
    fs::write(&opener, "#!/bin/sh\nexit 1\n").unwrap();
    host.write(b"o").unwrap();
    wait_for(&host, "Could not open Linear.");
    quit(&mut host, false);
}

#[test]
fn tool_shortcuts_open_interactive_sessions() {
    for key in [b"a", b"\r", b"e", b"t"] {
        let temp = fixture();
        let mut host = start(&temp);
        wait_for(&host, "TEST-1");
        host.write(key).unwrap();
        if key == b"e" {
            wait_for(&host, "EDITOR-READY");
        } else {
            host.write(b"printf 'TOOL-%s\\n' READY\r").unwrap();
            wait_for(&host, "TOOL-READY");
        }
        quit(&mut host, true);
    }
}

#[test]
#[ignore = "requires local Neovim"]
fn neovim_receives_pane_keys_and_mouse_clicks() {
    let temp = fixture();
    let script = temp.path().join("editor.vim");
    let focused = temp.path().join("focused");
    fs::write(&script, format!(
        "set mouse=a\nset noswapfile\nfile LEFT-PANE\ncall setline(1, 'LEFT-PANE')\nvsplit\nwincmd l\nenew\nfile RIGHT-PANE\ncall setline(1, 'RIGHT-PANE')\nnnoremap <C-h> <C-w>h\nnnoremap <C-l> <C-w>l\nautocmd WinEnter * call writefile([bufname('%')], '{}')\n",
        focused.display(),
    )).unwrap();
    let config = temp.path().join("mentat/config.toml");
    let text = fs::read_to_string(&config).unwrap().replace(
        "executable = '/bin/sh'\nargs = ['-c', 'printf EDITOR-READY; read line']",
        &format!(
            "executable = 'nvim'\nargs = ['--clean', '-n', '-i', 'NONE', '-S', '{}']",
            script.display()
        ),
    );
    fs::write(config, text).unwrap();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    host.write(b"e").unwrap();
    wait_for(&host, "RIGHT-PANE");
    let expect_focus = |name: &str| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if fs::read_to_string(&focused).unwrap_or_default().trim() == name {
                return;
            }
            assert!(Instant::now() < deadline, "Neovim did not focus {name}");
            thread::sleep(Duration::from_millis(20));
        }
    };
    host.write(b"\x08").unwrap();
    expect_focus("LEFT-PANE");
    host.write(b"\x0c").unwrap();
    expect_focus("RIGHT-PANE");
    // Host positions include the 28-column sidebar and one-row header.
    host.write(b"\x1b[<0;31;3M\x1b[<0;31;3m").unwrap();
    expect_focus("LEFT-PANE");
    host.write(b"\x1b[<0;80;3M\x1b[<0;80;3m").unwrap();
    expect_focus("RIGHT-PANE");
    // Enhanced keyboard reports must work as well as legacy control bytes.
    host.write(b"\x1b[104;5u").unwrap();
    expect_focus("LEFT-PANE");
    host.write(b"\x1b[108;5u").unwrap();
    expect_focus("RIGHT-PANE");
    // Full-screen tools have neither a sidebar nor a header offset.
    host.write(b"\x07b\x1b[<0;5;2M\x1b[<0;5;2m").unwrap();
    expect_focus("LEFT-PANE");
    quit(&mut host, true);
}

#[test]
fn dragging_output_copies_only_the_main_pane_and_preserves_paste() {
    use base64::Engine as _;
    use std::{
        io::{Read, Write},
        sync::{Arc, Mutex},
    };

    // Retain the raw host output so the clipboard command can be checked without
    // touching the developer's clipboard or requiring a GUI terminal in CI.
    struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let temp = fixture();
    let capture = temp.path().join("agent-input");
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_mentat"));
    command.cwd(temp.path().join("repo"));
    command.env("XDG_CONFIG_HOME", temp.path());
    command.env("XDG_STATE_HOME", temp.path().join("state"));
    command.env_remove("LINEAR_CLIENT_ID");
    command.env("TERM", "xterm-256color");
    command.env("CAPTURE", &capture);
    command.args(["--", "/bin/sh", "-c", "stty raw -echo; printf '\\033[?1002h\\033[?1006h\\033[?2004h\\033[2J\\033[Halpha\\r\\nbeta'; cat >> \"$CAPTURE\""]);
    let mut child = ChildGuard(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    let mut writer = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let collected = output.clone();
    let thread = thread::spawn(move || {
        let mut buf = [0; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            collected.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let wait = |predicate: &dyn Fn(&[u8]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if predicate(&output.lock().unwrap()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Expected terminal or clipboard output did not arrive"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let screen_contains = |bytes: &[u8], text: &str| {
        let mut parser =
            ::terminal::pty::Emulator::new_with_callbacks(30, 100, 0, Default::default());
        parser.process(bytes);
        parser.screen().contents().contains(text)
    };
    wait(&|bytes| screen_contains(bytes, "TEST-1"));
    writer.write_all(b"\r").unwrap();
    wait(&|bytes| screen_contains(bytes, "beta"));
    writer.write_all(b"\x07").unwrap();
    // Drag from 'l' in alpha to 't' in beta, accounting for header and sidebar.
    writer
        .write_all(b"\x1b[<0;30;2M\x1b[<32;31;3M\x1b[<0;31;3m")
        .unwrap();
    let clipboard = format!(
        "\x1b]52;c;{}\x1b\\",
        base64::engine::general_purpose::STANDARD.encode("lpha\nbet")
    );
    wait(&|bytes| {
        bytes
            .windows(clipboard.len())
            .any(|part| part == clipboard.as_bytes())
    });
    assert!(
        fs::read(&capture).unwrap_or_default().is_empty(),
        "Selection leaked into the agent"
    );
    // A click alone and a drag starting in the header must not copy anything.
    writer
        .write_all(b"\x1b[<0;29;2M\x1b[<0;29;2m\x1b[<0;29;1M\x1b[<32;31;3M\x1b[<0;31;3m")
        .unwrap();
    // A drag ending over the sidebar is clamped to the main pane.
    writer
        .write_all(b"\x1b[<0;29;2M\x1b[<32;1;3M\x1b[<0;1;3m")
        .unwrap();
    let clipped = format!(
        "\x1b]52;c;{}\x1b\\",
        base64::engine::general_purpose::STANDARD.encode("alpha\nb")
    );
    wait(&|bytes| {
        bytes
            .windows(clipped.len())
            .any(|part| part == clipped.as_bytes())
    });
    assert_eq!(
        output
            .lock()
            .unwrap()
            .windows(7)
            .filter(|part| *part == b"\x1b]52;c;")
            .count(),
        2
    );
    // Refocus the tool and paste using the terminal's normal bracketed paste.
    writer.write_all(b"\r\x1b[200~lpha\nbet\x1b[201~").unwrap();
    wait(&|_| fs::read(&capture).unwrap_or_default() == b"\x1b[200~lpha\nbet\x1b[201~");
    writer.write_all(b"\x07q").unwrap();
    wait(&|bytes| screen_contains(bytes, "Force quit"));
    writer.write_all(b"f").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "Mentat did not exit");
        thread::sleep(Duration::from_millis(20));
    }
    drop(writer);
    drop(pair.master);
    thread.join().unwrap();
}

#[test]
fn sidebar_focus_keeps_tools_visible_and_shortcuts_resume_them() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    let sidebar_color = host
        .parser
        .lock()
        .unwrap()
        .screen()
        .cell(1, 0)
        .unwrap()
        .fgcolor();
    let focus_sidebar = |host: &Session, visible: &str| {
        host.write(b"\x07").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let parser = host.parser.lock().unwrap();
            let screen = parser.screen();
            if screen.cell(1, 0).unwrap().fgcolor() == sidebar_color && screen.hide_cursor() {
                assert!(
                    screen.contents().contains(visible),
                    "Tool disappeared while focusing sidebar"
                );
                break;
            }
            assert!(Instant::now() < deadline, "Sidebar did not gain focus");
            drop(parser);
            thread::sleep(Duration::from_millis(20));
        }
    };
    host.write(b"a").unwrap();
    host.write(b"printf 'AGENT-%s\\n' VISIBLE\r").unwrap();
    wait_for(&host, "AGENT-VISIBLE");
    focus_sidebar(&host, "AGENT-VISIBLE");
    host.write(b"e").unwrap();
    wait_for(&host, "EDITOR-READY");
    focus_sidebar(&host, "EDITOR-READY");
    host.write(b"t").unwrap();
    host.write(b"printf 'SHELL-%s\\n' VISIBLE\r").unwrap();
    wait_for(&host, "SHELL-VISIBLE");
    focus_sidebar(&host, "SHELL-VISIBLE");
    // Background output must still redraw while the sidebar has keyboard focus.
    let marker = temp.path().join("emit-output");
    host.write(b"t").unwrap();
    host.write(
        format!(
            "while [ ! -f '{}' ]; do sleep 0.02; done; printf 'BACKGROUND-%s\\n' VISIBLE\r",
            marker.display()
        )
        .as_bytes(),
    )
    .unwrap();
    focus_sidebar(&host, "SHELL-VISIBLE");
    fs::write(marker, "go").unwrap();
    wait_for(&host, "BACKGROUND-VISIBLE");
    host.write(b"a").unwrap();
    wait_for(&host, "AGENT-VISIBLE");
    host.write(b"printf 'RESUMED-%s\\n' AGENT\r").unwrap();
    wait_for(&host, "RESUMED-AGENT");
    quit(&mut host, true);
}

#[test]
fn hiding_sidebar_gives_the_live_terminal_the_whole_screen() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    host.write(b"t").unwrap();
    host.write(b"printf 'BEFORE='; stty size\r").unwrap();
    wait_for(&host, "BEFORE=29 72");
    host.write(b"\x07b").unwrap();
    host.write(b"printf 'FULL='; stty size\r").unwrap();
    wait_for(&host, "FULL=30 100");
    assert!(
        !host
            .parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("Workspace")
    );
    host.write(b"\x07t").unwrap();
    host.write(b"printf 'RESTORED='; stty size\r").unwrap();
    wait_for(&host, "RESTORED=29 72");
    wait_for(&host, "Workspace");
    quit(&mut host, true);
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
fn agent_startup_animates_and_keeps_navigation_responsive() {
    for fail in [false, true] {
        let temp = fixture();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let agent = bin.join("codex");
        fs::write(
            &agent,
            r#"#!/bin/sh
exec 2>> "$TRACE"
printf 'started %s\n' "$1" >> "$TRACE"
if [ "$1" = app-server ]; then
    while IFS= read -r request; do
        printf '%s\n' "$request" >> "$TRACE"
        id=$(printf '%s\n' "$request" | sed -n 's/.*"id": *\([0-9][0-9]*\).*/\1/p')
        [ -n "$id" ] || continue
        case "$request" in
            *'"method":"initialize"'*)
                printf 'waiting for release\n' >> "$TRACE"
                while [ ! -f "$RELEASE" ]; do sleep 0.02; done
                if [ "$FAIL_START" = 1 ]; then
                    printf '{"id":%s,"error":{"message":"Fixture startup failed"}}\n' "$id"
                    continue
                fi
                ;;
        esac
        printf '{"id":%s,"result":{"thread":{"id":"01a11c20-c173-77f3-8512-3bee052093bc"}}}\n' "$id"
        printf 'replied %s\n' "$id" >> "$TRACE"
    done
    printf 'helper closed\n' >> "$TRACE"
else
    printf 'AGENT-READY\n'
    printf 'interactive ready\n' >> "$TRACE"
    cat > /dev/null
fi
"#,
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
        let release = temp.path().join("release");
        let trace = temp.path().join("agent-trace");
        let diagnostics = || {
            format!(
                "Agent trace:\n{}\nMentat logs:\n{}",
                fs::read_to_string(&trace).unwrap_or_default(),
                fs::read_dir(temp.path().join("state/mentat/logs"))
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter_map(|entry| fs::read_to_string(entry.path()).ok())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let wait = |host: &Session, text| wait_for_with_diagnostics(host, text, diagnostics);
        let args = [
            "/usr/bin/env".into(),
            "-u".into(),
            "LINEAR_CLIENT_ID".into(),
            format!("XDG_CONFIG_HOME={}", temp.path().display()).into(),
            format!("XDG_STATE_HOME={}/state", temp.path().display()).into(),
            format!("PATH={}:{}", bin.display(), std::env::var("PATH").unwrap()).into(),
            "MENTAT_LOG=debug".into(),
            format!("TRACE={}", trace.display()).into(),
            format!("RELEASE={}", release.display()).into(),
            format!("FAIL_START={}", u8::from(fail)).into(),
            env!("CARGO_BIN_EXE_mentat").into(),
            "--codex".into(),
        ];
        let mut host = Session::spawn(&args, &temp.path().join("repo"), 100, 30).unwrap();
        wait(&host, "TEST-1");
        host.write(b"\r").unwrap();
        wait(&host, "Starting codex");
        // Observe the helper at the release gate before testing UI animation.
        let deadline = Instant::now() + Duration::from_secs(8);
        while !fs::read_to_string(&trace)
            .unwrap_or_default()
            .contains("waiting for release")
        {
            assert!(
                Instant::now() < deadline,
                "Helper never initialized: {}",
                diagnostics()
            );
            thread::sleep(Duration::from_millis(20));
        }
        let first = host.parser.lock().unwrap().screen().contents();
        assert!(
            first.lines().nth(1).unwrap().contains("Starting codex"),
            "Startup indicator should be in the panel header"
        );
        assert!(
            first.contains("Body"),
            "Ticket overview should remain visible during startup"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while host.parser.lock().unwrap().screen().contents() == first {
            assert!(Instant::now() < deadline, "Spinner did not animate");
            thread::sleep(Duration::from_millis(20));
        }
        host.write(b"\x07?").unwrap();
        wait(&host, "Keyboard shortcuts");
        host.write(b"?\x0c").unwrap();
        wait(&host, "Starting codex");
        fs::write(release, "ready").unwrap();
        wait(
            &host,
            if fail {
                "Agent startup failed"
            } else {
                "AGENT-READY"
            },
        );
        assert!(
            !host
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("Starting codex")
        );
        quit(&mut host, false);
    }
}

#[test]
fn ticket_paste_only_seeds_new_agent_conversations() {
    let temp = fixture();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let agent = bin.join("claude");
    fs::write(
        &agent,
        "#!/bin/sh\nstty raw -echo\nprintf '\\033[?2004hAGENT-READY'\ncat >> \"$CAPTURE\"\n",
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    let capture = temp.path().join("input");
    let start_agent = |extra: &[&str]| {
        let mut args: Vec<std::ffi::OsString> = vec![
            "/usr/bin/env".into(),
            "-u".into(),
            "LINEAR_CLIENT_ID".into(),
            format!("XDG_CONFIG_HOME={}", temp.path().display()).into(),
            format!("XDG_STATE_HOME={}/state", temp.path().display()).into(),
            format!("PATH={}:{}", bin.display(), std::env::var("PATH").unwrap()).into(),
            format!("CAPTURE={}", capture.display()).into(),
            env!("CARGO_BIN_EXE_mentat").into(),
            "--claude".into(),
        ];
        args.extend(extra.iter().map(std::ffi::OsString::from));
        let host = Session::spawn(&args, &temp.path().join("repo"), 100, 30).unwrap();
        wait_for(&host, "TEST-1");
        host
    };
    let expect_input = |expected: &[u8]| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let actual = fs::read(&capture).unwrap_or_default();
            if actual == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected:?}, got {actual:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let paste = b"\x1b[200~Ticket 1\n\nBody\x1b[201~";
    let mut expected = paste.to_vec();
    let mut host = start_agent(&[]);
    host.write(b"a").unwrap();
    wait_for(&host, "AGENT-READY");
    expect_input(&expected);
    host.write(b"\x07aLIVE").unwrap();
    expected.extend_from_slice(b"LIVE");
    expect_input(&expected);
    // Editing keys must reach the agent without changing focus or losing Shift.
    host.write(b"\x7f\x1b[127u\x1b[13;2uAFTER\r").unwrap();
    expected.extend_from_slice(b"\x7f\x7f\x1b[13;2uAFTER\r");
    expect_input(&expected);
    // A focused PTY owns shortcuts, including Mentat's pane and manager keys.
    host.write(b"\x08\x0a\x0b\x0c\x1b[104;5u\x1b[108;5uqbes?\x03")
        .unwrap();
    expected.extend_from_slice(b"\x08\x0a\x0b\x0c\x08\x0cqbes?\x03");
    host.write(b"\x09\x1b[105;5u").unwrap();
    expected.extend_from_slice(b"\x09\x09");
    expect_input(&expected);
    quit(&mut host, false);

    // Restoring a saved conversation must not insert the ticket again.
    let mut host = start_agent(&[]);
    host.write(b"a").unwrap();
    wait_for(&host, "AGENT-READY");
    host.write(b"RESTORED").unwrap();
    expected.extend_from_slice(b"RESTORED");
    expect_input(&expected);
    quit(&mut host, false);

    let mut host = start_agent(&["--resume", "11111111-1111-4111-8111-111111111111"]);
    host.write(b"a").unwrap();
    wait_for(&host, "AGENT-READY");
    host.write(b"EXPLICIT").unwrap();
    expected.extend_from_slice(b"EXPLICIT");
    expect_input(&expected);
    quit(&mut host, false);

    let mut host = start_agent(&["--new-conversation"]);
    host.write(b"a").unwrap();
    wait_for(&host, "AGENT-READY");
    expected.extend_from_slice(paste);
    expect_input(&expected);
    quit(&mut host, false);
}

#[test]
fn ticket_tools_switch_and_restart_without_children() {
    let temp = fixture();
    let mut host = start(&temp);
    wait_for(&host, "TEST-1");
    host.write(b"a").unwrap();
    host.write(b"printf 'FIRST-%s\\n' READY\r").unwrap();
    wait_for(&host, "FIRST-READY");
    host.write(b"\x07e").unwrap();
    wait_for(&host, "EDITOR-READY");
    host.write(b"\x07j\r").unwrap();
    host.write(b"printf 'SECOND-%s\\n' READY\r").unwrap();
    wait_for(&host, "SECOND-READY");
    quit(&mut host, true);
    let mut restored = start(&temp);
    wait_for(&restored, "Ticket overview");
    wait_for(&restored, "https://linear.app/test/issue/TEST-1");
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
    // Leave the cursor hidden so shutdown exercises cursor restoration after
    // the host has closed both stdout and stderr.
    command.args([
        "--",
        "/bin/sh",
        "-c",
        &format!(
            "echo $$ > '{}'; trap '' HUP TERM; printf '\\033[?25lCHILD-READY'; read line",
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
            writer.write_all(b"a").unwrap();
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
    assert!(
        status.success(),
        "Mentat exited unsuccessfully: {status:?}\nLogs:\n{}",
        fs::read_dir(temp.path().join("state/mentat/logs"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    );
    // SAFETY: signal zero checks the child that this test created.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "Child survived terminal closure"
    );
}
