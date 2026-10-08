use ::terminal::pty::Session;
use std::{
    ffi::OsString,
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn spawn(args: &[&str]) -> Session {
    Session::spawn(
        &args.iter().map(OsString::from).collect::<Vec<_>>(),
        Path::new("/tmp"),
        80,
        24,
    )
    .unwrap()
}

fn wait_for(session: &Session, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = session.parser.lock().unwrap().screen().contents();
        if screen.contains(text) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Missing {text:?}; screen: {screen:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn drains_without_a_view_and_preserves_process_and_screen() {
    let mut session = spawn(&[
        "/bin/sh",
        "-c",
        "stty -echo; printf 'READY\\n'; read line; seq 1 20000; printf 'DRAINED\\n'; read line; stty size; read line",
    ]);
    wait_for(&session, "READY");
    let pid = session.pid;
    session.write(b"go\n").unwrap();
    wait_for(&session, "DRAINED");
    assert!(!session.poll_exit().unwrap());
    assert_eq!(session.pid, pid);
    let screen = session.parser.lock().unwrap().screen().clone();
    thread::sleep(Duration::from_millis(30));
    assert_eq!(
        screen.contents(),
        session.parser.lock().unwrap().screen().contents()
    );
    session.resize(100, 32).unwrap();
    session.write(b"size\n").unwrap();
    wait_for(&session, "32 100");
    session.shutdown().unwrap();
    assert!(session.poll_exit().unwrap());
}

#[test]
fn shutdown_escalates_for_a_stubborn_child() {
    let mut session = spawn(&[
        "/bin/sh",
        "-c",
        "trap '' HUP TERM; printf READY; while :; do sleep 1; done",
    ]);
    wait_for(&session, "READY");
    let start = Instant::now();
    session.shutdown().unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(session.poll_exit().unwrap());
    // SAFETY: signal zero only checks whether the reaped child still exists.
    assert_eq!(unsafe { libc::kill(session.pid as i32, 0) }, -1);
}

#[test]
#[ignore = "requires local Neovim"]
fn neovim_edits_resizes_and_exits_in_the_emulator() {
    check_vi_editor(&["nvim", "--clean", "-n", "-i", "NONE"], "NVIM");
}

#[test]
#[ignore = "requires local Vim"]
fn vim_edits_resizes_and_exits_in_the_emulator() {
    check_vi_editor(&["vim", "-u", "NONE", "-n", "-i", "NONE"], "VIM");
}

fn check_vi_editor(args: &[&str], banner: &str) {
    let mut session = spawn(args);
    wait_for(&session, banner);
    session.write(b"iMENTAT-EDITOR-CHECK\x1b").unwrap();
    wait_for(&session, "MENTAT-EDITOR-CHECK");
    assert!(session.parser.lock().unwrap().screen().alternate_screen());
    session.resize(100, 32).unwrap();
    session.write(b":q\r").unwrap();
    wait_for(&session, "E37");
    session.write(b"\x1b:q!\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.poll_exit().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn shutdown_stops_a_background_job_in_another_group() {
    let mut session = spawn(&[
        "/bin/sh",
        "-c",
        "set -m; (trap '' HUP TERM; exec sleep 300) & printf 'JOB=%s\\n' $!; read line",
    ]);
    wait_for(&session, "JOB=");
    let screen = session.parser.lock().unwrap().screen().contents();
    let pid: i32 = screen.split("JOB=").nth(1).unwrap().trim().parse().unwrap();
    session.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    // SAFETY: signal zero checks the background job owned by this test.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "Background job survived shutdown"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
