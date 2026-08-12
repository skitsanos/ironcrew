//! Process-level attended-terminal regressions for `crew:ask_human()`.
//!
//! These tests use a real controlling pseudo-terminal. Ordinary integration
//! test stdin is a pipe, which only exercises the unattended fallback path.

#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

/// `portable-pty` launches through `forkpty` on macOS. Starting several of
/// those children concurrently from Rust's multithreaded test harness is
/// inherently unsafe and can leave a child stalled before `exec`. Keep the
/// real-process PTY cases serial while leaving the rest of the test suite
/// parallel.
fn pty_process_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct PtyRun {
    _project: tempfile::TempDir,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    // Keep the master alive until the child and its cloned reader are done.
    _master: Box<dyn MasterPty + Send>,
}

impl PtyRun {
    fn spawn(script: &str, max_answer_bytes: Option<usize>) -> Self {
        Self::spawn_with_mode(script, max_answer_bytes, false)
    }

    fn spawn_noncanonical(script: &str, max_answer_bytes: Option<usize>) -> Self {
        Self::spawn_with_mode(script, max_answer_bytes, true)
    }

    fn spawn_with_mode(script: &str, max_answer_bytes: Option<usize>, noncanonical: bool) -> Self {
        let project = tempfile::tempdir().expect("create flow directory");
        let script = format!(
            r#"
            local crew = Crew.new({{
                goal = "PTY human-input regression",
                provider = "openai",
                model = "test",
                api_key = "test",
            }})
            {script}
            "#
        );
        std::fs::write(project.path().join("crew.lua"), script).expect("write flow");

        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pseudo-terminal");

        let mut command = if noncanonical {
            let mut command = CommandBuilder::new("/bin/sh");
            command.arg("-c");
            command.arg(
                "stty -icanon min 1 time 0; exec \"$IRONCREW_TEST_BIN\" run \"$IRONCREW_TEST_PROJECT\"",
            );
            command.env("IRONCREW_TEST_BIN", env!("CARGO_BIN_EXE_ironcrew"));
            command.env("IRONCREW_TEST_PROJECT", project.path());
            command
        } else {
            let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ironcrew"));
            command.arg("run");
            command.arg(project.path());
            command
        };
        command.cwd(project.path());
        command.env("IRONCREW_STORE", "json");
        if let Some(limit) = max_answer_bytes {
            command.env("IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES", limit.to_string());
        }

        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn ironcrew under pseudo-terminal");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let (output_tx, output_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if output_tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    // PTY masters commonly report EIO, rather than EOF, after
                    // the slave closes. Both mean output collection is done.
                    Err(_) => break,
                }
            }
        });

        Self {
            _project: project,
            child,
            writer,
            output_rx,
            output: Vec::new(),
            _master: pair.master,
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn collect_available(&mut self) {
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.output.extend(chunk);
        }
    }

    fn wait_for_text(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.collect_available();
            if self.output().contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll child status") {
                panic!(
                    "ironcrew exited with {status:?} before {needle:?}; output:\n{}",
                    self.output()
                );
            }

            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {needle:?}; output:\n{}",
                self.output()
            );
            let wait = (deadline - now).min(Duration::from_millis(100));
            match self.output_rx.recv_timeout(wait) {
                Ok(chunk) => self.output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "PTY output closed before {needle:?}; output:\n{}",
                        self.output()
                    );
                }
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write PTY input");
        self.writer.flush().expect("flush PTY input");
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            self.collect_available();
            if let Some(status) = self.child.try_wait().expect("poll child status") {
                // Give the reader a moment to forward the child's final line.
                if let Ok(chunk) = self.output_rx.recv_timeout(Duration::from_millis(100)) {
                    self.output.extend(chunk);
                }
                self.collect_available();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "ironcrew did not exit after terminal input; output:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for PtyRun {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
    }
}

#[test]
fn timed_out_reader_cannot_steal_the_next_crlf_answer_or_hold_shutdown() {
    let _process_guard = pty_process_guard();
    let mut run = PtyRun::spawn(
        r#"
        local first = crew:ask_human({
            prompt = "FIRST_TIMEOUT_PROMPT",
            timeout_s = 1,
            default = "first-timed-out",
        })
        local second = crew:ask_human({
            prompt = "SECOND_ANSWER_PROMPT",
            timeout_s = 10,
        })
        print("HITL_RESULT=" .. first .. ":" .. second)
        "#,
        None,
    );

    run.wait_for_text("SECOND_ANSWER_PROMPT", Duration::from_secs(10));
    run.send(b"second-answer\r\n");
    run.wait_for_text(
        "HITL_RESULT=first-timed-out:second-answer",
        Duration::from_secs(3),
    );
    let status = run.wait_for_exit(Duration::from_secs(3));
    assert!(status.success(), "output:\n{}", run.output());
}

#[test]
fn terminal_eof_resolves_immediately_without_waiting_for_question_timeout() {
    let _process_guard = pty_process_guard();
    let mut run = PtyRun::spawn(
        r#"
        crew:ask_human({ prompt = "EOF_PROMPT", timeout_s = 30 })
        "#,
        None,
    );

    run.wait_for_text("EOF_PROMPT", Duration::from_secs(10));
    run.send(&[0x04]); // VEOF (Ctrl-D) on an empty canonical input line.
    let status = run.wait_for_exit(Duration::from_secs(3));
    assert!(!status.success(), "flow unexpectedly succeeded");
    assert!(
        run.output().contains("timed out"),
        "output:\n{}",
        run.output()
    );
}

#[test]
fn invalid_utf8_terminal_input_is_an_error_and_does_not_hang_shutdown() {
    let _process_guard = pty_process_guard();
    let mut run = PtyRun::spawn(
        r#"
        crew:ask_human({ prompt = "INVALID_UTF8_PROMPT", timeout_s = 30 })
        "#,
        None,
    );

    run.wait_for_text("INVALID_UTF8_PROMPT", Duration::from_secs(10));
    run.send(&[0xff, b'\r']);
    let status = run.wait_for_exit(Duration::from_secs(3));
    assert!(!status.success(), "flow unexpectedly succeeded");
    assert!(
        run.output().contains("controlling terminal read failed")
            && run.output().contains("invalid utf-8"),
        "output:\n{}",
        run.output()
    );
}

#[test]
fn terminal_answer_is_bounded_before_over_limit_input_is_retained() {
    let _process_guard = pty_process_guard();
    let mut run = PtyRun::spawn(
        r#"
        crew:ask_human({ prompt = "OVERSIZED_PROMPT", timeout_s = 30 })
        "#,
        Some(8),
    );

    run.wait_for_text("OVERSIZED_PROMPT", Duration::from_secs(10));
    run.send(b"123456789\r");
    let status = run.wait_for_exit(Duration::from_secs(3));
    assert!(!status.success(), "flow unexpectedly succeeded");
    assert!(
        run.output()
            .contains("terminal answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES (8)"),
        "output:\n{}",
        run.output()
    );
}

#[test]
fn oversized_terminal_line_is_drained_before_the_next_prompt_reads() {
    let _process_guard = pty_process_guard();
    let mut run = PtyRun::spawn(
        r#"
        local first_ok = pcall(function()
            return crew:ask_human({
                prompt = "OVERSIZED_THEN_CONTINUE_PROMPT",
                timeout_s = 30,
            })
        end)
        print("FIRST_OVERSIZED_REJECTED=" .. tostring(not first_ok))
        local second = crew:ask_human({
            prompt = "SECOND_AFTER_OVERSIZE_PROMPT",
            timeout_s = 30,
        })
        print("SECOND_AFTER_OVERSIZE_RESULT=" .. second)
        "#,
        Some(8),
    );

    run.wait_for_text("OVERSIZED_THEN_CONTINUE_PROMPT", Duration::from_secs(10));
    // Queue a complete oversized line and the next prompt's legitimate
    // answer together. The first reader must consume through its own newline,
    // while leaving the following line untouched for the serialized reader.
    run.send(b"123456789\rsecond\r");
    run.wait_for_text(
        "SECOND_AFTER_OVERSIZE_RESULT=second",
        Duration::from_secs(3),
    );

    let status = run.wait_for_exit(Duration::from_secs(3));
    assert!(status.success(), "output:\n{}", run.output());
    assert!(
        run.output().contains("FIRST_OVERSIZED_REJECTED=true"),
        "output:\n{}",
        run.output()
    );
}

#[test]
fn oversized_noncanonical_terminal_still_honors_the_question_deadline() {
    let _process_guard = pty_process_guard();
    let mut run = PtyRun::spawn_noncanonical(
        r#"
        crew:ask_human({
            prompt = "NONCANONICAL_OVERSIZED_PROMPT",
            timeout_s = 1,
        })
        "#,
        Some(8),
    );

    run.wait_for_text("NONCANONICAL_OVERSIZED_PROMPT", Duration::from_secs(10));
    // Noncanonical mode makes each byte readable without a line ending. Once
    // the ninth byte marks the answer oversized, the deadline must cancel and
    // flush the input instead of waiting forever for a newline.
    run.send(b"123456789");
    let status = run.wait_for_exit(Duration::from_secs(4));
    assert!(!status.success(), "flow unexpectedly succeeded");
    assert!(
        run.output()
            .contains("terminal answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES (8)"),
        "output:\n{}",
        run.output()
    );
}
