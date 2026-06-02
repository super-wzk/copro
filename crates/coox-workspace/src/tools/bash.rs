use coox_harness::tools::{Tool, ToolContext};
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

pub const BASH_TOOL_NAME: &str = "bash";

const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2_000;
const PROCESS_TERMINATE_GRACE: Duration = Duration::from_secs(2);

const BASH_TOOL_DESCRIPTION: &str = concat!(
    "Run a bash command in the workspace root. Prefer dedicated tools for file reads, ",
    "searches, edits, and other structured operations; use bash only when a specialized ",
    "tool is not available or when build, test, inspection, or shell behavior is required. ",
    "Do not use this for long-running or persistent background tasks that need later control; ",
    "use a dedicated background task/session tool when available. If a command intentionally ",
    "starts a short-lived background helper, detach it explicitly, redirect output, and print ",
    "the PID or status needed by the user. The timeout is in seconds. Output is ",
    "tail-truncated to 50KB or 2000 lines per stream."
);

#[derive(Clone)]
pub struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct BashInput {
    /// Bash command to execute with `bash -lc`.
    pub command: String,
    /// Maximum command runtime in seconds.
    pub timeout: u64,
}

#[async_trait]
impl Tool for BashTool {
    type Input = BashInput;
    type Output = String;

    fn name(&self) -> &str {
        BASH_TOOL_NAME
    }

    fn description(&self) -> &str {
        BASH_TOOL_DESCRIPTION
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn call(&self, input: Self::Input, context: ToolContext) -> Result<Self::Output, String> {
        let cancel = context.cancellation().clone();
        if cancel.is_cancelled() {
            return Err("bash cancelled".to_string());
        }

        let command = input.command.trim();
        if command.is_empty() {
            return Err("command cannot be empty".to_string());
        }
        if input.timeout == 0 {
            return Err("timeout must be greater than 0 seconds".to_string());
        }

        let output = run_bash_command(&self.cwd, command, input.timeout, cancel).await?;
        Ok(format_bash_output(output))
    }
}

async fn run_bash_command(
    cwd: &Path,
    command: &str,
    timeout_secs: u64,
    cancel: CancellationToken,
) -> Result<BashOutput, String> {
    let mut process = Command::new("bash");
    process
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_tree(&mut process);

    let mut child = process
        .spawn()
        .map_err(|error| format!("failed to run bash command: {error}"))?;
    let process_tree = ProcessTree::capture(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture bash stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture bash stderr".to_string())?;
    let stdout_task = tokio::spawn(read_tail(stdout));
    let stderr_task = tokio::spawn(read_tail(stderr));

    let timeout = Duration::from_secs(timeout_secs);
    let mut timeout = Box::pin(tokio::time::sleep(timeout));
    tokio::select! {
        _ = cancel.cancelled() => {
            terminate_child_tree(&mut child, &process_tree).await;
            let output = collect_bash_output(None, stdout_task, stderr_task).await;
            Err(format!("bash cancelled\n{}", format_bash_output(output)))
        }
        _ = &mut timeout => {
            terminate_child_tree(&mut child, &process_tree).await;
            let output = collect_bash_output(None, stdout_task, stderr_task).await;
            Err(format!(
                "bash command timed out after {timeout_secs}s\n{}",
                format_bash_output(output)
            ))
        }
        status = child.wait() => {
            let status = status.map_err(|error| format!("failed to wait for bash command: {error}"))?;
            Ok(collect_bash_output(Some(status), stdout_task, stderr_task).await)
        },
    }
}

async fn collect_bash_output(
    status: Option<ExitStatus>,
    stdout_task: JoinHandle<Result<TruncatedOutput, String>>,
    stderr_task: JoinHandle<Result<TruncatedOutput, String>>,
) -> BashOutput {
    let (stdout, stderr) = tokio::join!(
        collect_stream_output(stdout_task, "stdout"),
        collect_stream_output(stderr_task, "stderr")
    );

    BashOutput {
        status,
        stdout,
        stderr,
    }
}

async fn collect_stream_output(
    task: JoinHandle<Result<TruncatedOutput, String>>,
    label: &str,
) -> TruncatedOutput {
    match task.await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => TruncatedOutput::from_text(format!("<failed to read {label}: {error}>")),
        Err(error) => {
            TruncatedOutput::from_text(format!("<failed to join {label} reader: {error}>"))
        }
    }
}

async fn terminate_child_tree(child: &mut Child, process_tree: &ProcessTree) {
    process_tree.terminate(child);

    if matches!(
        tokio::time::timeout(PROCESS_TERMINATE_GRACE, child.wait()).await,
        Ok(Ok(_))
    ) {
        return;
    }

    process_tree.kill(child);
    let _ = child.kill().await;
}

#[cfg(unix)]
fn configure_process_tree(process: &mut Command) {
    process.process_group(0);
}

#[cfg(windows)]
fn configure_process_tree(_process: &mut Command) {}

#[cfg(not(any(unix, windows)))]
fn configure_process_tree(_process: &mut Command) {}

#[cfg(unix)]
struct ProcessTree;

#[cfg(unix)]
impl ProcessTree {
    fn capture(_child: &Child) -> Self {
        Self
    }

    fn terminate(&self, child: &Child) {
        if let Some(pid) = child.id() {
            signal_process_group(pid, libc::SIGTERM);
        }
    }

    fn kill(&self, child: &Child) {
        if let Some(pid) = child.id() {
            signal_process_group(pid, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: Option<WindowsJob>,
}

#[cfg(windows)]
impl ProcessTree {
    fn capture(child: &Child) -> Self {
        Self {
            job: WindowsJob::capture(child),
        }
    }

    fn terminate(&self, _child: &Child) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }

    fn kill(&self, _child: &Child) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn capture(_child: &Child) -> Self {
        Self
    }

    fn terminate(&self, _child: &Child) {}

    fn kill(&self, _child: &Child) {}
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        libc::kill(pgid, signal);
    }
}

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
unsafe impl Sync for WindowsJob {}

#[cfg(windows)]
impl WindowsJob {
    fn capture(child: &Child) -> Option<Self> {
        use std::mem::size_of;
        use std::ptr::null;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let process_handle = child.raw_handle()? as HANDLE;
        unsafe {
            let job = CreateJobObjectW(null(), null());
            if job.is_null() {
                return None;
            }

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0;
            if !configured {
                CloseHandle(job);
                return None;
            }

            if AssignProcessToJobObject(job, process_handle) == 0 {
                CloseHandle(job);
                return None;
            }

            Some(Self(job))
        }
    }

    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

async fn read_tail<R>(mut reader: R) -> Result<TruncatedOutput, String>
where
    R: AsyncRead + Unpin,
{
    let mut output = TailOutput::default();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(output.finish());
        }
        output.push(&buffer[..read]);
    }
}

fn format_bash_output(output: BashOutput) -> String {
    let mut text = format!("exit code: {}\n", exit_status_label(output.status));

    if output.stdout.is_empty() && output.stderr.is_empty() {
        text.push_str("stdout: <empty>\nstderr: <empty>");
        return text;
    }

    if !output.stdout.is_empty() {
        text.push_str("\nstdout:\n");
        text.push_str(&output.stdout.content);
        append_truncation_note(&mut text, "stdout", &output.stdout);
    }
    if !output.stderr.is_empty() {
        text.push_str("\nstderr:\n");
        text.push_str(&output.stderr.content);
        append_truncation_note(&mut text, "stderr", &output.stderr);
    }

    text
}

fn append_truncation_note(text: &mut String, label: &str, output: &TruncatedOutput) {
    if !output.truncated {
        return;
    }

    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "[{label} truncated: showing tail, {} of {} bytes, {} of {} lines]\n",
        output.content.len(),
        output.total_bytes,
        output.visible_lines,
        output.total_lines
    ));
}

fn exit_status_label(status: Option<ExitStatus>) -> String {
    status
        .and_then(|status| status.code())
        .map(|code| code.to_string())
        .unwrap_or_else(|| "<terminated>".to_string())
}

struct BashOutput {
    status: Option<ExitStatus>,
    stdout: TruncatedOutput,
    stderr: TruncatedOutput,
}

#[derive(Default)]
struct TailOutput {
    tail: VecDeque<u8>,
    total_bytes: usize,
    total_newlines: usize,
    last_byte: Option<u8>,
}

impl TailOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes += bytes.len();
        self.total_newlines += bytes.iter().filter(|byte| **byte == b'\n').count();
        self.last_byte = bytes.last().copied().or(self.last_byte);

        self.tail.extend(bytes);
        while self.tail.len() > MAX_OUTPUT_BYTES {
            self.tail.pop_front();
        }
    }

    fn finish(self) -> TruncatedOutput {
        let total_lines = if self.total_bytes == 0 {
            0
        } else {
            self.total_newlines + usize::from(self.last_byte != Some(b'\n'))
        };
        let bytes: Vec<_> = self.tail.into_iter().collect();
        let mut content = String::from_utf8_lossy(&bytes).into_owned();
        let bytes_truncated = self.total_bytes > bytes.len();
        let lines_before = count_lines(&content);

        let lines_truncated = lines_before > MAX_OUTPUT_LINES;
        if lines_truncated {
            content = tail_lines(&content, MAX_OUTPUT_LINES);
        }

        TruncatedOutput {
            visible_lines: count_lines(&content),
            content,
            total_bytes: self.total_bytes,
            total_lines,
            truncated: bytes_truncated || lines_truncated,
        }
    }
}

struct TruncatedOutput {
    content: String,
    total_bytes: usize,
    total_lines: usize,
    visible_lines: usize,
    truncated: bool,
}

impl TruncatedOutput {
    fn from_text(content: String) -> Self {
        let total_bytes = content.len();
        let total_lines = count_lines(&content);
        Self {
            content,
            total_bytes,
            total_lines,
            visible_lines: total_lines,
            truncated: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines = text.lines().count();
    let skip = lines.saturating_sub(max_lines);
    let mut tail = text.lines().skip(skip).collect::<Vec<_>>().join("\n");
    if text.ends_with('\n') {
        tail.push('\n');
    }
    tail
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}
