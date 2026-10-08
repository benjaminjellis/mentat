use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use linear::Issue;
use terminal::{input, pty::Session};

pub struct PendingPaste {
    text: String,
    deadline: Instant,
}

impl PendingPaste {
    pub fn new(issue: &Issue) -> Self {
        Self {
            text: prompt(issue),
            deadline: Instant::now() + Duration::from_secs(15),
        }
    }

    /// Wait for bracketed paste so multiline text cannot act as Enter presses.
    pub fn poll(&self, session: &Session) -> Result<bool> {
        ensure!(
            Instant::now() < self.deadline,
            "The agent is not ready to accept the ticket. Press c to retry."
        );
        let ready = session
            .parser
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screen()
            .bracketed_paste();
        if !ready {
            return Ok(false);
        }
        session.write(&input::paste(&self.text, true))?;
        Ok(true)
    }
}

fn prompt(issue: &Issue) -> String {
    let title = clean(&issue.title);
    let body = clean(issue.description.as_deref().unwrap_or_default());
    if body.trim().is_empty() {
        title
    } else {
        format!("{title}\n\n{body}")
    }
}

fn clean(text: &str) -> String {
    // Remote ticket text must not close the paste envelope or inject control keys.
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, path::Path, thread};

    fn issue(body: Option<&str>) -> Issue {
        Issue {
            id: "id".into(),
            identifier: "ENG-1".into(),
            title: "Fix the bug".into(),
            description: body.map(str::to_owned),
            url: "url".into(),
            state: linear::Named {
                name: "Todo".into(),
            },
            team: linear::Named {
                name: "Team".into(),
            },
        }
    }

    #[test]
    fn formats_title_and_optional_body_without_control_keys() {
        assert_eq!(prompt(&issue(None)), "Fix the bug");
        assert_eq!(prompt(&issue(Some(" \n"))), "Fix the bug");
        assert_eq!(
            prompt(&issue(Some("First\r\nSecond\x1b[201~\x03"))),
            "Fix the bug\n\nFirst\nSecond[201~"
        );
    }

    #[test]
    fn waits_for_paste_mode_and_does_not_submit() {
        let mut child = Session::spawn(&["/bin/sh", "-c",
            "stty raw -echo; printf READY; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf '\\033[?2004h'; dd bs=1 count=29 2>/dev/null | od -An -tx1; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf SUBMITTED"]
            .map(OsString::from), Path::new("/tmp"), 120, 24).unwrap();
        let paste = PendingPaste::new(&issue(Some("Body")));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !child
            .parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("READY")
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!paste.poll(&child).unwrap());
        child.write(b"x").unwrap();
        while !paste.poll(&child).unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        loop {
            let screen = child.parser.lock().unwrap().screen().contents();
            if screen
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("32 30 31 7e")
            {
                break;
            }
            assert!(Instant::now() < deadline, "{screen}");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !child
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("SUBMITTED")
        );
        assert!(!child.poll_exit().unwrap());
        child.shutdown().unwrap();
    }
}
