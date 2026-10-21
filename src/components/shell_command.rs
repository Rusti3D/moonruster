// linux shell command execution utility (port of moonraker/components/shell_command.py)

use crate::common::ServerError;
use crate::confighelper::ConfigHelper;
use crate::server::Component;
use async_trait::async_trait;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::process::Stdio;
use std::rc::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type OutputCallback = Rc<dyn Fn(Vec<u8>)>;

#[derive(Debug, Clone)]
pub struct ShellCommandError {
    pub message: String,
    pub return_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status_code: i64,
}

impl From<ShellCommandError> for ServerError {
    fn from(e: ShellCommandError) -> Self {
        ServerError::new(e.message, e.status_code)
    }
}

impl std::fmt::Display for ShellCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// python's shlex.split (posix mode)
pub fn shlex_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_token = true;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    cur.push(c2);
                }
            }
            '"' => {
                in_token = true;
                while let Some(c2) = chars.next() {
                    match c2 {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&n) if n == '"' || n == '\\' || n == '$' || n == '`' || n == '\n' => {
                                cur.push(n);
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        other => cur.push(other),
                    }
                }
            }
            '\\' => {
                in_token = true;
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        cur.push(n);
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

pub struct ShellCommand {
    pub name: String,
    command: Vec<String>,
    std_out_cb: Option<OutputCallback>,
    std_err_cb: Option<OutputCallback>,
    env: Option<HashMap<String, String>>,
    log_stderr: bool,
    cwd: Option<String>,
    pid: Cell<Option<u32>>,
    cancelled: Cell<bool>,
    return_code: Cell<Option<i32>>,
    run_lock: tokio::sync::Mutex<()>,
    done_notify: RefCell<Option<Rc<tokio::sync::Notify>>>,
    finished: Cell<bool>,
}

fn send_signal(pid: u32, sig: i32) -> bool {
    unsafe { libc::kill(pid as i32, sig) == 0 }
}

impl ShellCommand {
    pub fn new(
        cmd: &str,
        std_out_cb: Option<OutputCallback>,
        std_err_cb: Option<OutputCallback>,
        env: Option<HashMap<String, String>>,
        log_stderr: bool,
        cwd: Option<String>,
    ) -> Rc<Self> {
        let expanded = crate::utils::expand_user(cmd).to_string_lossy().to_string();
        Rc::new(ShellCommand {
            name: cmd.to_string(),
            command: shlex_split(&expanded),
            std_out_cb,
            std_err_cb,
            env,
            log_stderr,
            cwd,
            pid: Cell::new(None),
            cancelled: Cell::new(false),
            return_code: Cell::new(None),
            run_lock: tokio::sync::Mutex::new(()),
            done_notify: RefCell::new(None),
            finished: Cell::new(false),
        })
    }

    pub fn get_return_code(&self) -> Option<i32> {
        self.return_code.get()
    }

    pub async fn cancel(&self, sig_idx: usize) {
        if self.cancelled.get() {
            return;
        }
        self.cancelled.set(true);
        let pid = match self.pid.get() {
            Some(p) => p,
            None => return,
        };
        let sigs = [(libc::SIGINT, "SIGINT"), (libc::SIGTERM, "SIGTERM"), (libc::SIGKILL, "SIGKILL")];
        let idx = sig_idx.min(2);
        let mut exit_success = false;
        for (sig, name) in &sigs[idx..] {
            if self.finished.get() {
                exit_success = true;
                break;
            }
            send_signal(pid, *sig);
            let notify = self.done_notify.borrow().clone();
            if let Some(n) = notify {
                if tokio::time::timeout(std::time::Duration::from_secs(2), n.notified()).await.is_err() && !self.finished.get() {
                    continue;
                }
            }
            log::debug!("Command '{}' exited with signal: {}", self.name, name);
            exit_success = true;
            break;
        }
        if !exit_success {
            log::info!("WARNING: {} did not cleanly exit", self.name);
        }
    }

    fn spawn_child(&self, has_input: bool, use_stderr_pipe: bool) -> Option<tokio::process::Child> {
        if self.command.is_empty() {
            log::error!("shell_command: Command ({}) failed", self.name);
            return None;
        }
        let mut cmd = tokio::process::Command::new(&self.command[0]);
        cmd.args(&self.command[1..]);
        cmd.stdin(if has_input { Stdio::piped() } else { Stdio::inherit() });
        cmd.stdout(Stdio::piped());
        if use_stderr_pipe {
            cmd.stderr(Stdio::piped());
        } else {
            // python redirects stderr to stdout
            cmd.stderr(Stdio::piped());
        }
        if let Some(env) = &self.env {
            cmd.env_clear();
            cmd.envs(env);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.kill_on_drop(false);
        match cmd.spawn() {
            Ok(c) => Some(c),
            Err(e) => {
                log::error!("shell_command: Command ({}) failed\n{}", self.name, e);
                None
            }
        }
    }

    fn check_proc_success(&self, complete: bool, log_complete: bool, success_codes: &[i32]) -> bool {
        let rc = self.return_code.get();
        let success = rc.map(|c| success_codes.contains(&c)).unwrap_or(false) && complete;
        let msg = if success {
            format!("Command ({}) successfully finished", self.name)
        } else if self.cancelled.get() {
            format!("Command ({}) cancelled", self.name)
        } else if !complete {
            format!("Command ({}) timed out", self.name)
        } else {
            format!(
                "Command ({}) exited with return code {}",
                self.name,
                rc.map(|c| c.to_string()).unwrap_or_else(|| "None".into())
            )
        };
        if log_complete {
            log::info!("{}", msg);
        }
        success
    }

    fn reset(&self) {
        self.return_code.set(None);
        self.pid.set(None);
        self.cancelled.set(false);
        self.finished.set(false);
        *self.done_notify.borrow_mut() = Some(Rc::new(tokio::sync::Notify::new()));
    }

    /// Runs the process, collecting output.  Returns (complete, stdout, stderr)
    async fn execute(
        &self,
        timeout: f64,
        sig_idx: usize,
        proc_input: Option<&str>,
        stream_callbacks: bool,
    ) -> Option<(bool, bool, Vec<u8>, Vec<u8>)> {
        let use_err_pipe = self.std_err_cb.is_some() || self.log_stderr;
        let mut child = self.spawn_child(proc_input.is_some(), use_err_pipe)?;
        self.pid.set(child.id());
        if let (Some(input), Some(mut stdin)) = (proc_input, child.stdin.take()) {
            let data = input.as_bytes().to_vec();
            crate::server::spawn(async move {
                let _ = stdin.write_all(&data).await;
            });
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_cb = if stream_callbacks { self.std_out_cb.clone() } else { None };
        // when stderr isn't requested it is merged into stdout
        let err_cb = if stream_callbacks {
            if use_err_pipe { self.std_err_cb.clone() } else { self.std_out_cb.clone() }
        } else {
            None
        };
        let log_stderr = self.log_stderr && stream_callbacks;
        let read_out = read_stream(stdout, out_cb, false);
        let read_err = read_stream(stderr, err_cb, log_stderr);
        let wait = async {
            let (o, e) = futures::join!(read_out, read_err);
            let status = child.wait().await;
            (o, e, status)
        };
        let to = if timeout <= 0.0 { 9999999999.0 } else { timeout };
        let notify = self.done_notify.borrow().clone();
        tokio::pin!(wait);
        let res = tokio::time::timeout(std::time::Duration::from_secs_f64(to), &mut wait).await;
        let (timed_out, out, mut err, status) = match res {
            Ok((o, e, s)) => (false, o, e, s.ok()),
            Err(_) => {
                // cancel concurrently with completing the wait
                let cancel = self.cancel(sig_idx);
                let (_, (o, e, s)) = futures::join!(cancel, async {
                    let r = (&mut wait).await;
                    self.finished.set(true);
                    if let Some(n) = &notify {
                        n.notify_waiters();
                    }
                    r
                });
                (true, o, e, s.ok())
            }
        };
        self.finished.set(true);
        if let Some(n) = &notify {
            n.notify_waiters();
        }
        let code = status.and_then(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().or_else(|| s.signal().map(|sig| -sig))
        });
        self.return_code.set(code);
        if !use_err_pipe {
            // merged output
            let mut merged = out.clone();
            merged.append(&mut err);
            return Some((!timed_out, timed_out, merged, Vec::new()));
        }
        Some((!timed_out, timed_out, out, err))
    }

    pub async fn run(
        self: &Rc<Self>,
        factory: &ShellCommandFactory,
        timeout: f64,
        verbose: bool,
        log_complete: bool,
        sig_idx: usize,
        proc_input: Option<&str>,
        success_codes: Option<&[i32]>,
    ) -> bool {
        let _g = self.run_lock.lock().await;
        factory.add_running_command(self);
        self.reset();
        let verbose = verbose && !(self.std_out_cb.is_none() && self.std_err_cb.is_none() && !self.log_stderr);
        let res = self.execute(timeout, sig_idx, proc_input, verbose).await;
        factory.remove_running_command(self);
        let (complete, _, _, _) = match res {
            None => return false,
            Some(r) => r,
        };
        let complete = complete && !self.cancelled.get();
        self.check_proc_success(complete, log_complete, success_codes.unwrap_or(&[0]))
    }

    pub async fn run_with_response(
        self: &Rc<Self>,
        factory: &ShellCommandFactory,
        timeout: f64,
        attempts: usize,
        log_complete: bool,
        sig_idx: usize,
        proc_input: Option<&str>,
        success_codes: Option<&[i32]>,
    ) -> Result<String, ShellCommandError> {
        let _g = self.run_lock.lock().await;
        factory.add_running_command(self);
        let mut attempts = attempts.max(1);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while attempts > 0 {
            self.reset();
            stdout.clear();
            stderr.clear();
            if let Some((complete, timed_out, out, err)) = self.execute(timeout, sig_idx, proc_input, false).await {
                stdout = out;
                stderr = err;
                let complete = complete && !self.cancelled.get();
                if complete && self.log_stderr && !stderr.is_empty() {
                    log::info!("{}: {}", self.command.first().cloned().unwrap_or_default(), String::from_utf8_lossy(&stderr));
                }
                if self.check_proc_success(complete, log_complete, success_codes.unwrap_or(&[0])) {
                    factory.remove_running_command(self);
                    return Ok(String::from_utf8_lossy(&stdout).trim_end_matches('\n').to_string());
                }
                if !stdout.is_empty() {
                    log::debug!("Shell command '{}' output:\n{}", self.name, String::from_utf8_lossy(&stdout));
                }
                if self.cancelled.get() && !timed_out {
                    break;
                }
            }
            attempts -= 1;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        factory.remove_running_command(self);
        Err(ShellCommandError {
            message: format!("Error running shell command: '{}'", self.name),
            return_code: self.return_code.get(),
            stdout,
            stderr,
            status_code: 500,
        })
    }
}

async fn read_stream<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>, cb: Option<OutputCallback>, log_it: bool) -> Vec<u8> {
    let mut stream = match stream {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut all = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let data = &buf[..n];
        all.extend_from_slice(data);
        if log_it {
            log::info!("{}", String::from_utf8_lossy(data));
        }
        if let Some(cb) = &cb {
            pending.extend_from_slice(data);
            while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                let line = line[..line.len() - 1].to_vec();
                if !line.is_empty() {
                    cb(line);
                }
            }
        }
    }
    if let Some(cb) = &cb {
        if !pending.is_empty() {
            cb(pending);
        }
    }
    all
}

pub struct ShellCommandFactory {
    running: RefCell<Vec<Rc<ShellCommand>>>,
}

#[async_trait(?Send)]
impl Component for ShellCommandFactory {
    async fn close(self: Rc<Self>) {
        let cmds: Vec<Rc<ShellCommand>> = self.running.borrow().clone();
        for c in cmds {
            c.cancel(1).await;
        }
    }
}

impl ShellCommandFactory {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        Rc::new(ShellCommandFactory { running: RefCell::new(Vec::new()) })
    }

    pub fn add_running_command(&self, cmd: &Rc<ShellCommand>) {
        let mut r = self.running.borrow_mut();
        if !r.iter().any(|c| Rc::ptr_eq(c, cmd)) {
            r.push(cmd.clone());
        }
    }

    pub fn remove_running_command(&self, cmd: &Rc<ShellCommand>) {
        self.running.borrow_mut().retain(|c| !Rc::ptr_eq(c, cmd));
    }

    pub fn build_shell_command(
        &self,
        cmd: &str,
        callback: Option<OutputCallback>,
        std_err_callback: Option<OutputCallback>,
        env: Option<HashMap<String, String>>,
        log_stderr: bool,
        cwd: Option<String>,
    ) -> Rc<ShellCommand> {
        ShellCommand::new(cmd, callback, std_err_callback, env, log_stderr, cwd)
    }

    /// Runs a command processing output as it is received
    #[allow(clippy::too_many_arguments)]
    pub async fn run_cmd_async(
        &self,
        cmd: &str,
        callback: Option<OutputCallback>,
        std_err_callback: Option<OutputCallback>,
        timeout: f64,
        attempts: usize,
        verbose: bool,
        sig_idx: usize,
        proc_input: Option<&str>,
        log_complete: bool,
        log_stderr: bool,
        env: Option<HashMap<String, String>>,
        cwd: Option<String>,
        success_codes: Option<&[i32]>,
    ) -> Result<(), ShellCommandError> {
        let scmd = ShellCommand::new(cmd, callback, std_err_callback, env, log_stderr, cwd);
        for _ in 0..attempts.max(1) {
            if scmd.run(self, timeout, verbose, log_complete, sig_idx, proc_input, success_codes).await {
                return Ok(());
            }
        }
        Err(ShellCommandError {
            message: format!("Error running command {}", cmd),
            return_code: scmd.get_return_code(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            status_code: 500,
        })
    }

    /// Executes a command and returns decoded stdout
    pub async fn exec_cmd(&self, cmd: &str, opts: ExecOpts<'_>) -> Result<String, ShellCommandError> {
        let scmd = ShellCommand::new(cmd, None, None, opts.env, opts.log_stderr, opts.cwd);
        scmd.run_with_response(self, opts.timeout, opts.attempts, opts.log_complete, opts.sig_idx, opts.proc_input, opts.success_codes)
            .await
    }

    pub async fn exec(&self, cmd: &str, timeout: f64) -> Result<String, ShellCommandError> {
        self.exec_cmd(cmd, ExecOpts { timeout, ..Default::default() }).await
    }
}

pub struct ExecOpts<'a> {
    pub timeout: f64,
    pub attempts: usize,
    pub sig_idx: usize,
    pub proc_input: Option<&'a str>,
    pub log_complete: bool,
    pub log_stderr: bool,
    pub env: Option<HashMap<String, String>>,
    pub cwd: Option<String>,
    pub success_codes: Option<&'a [i32]>,
}

impl Default for ExecOpts<'_> {
    fn default() -> Self {
        ExecOpts {
            timeout: 2.0,
            attempts: 1,
            sig_idx: 1,
            proc_input: None,
            log_complete: true,
            log_stderr: false,
            env: None,
            cwd: None,
            success_codes: None,
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Rc<ShellCommandFactory> {
    ShellCommandFactory::new(config)
}

#[cfg(test)]
mod tests {
    #[test]
    fn shlex() {
        assert_eq!(super::shlex_split("a 'b c' \"d \\\"e\" f\\ g"), vec!["a", "b c", "d \"e", "f g"]);
    }
}
