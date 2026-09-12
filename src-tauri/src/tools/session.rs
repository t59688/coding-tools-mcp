use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::harness::model::{FileChangeRecord, ProjectBaseline, TaskStatus};
use crate::harness::state::diff_baselines;
use crate::planning::{ExecutionLedgerUpdate, PlanningService};
use crate::tools::context::ToolContext;
use crate::tools::workspace::{tool_ok, WorkspaceError};

const SESSION_BUFFER_BYTES: usize = 1_048_576;
const SESSION_RETAIN_FOR: Duration = Duration::from_secs(120);
const MAX_RETAINED_SESSIONS: usize = 128;

static WORKSPACE_SESSION_STORES: OnceLock<Mutex<HashMap<PathBuf, Vec<Weak<SessionStore>>>>> =
    OnceLock::new();

#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Arc<ExecSession>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session: ExecSession) -> Arc<ExecSession> {
        self.prune();
        let arc = Arc::new(session);
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(arc.session_id.clone(), arc.clone());
        self.prune_to_capacity();
        arc
    }

    pub fn get(&self, session_id: &str) -> Result<Arc<ExecSession>, WorkspaceError> {
        self.prune();
        self.sessions
            .lock()
            .expect("sessions lock")
            .get(session_id)
            .cloned()
            .ok_or_else(|| WorkspaceError::Tool {
                code: "SESSION_NOT_FOUND",
                message: format!("Session not found or retention expired: {session_id}"),
                category: "not_found",
                retryable: false,
            })
    }

    pub fn remove(&self, session_id: &str) {
        self.sessions
            .lock()
            .expect("sessions lock")
            .remove(session_id);
    }

    pub fn task_sessions(&self, task_id: &str) -> Vec<Arc<ExecSession>> {
        self.prune();
        self.sessions
            .lock()
            .expect("sessions lock")
            .values()
            .filter(|session| session.tracked_task_id.as_deref() == Some(task_id))
            .cloned()
            .collect()
    }

    fn session_ids(&self) -> Vec<String> {
        self.prune();
        self.sessions
            .lock()
            .expect("sessions lock")
            .keys()
            .cloned()
            .collect()
    }

    fn prune(&self) {
        let mut sessions = self.sessions.lock().expect("sessions lock");
        sessions.retain(|_, session| !session.retention_expired());
    }

    fn prune_to_capacity(&self) {
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if sessions.len() <= MAX_RETAINED_SESSIONS {
            return;
        }
        let mut completed = sessions
            .iter()
            .filter_map(|(id, session)| {
                session
                    .completed_at()
                    .map(|completed_at| (id.clone(), completed_at))
            })
            .collect::<Vec<_>>();
        completed.sort_by_key(|(_, completed_at)| *completed_at);
        let remove_count = sessions.len().saturating_sub(MAX_RETAINED_SESSIONS);
        for (id, _) in completed.into_iter().take(remove_count) {
            sessions.remove(&id);
        }
    }
}

pub fn register_workspace_session_store(workspace_root: &Path, store: &Arc<SessionStore>) {
    let registry = WORKSPACE_SESSION_STORES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry.lock().expect("workspace session registry lock");
    let stores = registry.entry(workspace_root.to_path_buf()).or_default();
    stores.retain(|entry| entry.strong_count() > 0);
    if !stores
        .iter()
        .filter_map(Weak::upgrade)
        .any(|registered| Arc::ptr_eq(&registered, store))
    {
        stores.push(Arc::downgrade(store));
    }
}

pub fn kill_workspace_sessions(workspace_root: &Path) -> usize {
    let stores = WORKSPACE_SESSION_STORES
        .get()
        .and_then(|registry| {
            let mut registry = registry.lock().expect("workspace session registry lock");
            let stores = registry.get_mut(workspace_root)?;
            stores.retain(|entry| entry.strong_count() > 0);
            Some(stores.iter().filter_map(Weak::upgrade).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    stores
        .into_iter()
        .map(|store| {
            store
                .session_ids()
                .into_iter()
                .filter(|session_id| {
                    let Ok(session) = store.get(session_id) else {
                        return false;
                    };
                    tauri::async_runtime::block_on(async {
                        if session.is_running().await {
                            session.mark_termination_reason("killed");
                            let _ = deliver_signal(&session, "KILL").await;
                            let _ = wait_for_exit(&session, 1500).await;
                        }
                    });
                    store.remove(session_id);
                    true
                })
                .count()
        })
        .sum()
}

pub struct ExecSession {
    pub session_id: String,
    pub(crate) child: AsyncMutex<Child>,
    pub stdin: AsyncMutex<Option<ChildStdin>>,
    stdin_open: Mutex<bool>,
    interactive_requested: bool,
    pty_attached: bool,
    stderr_merged: bool,
    stdout: Mutex<Vec<u8>>,
    stderr: Mutex<Vec<u8>>,
    stdout_total: Mutex<usize>,
    stderr_total: Mutex<usize>,
    pub started_at: Instant,
    pub exit_code: Mutex<Option<i32>>,
    exited: AtomicBool,
    termination_reason: Mutex<Option<String>>,
    completed_at: Mutex<Option<Instant>>,
    retention_scheduled: AtomicBool,
    reader_tasks: AsyncMutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
    tracked_task_id: Option<String>,
    baseline_before: Option<ProjectBaseline>,
    baseline_absorbed: AtomicBool,
    affected_files: Mutex<Vec<FileChangeRecord>>,
    operation_id: Mutex<Option<String>>,
    operation_finalized: AtomicBool,
    command: String,
}

impl ExecSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_mode(
        mut child: Child,
        interactive_requested: bool,
        pty_attached: bool,
        stderr_merged: bool,
        tracked_task_id: Option<String>,
        baseline_before: Option<ProjectBaseline>,
        command: String,
    ) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let stdin = child.stdin.take();
        let stdin_open = stdin.is_some();
        Self {
            session_id,
            child: AsyncMutex::new(child),
            stdin: AsyncMutex::new(stdin),
            stdin_open: Mutex::new(stdin_open),
            interactive_requested,
            pty_attached,
            stderr_merged,
            stdout: Mutex::new(Vec::new()),
            stderr: Mutex::new(Vec::new()),
            stdout_total: Mutex::new(0),
            stderr_total: Mutex::new(0),
            started_at: Instant::now(),
            exit_code: Mutex::new(None),
            exited: AtomicBool::new(false),
            termination_reason: Mutex::new(None),
            completed_at: Mutex::new(None),
            retention_scheduled: AtomicBool::new(false),
            reader_tasks: AsyncMutex::new(Vec::new()),
            tracked_task_id,
            baseline_before,
            baseline_absorbed: AtomicBool::new(false),
            affected_files: Mutex::new(Vec::new()),
            operation_id: Mutex::new(None),
            operation_finalized: AtomicBool::new(false),
            command,
        }
    }

    pub async fn spawn_readers(self: &Arc<Self>) {
        let stdout = {
            let mut guard = self.child.lock().await;
            guard.stdout.take()
        };
        let stderr = {
            let mut guard = self.child.lock().await;
            guard.stderr.take()
        };
        if let Some(stream) = stdout {
            let session = Arc::clone(self);
            let task = tauri::async_runtime::spawn(async move {
                session.read_stream(stream, true).await;
            });
            self.reader_tasks.lock().await.push(task);
        }
        if let Some(stream) = stderr {
            let session = Arc::clone(self);
            let task = tauri::async_runtime::spawn(async move {
                session.read_stream(stream, false).await;
            });
            self.reader_tasks.lock().await.push(task);
        }
    }

    pub async fn wait_for_readers(&self) {
        let mut tasks = self.reader_tasks.lock().await;
        while let Some(task) = tasks.pop() {
            let _ = tokio::time::timeout(Duration::from_millis(750), task).await;
        }
    }

    async fn read_stream<T>(&self, mut stream: T, is_stdout: bool)
    where
        T: tokio::io::AsyncRead + Unpin,
    {
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    if is_stdout {
                        let mut data = self.stdout.lock().expect("stdout lock");
                        data.extend_from_slice(chunk);
                        *self.stdout_total.lock().expect("stdout_total lock") += n;
                        trim_buffer(&mut data, SESSION_BUFFER_BYTES);
                    } else {
                        let mut data = self.stderr.lock().expect("stderr lock");
                        data.extend_from_slice(chunk);
                        *self.stderr_total.lock().expect("stderr_total lock") += n;
                        trim_buffer(&mut data, SESSION_BUFFER_BYTES);
                    }
                }
                Err(_) => break,
            }
        }
    }

    pub async fn kill_and_wait(&self) {
        self.mark_termination_reason("killed");
        let _ = deliver_signal(self, "KILL").await;
        let _ = wait_for_exit(self, 1500).await;
    }

    pub async fn refresh_status(&self) {
        let mut child = self.child.lock().await;
        if let Ok(Some(status)) = child.try_wait() {
            self.record_exit_status(status);
        }
    }

    fn record_exit_status(&self, status: std::process::ExitStatus) {
        *self.exit_code.lock().expect("exit_code lock") = status.code();
        self.exited.store(true, Ordering::Release);
        *self.stdin_open.lock().expect("stdin_open lock") = false;
        let mut completed_at = self.completed_at.lock().expect("completed_at lock");
        if completed_at.is_none() {
            *completed_at = Some(Instant::now());
        }
        let mut reason = self.termination_reason.lock().expect("termination lock");
        if reason.is_none() {
            *reason = Some("exited".into());
        }
    }

    pub(crate) fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    pub fn mark_termination_reason(&self, reason: &str) {
        *self.termination_reason.lock().expect("termination lock") = Some(reason.to_string());
    }

    pub(crate) fn mark_stdin_closed(&self) {
        *self.stdin_open.lock().expect("stdin_open lock") = false;
    }

    pub async fn is_running(&self) -> bool {
        self.refresh_status().await;
        !self.has_exited()
    }

    pub fn tracked_task_id(&self) -> Option<&str> {
        self.tracked_task_id.as_deref()
    }

    pub fn bind_operation(&self, operation_id: &str) {
        *self.operation_id.lock().expect("operation id lock") = Some(operation_id.to_string());
    }

    pub fn completed_at(&self) -> Option<Instant> {
        *self.completed_at.lock().expect("completed_at lock")
    }

    fn retention_expired(&self) -> bool {
        self.completed_at()
            .is_some_and(|completed| completed.elapsed() >= SESSION_RETAIN_FOR)
    }

    pub fn retained_stream_bytes(&self, stream: &str) -> (Vec<u8>, usize, usize) {
        let (data, total) = match stream {
            "stderr" => (
                self.stderr.lock().expect("stderr lock").clone(),
                *self.stderr_total.lock().expect("stderr_total lock"),
            ),
            _ => (
                self.stdout.lock().expect("stdout lock").clone(),
                *self.stdout_total.lock().expect("stdout_total lock"),
            ),
        };
        let retained_start = total.saturating_sub(data.len());
        (data, total, retained_start)
    }

    pub fn snapshot(&self, max_output_bytes: usize) -> Value {
        let stdout_bytes = self.stdout.lock().expect("stdout lock").clone();
        let stderr_bytes = self.stderr.lock().expect("stderr lock").clone();
        let stdout = truncate_tail(&stdout_bytes, max_output_bytes);
        let stderr = truncate_tail(&stderr_bytes, max_output_bytes);
        let exit_code = *self.exit_code.lock().expect("exit_code lock");
        let termination_reason = self
            .termination_reason
            .lock()
            .expect("termination lock")
            .clone();
        let status = if self.has_exited() {
            "exited"
        } else {
            "running"
        };
        let reason = termination_reason.as_deref().unwrap_or("running");
        let command_ok = match reason {
            "exited" => Some(exit_code.is_some_and(|code| code == 0)),
            "running" => None,
            _ => Some(false),
        };
        let operation_id = self.operation_id.lock().expect("operation id lock").clone();
        let affected_files = self.affected_files.lock().expect("affected files lock").clone();
        json!({
            "session_id": self.session_id,
            "operation_id": operation_id,
            "tracked_task_id": self.tracked_task_id,
            "interactive_requested": self.interactive_requested,
            "interactive": self.pty_attached,
            "pty_attached": self.pty_attached,
            "stderr_merged": self.stderr_merged,
            "stdin_open": *self.stdin_open.lock().expect("stdin_open lock"),
            "status": status,
            "termination_reason": reason,
            "recoverable": matches!(reason, "timeout" | "killed" | "spawn_failed" | "server_restart"),
            "suggestion": match reason {
                "timeout" => "读取保留输出，调整 timeout_ms 后重试",
                "killed" => "确认终止原因后重新执行命令",
                "exited" => "检查 exit_code 和 stderr",
                "crashed" => "检查 stderr 后重试或恢复工作区",
                _ => "继续读取 session 或等待进程结束",
            },
            "exit_code": exit_code,
            "transport_ok": true,
            "command_ok": command_ok,
            "stdout": stdout.content,
            "stderr": stderr.content,
            "stdout_truncated": stdout.truncated,
            "stderr_truncated": stderr.truncated,
            "elapsed_ms": self.started_at.elapsed().as_millis(),
            "retention_seconds": SESSION_RETAIN_FOR.as_secs(),
            "affected_files": affected_files,
            "output_refs": {
                "stdout": format!("session:{}:stdout", self.session_id),
                "stderr": format!("session:{}:stderr", self.session_id)
            }
        })
    }
}

fn trim_buffer(buf: &mut Vec<u8>, limit: usize) {
    if buf.len() > limit {
        let drop = buf.len() - limit;
        buf.drain(..drop);
    }
}

struct Truncated {
    content: String,
    truncated: bool,
}

fn truncate_tail(bytes: &[u8], max_bytes: usize) -> Truncated {
    let truncated = bytes.len() > max_bytes;
    let take = bytes.len().min(max_bytes);
    Truncated {
        content: String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(take)..]).into_owned(),
        truncated,
    }
}

pub fn bind_operation_and_finalize(ctx: &ToolContext, session_id: &str, operation_id: &str) {
    if let Ok(session) = ctx.sessions.get(session_id) {
        session.bind_operation(operation_id);
        tauri::async_runtime::block_on(session.refresh_status());
        if session.has_exited() {
            let _ = finalize_session(ctx, &session);
            schedule_session_eviction(ctx.sessions.clone(), session);
        }
    }
}

pub fn reconcile_task_sessions(ctx: &ToolContext, task_id: &str) -> Result<bool, WorkspaceError> {
    let sessions = ctx.sessions.task_sessions(task_id);
    let mut running = false;
    for session in sessions {
        tauri::async_runtime::block_on(session.refresh_status());
        if session.has_exited() {
            finalize_session(ctx, &session)?;
            schedule_session_eviction(ctx.sessions.clone(), session);
        } else {
            running = true;
        }
    }
    Ok(running)
}

pub fn read_output(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let output_ref = args
        .get("output_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkspaceError::invalid_argument("output_ref is required"))?;
    let parts: Vec<&str> = output_ref.split(':').collect();
    if parts.len() != 3 || parts[0] != "session" {
        return Err(WorkspaceError::invalid_argument(
            "output_ref must look like session:<id>:stdout, session:<id>:stderr, or session:<id>:full",
        ));
    }
    let session_id = parts[1];
    let ref_stream = parts[2];
    if ref_stream != "stdout" && ref_stream != "stderr" && ref_stream != "full" {
        return Err(WorkspaceError::invalid_argument(
            "output_ref stream must be stdout, stderr, or full",
        ));
    }
    let session = ctx.sessions.get(session_id)?;
    tauri::async_runtime::block_on(session.refresh_status());
    if session.has_exited() {
        finalize_session(ctx, &session)?;
        schedule_session_eviction(ctx.sessions.clone(), session.clone());
    }

    let requested_stream = args.get("stream").and_then(Value::as_str).unwrap_or("");
    let stream = if ref_stream == "stdout" || ref_stream == "stderr" {
        ref_stream
    } else if requested_stream == "stdout" || requested_stream == "stderr" {
        requested_stream
    } else {
        "stdout"
    };

    let (data, total_stream_bytes, retained_start_offset) = session.retained_stream_bytes(stream);
    let requested_offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(4096)
        .clamp(1, 1_048_576) as usize;
    let actual_offset = requested_offset
        .max(retained_start_offset)
        .min(total_stream_bytes);
    let local_offset = actual_offset.saturating_sub(retained_start_offset).min(data.len());
    let local_end = data.len().min(local_offset.saturating_add(limit));
    let chunk = &data[local_offset..local_end];
    let absolute_end = actual_offset.saturating_add(chunk.len());
    let next_offset = (absolute_end < total_stream_bytes).then_some(absolute_end as u64);
    let dropped_before_requested = requested_offset < retained_start_offset;
    let mut warnings = Vec::new();
    if ref_stream == "full" {
        warnings.push("legacy full output_ref defaults to stdout; use output_refs for stable stream paging");
    }
    if dropped_before_requested {
        warnings.push("requested offset was older than retained output; page starts at retained_start_offset");
    }

    Ok(tool_ok(json!({
        "output_ref": output_ref,
        "stream_output_ref": format!("session:{session_id}:{stream}"),
        "stream": stream,
        "offset": actual_offset,
        "requested_offset": requested_offset,
        "limit": limit,
        "content": String::from_utf8_lossy(chunk),
        "next_offset": next_offset,
        "retained_start_offset": retained_start_offset,
        "dropped_bytes": retained_start_offset,
        "total_retained_bytes": data.len(),
        "total_stream_bytes": total_stream_bytes,
        "retention_truncated": retained_start_offset > 0,
        "truncated": next_offset.is_some(),
        "session_status": if session.has_exited() { "exited" } else { "running" },
        "exit_code": *session.exit_code.lock().expect("exit_code lock"),
        "command_ok": session.snapshot(1)["command_ok"].clone(),
        "operation_id": session.operation_id.lock().expect("operation id lock").clone(),
        "affected_files": session.affected_files.lock().expect("affected files lock").clone(),
        "warnings": warnings
    })))
}

pub fn write_stdin(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkspaceError::invalid_argument("session_id is required"))?;
    let session = ctx.sessions.get(session_id)?;
    ensure_session_task_writable(ctx, &session)?;
    let chars = args.get("chars").and_then(Value::as_str).unwrap_or("");
    let max_output_bytes = args
        .get("max_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(65_536) as usize;

    let running = tauri::async_runtime::block_on(session.is_running());
    if !running {
        if !chars.is_empty() {
            return Err(WorkspaceError::Tool {
                code: "SESSION_CLOSED",
                message: "Session is closed; stdin write blocked.".into(),
                category: "runtime",
                retryable: false,
            });
        }
        finalize_session(ctx, &session)?;
        schedule_session_eviction(ctx.sessions.clone(), session.clone());
        return Ok(tool_ok(session.snapshot(max_output_bytes)));
    }

    if !chars.is_empty() {
        let mut stdin_guard = tauri::async_runtime::block_on(session.stdin.lock());
        let stdin = stdin_guard.as_mut().ok_or_else(|| WorkspaceError::Tool {
            code: "SESSION_CLOSED",
            message: "Session stdin is closed.".into(),
            category: "runtime",
            retryable: false,
        })?;
        use tokio::io::AsyncWriteExt;
        tauri::async_runtime::block_on(async {
            stdin
                .write_all(chars.as_bytes())
                .await
                .map_err(|_| WorkspaceError::Tool {
                    code: "SESSION_CLOSED",
                    message: "Session stdin is closed.".into(),
                    category: "runtime",
                    retryable: false,
                })
        })?;
        let _ = tauri::async_runtime::block_on(stdin.flush());
    }

    let yield_ms = args
        .get("yield_time_ms")
        .and_then(Value::as_u64)
        .unwrap_or(1000)
        .min(30_000);
    if yield_ms > 0 {
        std::thread::sleep(Duration::from_millis(yield_ms));
    }
    tauri::async_runtime::block_on(session.refresh_status());
    if session.has_exited() {
        tauri::async_runtime::block_on(session.wait_for_readers());
        finalize_session(ctx, &session)?;
        schedule_session_eviction(ctx.sessions.clone(), session.clone());
    }
    Ok(tool_ok(session.snapshot(max_output_bytes)))
}

pub fn kill_session(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkspaceError::invalid_argument("session_id is required"))?;
    let session = ctx.sessions.get(session_id)?;
    let max_output_bytes = args
        .get("max_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(65_536) as usize;
    let wait_ms = args
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5000)
        .min(30_000);
    let signal = args.get("signal").and_then(Value::as_str).unwrap_or("TERM");
    if !matches!(signal, "TERM" | "KILL" | "INT") {
        return Err(WorkspaceError::invalid_argument("signal must be TERM, KILL, or INT"));
    }

    let running = tauri::async_runtime::block_on(session.is_running());
    let mut killed = false;
    let mut status = "exited";
    let mut escalated = false;
    let mut signal_effective = signal.to_string();
    let mut warnings = Vec::<String>::new();

    if running {
        session.mark_termination_reason("killed");
        let delivery = tauri::async_runtime::block_on(deliver_signal(&session, signal))?;
        signal_effective = delivery.effective;
        if let Some(warning) = delivery.warning {
            warnings.push(warning);
        }
        let exited = tauri::async_runtime::block_on(wait_for_exit(&session, wait_ms));
        if !exited && signal != "KILL" {
            escalated = true;
            let delivery = tauri::async_runtime::block_on(deliver_signal(&session, "KILL"))?;
            signal_effective = delivery.effective;
            if let Some(warning) = delivery.warning {
                warnings.push(warning);
            }
            let _ = tauri::async_runtime::block_on(wait_for_exit(&session, 1000));
        }
        tauri::async_runtime::block_on(session.refresh_status());
        if session.has_exited() {
            tauri::async_runtime::block_on(session.wait_for_readers());
            killed = true;
            status = "killed";
            finalize_session(ctx, &session)?;
            schedule_session_eviction(ctx.sessions.clone(), session.clone());
        } else {
            status = "terminating";
            warnings.push("Process tree did not exit within the requested wait window".into());
        }
    } else {
        finalize_session(ctx, &session)?;
        schedule_session_eviction(ctx.sessions.clone(), session.clone());
    }

    let mut payload = session.snapshot(max_output_bytes);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("killed".into(), json!(killed));
        obj.insert("status".into(), json!(status));
        obj.insert("evicted".into(), Value::Bool(false));
        obj.insert("signal_requested".into(), json!(signal));
        obj.insert("signal_effective".into(), json!(signal_effective));
        obj.insert("escalated".into(), json!(escalated));
        if !warnings.is_empty() {
            obj.insert("warnings".into(), json!(warnings));
        }
    }
    Ok(tool_ok(payload))
}

fn ensure_session_task_writable(ctx: &ToolContext, session: &ExecSession) -> Result<(), WorkspaceError> {
    let Some(task_id) = session.tracked_task_id() else {
        return Ok(());
    };
    let task = ctx.harness.task(task_id).map_err(|error| WorkspaceError::Tool {
        code: "TASK_STATE_UNAVAILABLE",
        message: error.to_string(),
        category: "runtime",
        retryable: true,
    })?;
    if task.status.is_writable() {
        Ok(())
    } else {
        Err(WorkspaceError::Tool {
            code: "TASK_NOT_WRITABLE",
            message: "Tracked command task is no longer writable; use kill_session or resume the task.".into(),
            category: "permission",
            retryable: true,
        })
    }
}

fn finalize_session(ctx: &ToolContext, session: &Arc<ExecSession>) -> Result<(), WorkspaceError> {
    if !session.has_exited() {
        return Ok(());
    }

    if !session.baseline_absorbed.load(Ordering::Acquire) {
        if let (Some(task_id), Some(before)) = (session.tracked_task_id(), session.baseline_before.as_ref()) {
            let after = ctx.harness.capture_current_baseline(Some(before));
            let changes = diff_baselines(before, &after);
            ctx.harness
                .refresh_expected_state(task_id)
                .map_err(|error| WorkspaceError::Tool {
                    code: "BASELINE_REFRESH_FAILED",
                    message: error.to_string(),
                    category: "runtime",
                    retryable: true,
                })?;
            *session.affected_files.lock().expect("affected files lock") = changes;
        }
        session.baseline_absorbed.store(true, Ordering::Release);
    }

    finalize_operation_record(ctx, session);
    finalize_planning_ledger(ctx, session);
    Ok(())
}

fn finalize_operation_record(ctx: &ToolContext, session: &ExecSession) {
    let Some(operation_id) = session.operation_id.lock().expect("operation id lock").clone() else {
        return;
    };
    if session
        .operation_finalized
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let snapshot = session.snapshot(65_536);
    let kind = terminal_operation_kind(&snapshot);
    if ctx
        .harness
        .record_operation(
            Some(&operation_id),
            session.tracked_task_id(),
            "exec_command",
            kind,
            json!({"reason": "session_terminal"}),
            snapshot,
        )
        .is_err()
    {
        session.operation_finalized.store(false, Ordering::Release);
    }
}

fn finalize_planning_ledger(ctx: &ToolContext, session: &ExecSession) {
    let snapshot = session.snapshot(65_536);
    let state = match terminal_operation_kind(&snapshot) {
        "completed" => "completed",
        "cancelled" => "cancelled",
        _ => "failed",
    };
    let last_error = if state == "failed" {
        snapshot
            .get("stderr")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                snapshot
                    .get("termination_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
    } else {
        None
    };
    let changed_files = session
        .affected_files
        .lock()
        .expect("affected files lock")
        .iter()
        .map(|change| change.path.clone())
        .collect();
    let _ = PlanningService::new(ctx.workspace.root()).record_execution(ExecutionLedgerUpdate {
        task_id: session.tracked_task_id().map(str::to_string),
        last_tool: Some("exec_command".into()),
        state: Some(state.into()),
        last_error,
        changed_files,
        history_checkpoint_ref: None,
        verification: Vec::new(),
    });
}

fn terminal_operation_kind(snapshot: &Value) -> &'static str {
    match snapshot
        .get("termination_reason")
        .and_then(Value::as_str)
        .unwrap_or("running")
    {
        "exited" if snapshot.get("command_ok").and_then(Value::as_bool) == Some(true) => "completed",
        "killed" => "cancelled",
        "running" => "running",
        _ => "failed",
    }
}

pub fn schedule_session_eviction(store: Arc<SessionStore>, session: Arc<ExecSession>) {
    if session
        .retention_scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let session_id = session.session_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(SESSION_RETAIN_FOR).await;
        store.remove(&session_id);
    });
}

pub fn spawn_session_monitor(ctx: Arc<ToolContext>, session: Arc<ExecSession>, deadline: Instant) {
    tauri::async_runtime::spawn(async move {
        loop {
            session.refresh_status().await;
            if session.has_exited() {
                session.wait_for_readers().await;
                let _ = finalize_session(ctx.as_ref(), &session);
                schedule_session_eviction(ctx.sessions.clone(), session.clone());
                return;
            }
            if Instant::now() >= deadline {
                session.mark_termination_reason("timeout");
                let _ = deliver_signal(&session, "KILL").await;
                let _ = wait_for_exit(&session, 1500).await;
                session.refresh_status().await;
                session.wait_for_readers().await;
                let _ = finalize_session(ctx.as_ref(), &session);
                schedule_session_eviction(ctx.sessions.clone(), session.clone());
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

struct SignalDelivery {
    effective: String,
    warning: Option<String>,
}

async fn wait_for_exit(session: &ExecSession, wait_ms: u64) -> bool {
    if wait_ms == 0 {
        session.refresh_status().await;
        return session.has_exited();
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        session.refresh_status().await;
        if session.has_exited() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn deliver_signal(session: &ExecSession, signal: &str) -> Result<SignalDelivery, WorkspaceError> {
    let pid = {
        let child = session.child.lock().await;
        child.id()
    }
    .ok_or_else(|| WorkspaceError::Tool {
        code: "SESSION_CLOSED",
        message: "Process id is no longer available.".into(),
        category: "runtime",
        retryable: false,
    })?;
    send_session_signal(pid, signal)
}

#[cfg(unix)]
fn send_session_signal(pid: u32, signal: &str) -> Result<SignalDelivery, WorkspaceError> {
    let sig = match signal {
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        _ => libc::SIGTERM,
    };
    let group_result = unsafe { libc::kill(-(pid as i32), sig) };
    let result = if group_result == 0 {
        0
    } else {
        unsafe { libc::kill(pid as i32, sig) }
    };
    if result == 0 {
        Ok(SignalDelivery {
            effective: signal.to_string(),
            warning: None,
        })
    } else {
        Err(WorkspaceError::Tool {
            code: "SIGNAL_DELIVERY_FAILED",
            message: format!("Failed to deliver {signal} to process tree {pid}"),
            category: "runtime",
            retryable: true,
        })
    }
}

#[cfg(windows)]
fn send_session_signal(pid: u32, signal: &str) -> Result<SignalDelivery, WorkspaceError> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    if signal == "INT" {
        return Err(WorkspaceError::Tool {
            code: "SIGNAL_UNSUPPORTED",
            message: "INT cannot be delivered reliably to hidden Windows process groups. Use TERM or KILL; WSL TTY applications should receive control characters through write_stdin.".into(),
            category: "runtime",
            retryable: false,
        });
    }

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new("taskkill.exe");
    command.args(["/PID", &pid.to_string(), "/T"]);
    if signal == "KILL" {
        command.arg("/F");
    }
    command.creation_flags(CREATE_NO_WINDOW);
    if command.status().is_ok_and(|status| status.success()) {
        return Ok(SignalDelivery {
            effective: signal.to_string(),
            warning: None,
        });
    }

    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let result = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
            if result.is_ok() {
                return Ok(SignalDelivery {
                    effective: "KILL".into(),
                    warning: Some(format!(
                        "{signal} process-tree delivery failed; escalated to TerminateProcess"
                    )),
                });
            }
        }
    }
    Err(WorkspaceError::Tool {
        code: "SIGNAL_DELIVERY_FAILED",
        message: format!("Failed to terminate Windows process tree {pid}"),
        category: "runtime",
        retryable: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paging_never_repeats_offset_after_retention_truncation() {
        let total = SESSION_BUFFER_BYTES + 1024;
        let retained_start = total - SESSION_BUFFER_BYTES;
        let requested = 0usize;
        let actual = requested.max(retained_start).min(total);
        let local = actual - retained_start;
        let chunk_len = 64usize.min(SESSION_BUFFER_BYTES - local);
        let next = actual + chunk_len;
        assert_eq!(actual, retained_start);
        assert!(next > actual);
        assert!(next <= total);
    }

    #[test]
    fn terminal_kind_distinguishes_transport_from_command_failure() {
        assert_eq!(
            terminal_operation_kind(&json!({
                "termination_reason": "exited",
                "command_ok": true
            })),
            "completed"
        );
        assert_eq!(
            terminal_operation_kind(&json!({
                "termination_reason": "exited",
                "command_ok": false
            })),
            "failed"
        );
        assert_eq!(
            terminal_operation_kind(&json!({
                "termination_reason": "killed",
                "command_ok": false
            })),
            "cancelled"
        );
    }
}
