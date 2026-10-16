use std::{
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::replies::Replies;
use std::{ffi::OsString, path::Path};

pub type Emulator = vt100::Parser<Replies>;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct Session {
    pub parser: Arc<Mutex<Emulator>>,
    pub reader_error: Arc<Mutex<Option<String>>>,
    pub pid: u32,
    pub changed: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Writer,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    pub status: Option<portable_pty::ExitStatus>,
    closed: bool,
}

impl Session {
    pub fn spawn(args: &[OsString], cwd: &Path, cols: u16, rows: u16) -> Result<Self> {
        anyhow::ensure!(!args.is_empty(), "A PTY command is required");
        let pair = native_pty_system().openpty(size(cols, rows))?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let fd = pair
            .master
            .as_raw_fd()
            .context("PTY has no Unix file descriptor")?;
        let mut command = CommandBuilder::new(&args[0]);
        command.args(&args[1..]);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // The child speaks the emulated terminal protocol, not the host protocol.
        for name in [
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "TERMINFO",
            "TERMINFO_DIRS",
        ] {
            command.env_remove(name);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .context("Cannot start PTY child")?;
        drop(pair.slave);
        let pid = child.process_id().expect("Unix PTY child has a PID");
        let parser = Arc::new(Mutex::new(Emulator::new_with_callbacks(
            rows.max(1),
            cols.max(1),
            1000,
            Replies::default(),
        )));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_error = Arc::new(Mutex::new(None));
        let changed = Arc::new(AtomicBool::new(true));
        let mut session = Self {
            parser: parser.clone(),
            reader_error: reader_error.clone(),
            pid,
            changed: changed.clone(),
            master: pair.master,
            child,
            writer: writer.clone(),
            stop: stop.clone(),
            reader: None,
            status: None,
            closed: false,
        };

        session.reader = Some(thread::Builder::new().name("pty-output".into()).spawn(
            move || {
                let result = drain(fd, &mut reader, &writer, &parser, &stop, &changed);
                if let Err(error) = result {
                    *reader_error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(error.to_string());
                    changed.store(true, Ordering::Relaxed);
                }
            },
        )?);
        Ok(session)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        if !bytes.is_empty() {
            let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
            if parser.screen().scrollback() != 0 {
                parser.screen_mut().set_scrollback(0);
                self.changed.store(true, Ordering::Relaxed);
            }
        }
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(bytes)?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        // Serialize the grid resize with output parsing before SIGWINCH reaches the child.
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen_mut().set_size(rows.max(1), cols.max(1));
        self.master.resize(size(cols, rows))?;
        Ok(())
    }

    pub fn poll_exit(&mut self) -> Result<bool> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status.is_some())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        // portable-pty calls setsid() before exec: the child PID is its own group.
        // A job-control shell can put its foreground child in a separate group.
        let mut groups = descendant_groups(self.pid);
        groups.push(self.pid as libc::pid_t);
        if let Some(foreground) = self.master.process_group_leader()
            && !groups.contains(&foreground)
        {
            groups.push(foreground);
        }
        for &group in &groups {
            signal_group(group, libc::SIGHUP);
        }
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            self.poll_exit()?;
            if groups.iter().all(|&group| !group_exists(group)) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        for &group in &groups {
            signal_group(group, libc::SIGKILL);
        }
        if self.status.is_none() {
            self.status = Some(self.child.wait()?);
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("PTY reader panicked"))?;
        }
        self.closed = true;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.max(1),
        rows: rows.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

// Job-control shells give background jobs separate process groups. Capture the
// owned process tree before SIGHUP can orphan those jobs. This runs only on exit.
fn descendant_groups(root: u32) -> Vec<libc::pid_t> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid="])
        .output()
    else {
        return Vec::new();
    };
    let rows: Vec<Vec<i32>> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            line.split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect()
        })
        .filter(|row: &Vec<i32>| row.len() == 3)
        .collect();
    let mut owned = vec![root as i32];
    let mut groups = Vec::new();
    // SAFETY: getpgrp reads the calling process's group.
    let host_group = unsafe { libc::getpgrp() };
    let mut cursor = 0;
    while cursor < owned.len() {
        for row in &rows {
            if row[1] == owned[cursor] && !owned.contains(&row[0]) {
                owned.push(row[0]);
                if row[2] > 1 && row[2] != host_group && !groups.contains(&row[2]) {
                    groups.push(row[2]);
                }
            }
        }
        cursor += 1;
    }
    groups
}

fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    if group > 1 {
        // SAFETY: negative PID targets only the process group created for this PTY.
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

fn group_exists(group: libc::pid_t) -> bool {
    // SAFETY: signal zero checks existence without delivering a signal.
    group > 1 && unsafe { libc::kill(-group, 0) == 0 }
}

fn drain(
    fd: libc::c_int,
    reader: &mut dyn Read,
    writer: &Writer,
    parser: &Mutex<Emulator>,
    stop: &AtomicBool,
    changed: &AtomicBool,
) -> io::Result<()> {
    let mut bytes = [0; 8192];
    while !stop.load(Ordering::Relaxed) {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: master stays alive until this reader joins; pollfd is one valid entry.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        match reader.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                let replies = {
                    let mut parser = parser.lock().unwrap_or_else(|e| e.into_inner());
                    parser.process(&bytes[..count]);
                    changed.store(true, Ordering::Relaxed);
                    std::mem::take(&mut parser.callbacks_mut().output)
                };
                if !replies.is_empty() {
                    writer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .write_all(&replies)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Linux reports EIO when the last slave descriptor closes.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
