use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::tools::context::ToolContext;
use crate::tools::session::{self, ExecSession};
use crate::tools::workspace::{tool_ok, WorkspaceError};

pub fn exec_command(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let cmd = args
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkspaceError::invalid_argument("cmd is required"))?;
    let workdir_raw = args
        .get("workdir")
        .or_else(|| args.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or(".");
    let workdir = ctx.workspace.resolve_existing(workdir_raw)?;
    if !workdir.path.is_dir() {
        return Err(WorkspaceError::not_a_directory(
            "workdir is not a directory",
        ));
    }
    let filesystem_scope = args
        .get("filesystem_scope")
        .and_then(Value::as_str)
        .unwrap_or("workspace")
        .to_string();
    validate_child_process_scope(ctx, args)?;

    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    let max_output = args
        .get("max_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(32_768) as usize;
    let yield_ms = args
        .get("yield_time_ms")
        .and_then(Value::as_u64)
        .unwrap_or(1000)
        .min(30_000);
    let tty = args.get("tty").and_then(Value::as_bool).unwrap_or(false);
    let stdin_text = args.get("stdin").and_then(Value::as_str).unwrap_or("");

    if !tty {
        if let Some(result) = run_native_diagnostic(ctx, cmd, &workdir.path)? {
            let mut result = result;
            if let Some(object) = result.as_object_mut() {
                object.insert(
                    "filesystem_scope".into(),
                    Value::String(filesystem_scope.clone()),
                );
                object.insert("sandbox_enforced".into(), Value::Bool(false));
                object.insert(
                    "execution_boundary".into(),
                    Value::String("policy_only".into()),
                );
                object.insert("child_process".into(), Value::Bool(false));
                object.insert("transport_ok".into(), Value::Bool(true));
                object.insert("command_ok".into(), Value::Bool(true));
                object.insert("interactive_requested".into(), Value::Bool(false));
                object.insert("interactive".into(), Value::Bool(false));
                object.insert("pty_attached".into(), Value::Bool(false));
                object.insert("stderr_merged".into(), Value::Bool(false));
            }
            return Ok(tool_ok(result));
        }
    }

    let result = tauri::async_runtime::block_on(async {
        run_command(
            ctx,
            cmd,
            &workdir.path,
            Duration::from_millis(timeout_ms),
            Duration::from_millis(yield_ms),
            max_output,
            tty,
            stdin_text,
        )
        .await
    });

    match result {
        Ok(mut out) => {
            if let Some(object) = out.as_object_mut() {
                object.insert("filesystem_scope".into(), Value::String(filesystem_scope));
                object.insert("sandbox_enforced".into(), Value::Bool(false));
                object.insert(
                    "execution_boundary".into(),
                    Value::String("policy_only".into()),
                );
                object.insert("child_process".into(), Value::Bool(true));
            }
            Ok(tool_ok(out))
        }
        Err(error) => match execution_failure_result(&error, cmd, &workdir.path) {
            Some(result) => Ok(tool_ok(result)),
            None => Err(error),
        },
    }
}

fn validate_child_process_scope(_ctx: &ToolContext, args: &Value) -> Result<(), WorkspaceError> {
    let scope = args
        .get("filesystem_scope")
        .and_then(Value::as_str)
        .unwrap_or("workspace");
    match scope {
        "workspace" => Ok(()),
        "host" => Err(WorkspaceError::ToolDetails {
            code: "EXTERNAL_EXECUTION_NOT_ALLOWED",
            message: "exec_command 只允许在 Workspace 内执行，Workspace 外执行已禁用。".into(),
            category: "permission",
            retryable: false,
            details: json!({
                "stage": "policy",
                "filesystem_scope": "host",
                "sandbox_enforced": false,
                "recoverable": false,
                "suggestion": "将 filesystem_scope 设置为 workspace，并在当前 Workspace 内执行"
            }),
        }),
        _ => Err(WorkspaceError::invalid_argument(
            "filesystem_scope must be workspace",
        )),
    }
}

fn run_native_diagnostic(
    ctx: &ToolContext,
    cmd: &str,
    cwd: &Path,
) -> Result<Option<Value>, WorkspaceError> {
    let parts = shell_words::split(cmd)
        .map_err(|_| WorkspaceError::invalid_argument("Invalid command syntax"))?;
    if parts.is_empty() {
        return Ok(None);
    }

    let command = parts[0].to_ascii_lowercase();
    let stdout = match command.as_str() {
        "pwd" if parts.len() == 1 => Some(format!("{}\n", cwd.display())),
        "ls" | "dir" => Some(list_directory(ctx, cwd, &parts[1..])?),
        "which" if parts.len() == 2 => {
            let search_path = ctx.executable_path_env();
            let path = which_on_path(&parts[1], cwd, search_path.as_deref()).ok_or_else(|| {
                WorkspaceError::Tool {
                    code: "COMMAND_NOT_FOUND",
                    message: format!("Program not found on PATH: {}", parts[1]),
                    category: "runtime",
                    retryable: false,
                }
            })?;
            Some(format!("{}\n", path.display()))
        }
        "echo" => Some(format!("{}\n", parts[1..].join(" "))),
        _ => None,
    };

    Ok(stdout.map(|stdout| {
        json!({
            "command": cmd,
            "resolved_cwd": cwd.display().to_string(),
            "status": "exited",
            "termination_reason": "exited",
            "recoverable": false,
            "suggestion": "命令已完成",
            "exit_code": 0,
            "stdout": stdout,
            "stderr": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "duration_ms": 0,
            "elapsed_ms": 0,
            "execution_mode": "native_builtin",
            "command_runner": "native_builtin",
            "warnings": ["native diagnostic without child process"]
        })
    }))
}

fn list_directory(
    ctx: &ToolContext,
    cwd: &Path,
    args: &[String],
) -> Result<String, WorkspaceError> {
    let target = match args {
        [] => cwd.to_path_buf(),
        [path] => ctx.workspace.resolve_existing(path)?.path,
        _ => {
            return Err(WorkspaceError::invalid_argument(
                "ls/dir accepts at most one directory path",
            ))
        }
    };
    if !target.is_dir() {
        return Err(WorkspaceError::not_a_directory(
            "ls/dir target is not a directory",
        ));
    }

    let mut entries = std::fs::read_dir(target)
        .map_err(|error| WorkspaceError::ToolDetails {
            code: "DIRECTORY_READ_FAILED",
            message: format!("Failed to read directory: {error}"),
            category: "runtime",
            retryable: true,
            details: json!({
                "stage": "native_builtin",
                "reason": "directory_read_failed",
                "retryable": true
            }),
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort_unstable();
    Ok(if entries.is_empty() {
        String::new()
    } else {
        format!("{}\n", entries.join("\n"))
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_command(
    ctx: &ToolContext,
    cmd: &str,
    cwd: &Path,
    limit: Duration,
    yield_time: Duration,
    max_output: usize,
    tty: bool,
    stdin_text: &str,
) -> Result<Value, WorkspaceError> {
    let search_path = ctx.executable_path_env();
    let (program, args) = parse_and_resolve(
        cmd,
        cwd,
        ctx.workspace.root(),
        &ctx.policy,
        search_path.as_deref(),
    )?;
    let start = Instant::now();
    let tracked_task = ctx
        .harness
        .current_task()
        .map_err(harness_runtime_error)?;
    let tracked_task_id = tracked_task.as_ref().map(|task| task.id.clone());
    let baseline_before = tracked_task
        .as_ref()
        .map(|task| ctx.harness.expected_baseline(&task.id))
        .transpose()
        .map_err(harness_runtime_error)?;

    let (mut command, pty_attached, stderr_merged) =
        command_for_execution(&program, &args, tty)?;
    if let Some(path) = search_path.as_ref() {
        command.env("PATH", path);
    }
    command
        .current_dir(platform_command_path(cwd))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    configure_process_group(&mut command);

    #[cfg(windows)]
    command
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONLEGACYWINDOWSSTDIO", "0");

    let child = command.spawn().map_err(|error| WorkspaceError::ToolDetails {
        code: "COMMAND_SPAWN_FAILED",
        message: format!("Failed to start command: {error}"),
        category: "runtime",
        retryable: true,
        details: json!({
            "termination_reason": "spawn_failed",
            "recoverable": true,
            "suggestion": "检查命令路径、权限和运行时环境后重试"
        }),
    })?;

    let session = ctx.sessions.insert(ExecSession::new_with_mode(
        child,
        tty,
        pty_attached,
        stderr_merged,
        tracked_task_id,
        baseline_before,
    ));
    session.spawn_readers().await;
    let deadline = start + limit;

    if !stdin_text.is_empty() {
        let mut stdin_guard = session.stdin.lock().await;
        if let Some(stdin) = stdin_guard.as_mut() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(stdin_text.as_bytes())
                .await
                .map_err(|_| WorkspaceError::Tool {
                    code: "SESSION_CLOSED",
                    message: "Failed to write stdin.".into(),
                    category: "runtime",
                    retryable: false,
                })?;
            let _ = stdin.flush().await;
            if !tty {
                let _ = stdin.shutdown().await;
                *stdin_guard = None;
                session.mark_stdin_closed();
            }
        }
    } else if !tty {
        let mut stdin_guard = session.stdin.lock().await;
        if let Some(stdin) = stdin_guard.as_mut() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.shutdown().await;
        }
        *stdin_guard = None;
        session.mark_stdin_closed();
    }

    if yield_time.is_zero() {
        session::spawn_session_monitor(
            ctx.sessions.clone(),
            ctx.harness.clone(),
            ctx.workspace.root().to_path_buf(),
            session.clone(),
            deadline,
        );
        return Ok(merge_exec_result(
            session.snapshot(max_output),
            start,
            cmd,
            cwd,
            true,
        ));
    }

    loop {
        session.refresh_status().await;
        if session.has_exited() {
            session.wait_for_readers().await;
            session::finalize_session(ctx, &session)?;
            let snapshot = session.snapshot(max_output);
            session::spawn_session_monitor(
                ctx.sessions.clone(),
                ctx.harness.clone(),
                ctx.workspace.root().to_path_buf(),
                session.clone(),
                Instant::now(),
            );
            return Ok(merge_exec_result(snapshot, start, cmd, cwd, false));
        }
        if Instant::now() >= deadline {
            session.kill_and_wait().await;
            session.mark_termination_reason("timeout");
            session.refresh_status().await;
            session.wait_for_readers().await;
            session::finalize_session(ctx, &session)?;
            let snapshot = session.snapshot(max_output);
            session::spawn_session_monitor(
                ctx.sessions.clone(),
                ctx.harness.clone(),
                ctx.workspace.root().to_path_buf(),
                session.clone(),
                Instant::now(),
            );
            return Err(WorkspaceError::ToolDetails {
                code: "TIMEOUT",
                message: "Command timed out.".into(),
                category: "runtime",
                retryable: true,
                details: json!({
                    "termination_reason": "timeout",
                    "recoverable": true,
                    "suggestion": "读取 output_refs，调整 timeout_ms 后重试",
                    "session": snapshot
                }),
            });
        }
        if Instant::now().saturating_duration_since(start) >= yield_time {
            session::spawn_session_monitor(
                ctx.sessions.clone(),
                ctx.harness.clone(),
                ctx.workspace.root().to_path_buf(),
                session.clone(),
                deadline,
            );
            let snapshot = session.snapshot(max_output);
            return Ok(merge_exec_result(snapshot, start, cmd, cwd, true));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn harness_runtime_error(error: crate::harness::HarnessError) -> WorkspaceError {
    WorkspaceError::Tool {
        code: "HARNESS_STATE_UNAVAILABLE",
        message: error.to_string(),
        category: "runtime",
        retryable: true,
    }
}

pub fn exec_health_check(ctx: &ToolContext) -> Result<Value, WorkspaceError> {
    let start = Instant::now();
    let cwd = ctx.workspace.root().to_path_buf();
    #[cfg(windows)]
    let probe = r#"cmd.exe /d /c "echo exec-health && echo exec-health-stderr 1>&2""#;
    #[cfg(not(windows))]
    let probe = r#"sh -c "printf exec-health; printf exec-health-stderr >&2""#;

    let result = tauri::async_runtime::block_on(run_command(
        ctx,
        probe,
        &cwd,
        Duration::from_secs(5),
        Duration::from_secs(5),
        16_384,
        false,
        "",
    ));

    let mut response = json!({
        "worker": {"alive": true},
        "session_create": false,
        "command_run": false,
        "stdout_capture": false,
        "stderr_capture": false,
        "duration_ms": start.elapsed().as_millis(),
        "next_actions": []
    });

    match result {
        Ok(snapshot) => {
            let session_created = snapshot.get("session_id").is_some();
            let command_run = snapshot.get("exit_code").and_then(Value::as_i64) == Some(0);
            let stdout_capture = snapshot
                .get("stdout")
                .and_then(Value::as_str)
                .is_some_and(|value| value.contains("exec-health"));
            let stderr_capture = snapshot
                .get("stderr")
                .and_then(Value::as_str)
                .is_some_and(|value| value.contains("exec-health-stderr"));
            let healthy = session_created && command_run && stdout_capture && stderr_capture;
            response["session_create"] = Value::Bool(session_created);
            response["command_run"] = Value::Bool(command_run);
            response["stdout_capture"] = Value::Bool(stdout_capture);
            response["stderr_capture"] = Value::Bool(stderr_capture);
            response["status"] = Value::String(if healthy { "success" } else { "error" }.into());
            response["summary"] = Value::String(if healthy {
                "exec worker、session、命令执行和 stdout/stderr 捕获均正常".into()
            } else {
                "exec health check 未通过，请查看 probe 结果".into()
            });
            response["probe"] = snapshot;
            if !healthy {
                response["next_actions"] = json!(["检查 exec worker 日志", "重启运行时"]);
            }
        }
        Err(error) => {
            response["status"] = Value::String("error".into());
            response["summary"] = Value::String("exec session 创建或探针执行失败".into());
            response["error"] = error.to_error_value();
            response["next_actions"] = json!(["检查 exec worker 日志", "重启运行时"]);
        }
    }
    response["duration_ms"] = json!(start.elapsed().as_millis());
    Ok(tool_ok(response))
}

fn execution_failure_result(error: &WorkspaceError, command: &str, cwd: &Path) -> Option<Value> {
    let code = match error {
        WorkspaceError::Tool { code, .. } | WorkspaceError::ToolDetails { code, .. } => *code,
    };
    if !matches!(
        code,
        "COMMAND_REJECTED" | "COMMAND_SPAWN_FAILED" | "TIMEOUT"
    ) {
        return None;
    }

    let error_value = error.to_error_value();
    let details = error_value
        .get("details")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut result = details.get("session").cloned().unwrap_or_else(|| {
        json!({
            "status": "spawn_failed",
            "termination_reason": "spawn_failed",
            "recoverable": error_value["retryable"].as_bool().unwrap_or(false),
            "exit_code": Value::Null,
            "stdout": "",
            "stderr": "",
            "stdout_truncated": false,
            "stderr_truncated": false
        })
    });
    if let Some(object) = result.as_object_mut() {
        object.insert("command".into(), json!(command));
        object.insert("resolved_cwd".into(), json!(cwd.display().to_string()));
        object.insert("execution_mode".into(), json!("direct"));
        object.insert("filesystem_scope".into(), json!("workspace"));
        object.insert("sandbox_enforced".into(), Value::Bool(false));
        object.insert("execution_boundary".into(), json!("policy_only"));
        object.insert("child_process".into(), Value::Bool(true));
        object.insert("transport_ok".into(), Value::Bool(true));
        object.insert("command_ok".into(), Value::Bool(false));
        object.insert("error".into(), error_value);
        if code == "TIMEOUT" {
            object.insert("termination_reason".into(), json!("timeout"));
        } else {
            object.insert("status".into(), json!("spawn_failed"));
            object.insert("termination_reason".into(), json!("spawn_failed"));
        }
    }
    Some(result)
}

fn merge_exec_result(
    mut snapshot: Value,
    start: Instant,
    command: &str,
    cwd: &Path,
    keep_session: bool,
) -> Value {
    if let Some(obj) = snapshot.as_object_mut() {
        let duration_ms = start.elapsed().as_millis();
        obj.insert("command".into(), json!(command));
        obj.insert("resolved_cwd".into(), json!(cwd.display().to_string()));
        obj.insert("duration_ms".into(), json!(duration_ms));
        obj.insert("elapsed_ms".into(), json!(duration_ms));
        obj.insert("transport_ok".into(), Value::Bool(true));
        let command_ok = match obj
            .get("termination_reason")
            .and_then(Value::as_str)
            .unwrap_or("running")
        {
            "exited" => obj
                .get("exit_code")
                .and_then(Value::as_i64)
                .map(|exit_code| exit_code == 0)
                .or(Some(false)),
            "running" => None,
            _ => Some(false),
        };
        obj.insert(
            "command_ok".into(),
            command_ok.map(Value::Bool).unwrap_or(Value::Null),
        );
        obj.insert("execution_mode".into(), json!("direct"));
        obj.insert(
            "warnings".into(),
            json!(if keep_session {
                vec!["session retained for read_output/write_stdin/kill_session"]
            } else {
                vec!["completed session retained temporarily for read_output"]
            }),
        );
    }
    snapshot
}

fn parse_and_resolve(
    cmd: &str,
    cwd: &Path,
    workspace_root: &Path,
    policy: &crate::tools::policy::PolicySettings,
    search_path: Option<&OsStr>,
) -> Result<(String, Vec<String>), WorkspaceError> {
    let parts = shell_words::split(cmd)
        .map_err(|_| WorkspaceError::invalid_argument("Invalid command syntax"))?;
    if parts.is_empty() {
        return Err(WorkspaceError::invalid_argument("Empty command"));
    }

    let program = resolve_program(&parts[0], cwd, workspace_root, policy, search_path)?;
    Ok((program, parts[1..].to_vec()))
}

fn resolve_program(
    raw: &str,
    cwd: &Path,
    workspace_root: &Path,
    policy: &crate::tools::policy::PolicySettings,
    search_path: Option<&OsStr>,
) -> Result<String, WorkspaceError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorkspaceError::invalid_argument("Empty program"));
    }

    let explicit_path = trimmed.contains(['/', '\\']);
    let candidate = if Path::new(trimmed).is_absolute() {
        Path::new(trimmed).to_path_buf()
    } else {
        cwd.join(trimmed)
    };
    if candidate.is_file() {
        let resolved = candidate.canonicalize().map_err(|_| WorkspaceError::Tool {
            code: "COMMAND_REJECTED",
            message: format!("Program not found: {trimmed}"),
            category: "runtime",
            retryable: false,
        })?;
        let canonical_workspace = workspace_root
            .canonicalize()
            .map_err(|_| WorkspaceError::Tool {
                code: "COMMAND_REJECTED",
                message: "Workspace root is unavailable".into(),
                category: "runtime",
                retryable: true,
            })?;
        if !resolved.starts_with(&canonical_workspace) {
            return Err(WorkspaceError::Tool {
                code: "EXECUTABLE_OUTSIDE_WORKSPACE",
                message: format!("Workspace 外可执行文件被拒绝: {trimmed}"),
                category: "security",
                retryable: false,
            });
        }
        let extension = resolved
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", value.to_ascii_lowercase()))
            .unwrap_or_default();
        if policy.workspace_local_entries
            && (extension.is_empty() || policy.workspace_script_extensions.contains(&extension))
        {
            return Ok(resolved.to_string_lossy().into_owned());
        }
        return Err(WorkspaceError::Tool {
            code: "COMMAND_REJECTED",
            message: format!("Workspace 本地入口未获允许: {trimmed}"),
            category: "policy",
            retryable: false,
        });
    }

    if explicit_path {
        return Err(WorkspaceError::Tool {
            code: "COMMAND_REJECTED",
            message: format!("Program not found: {trimmed}"),
            category: "runtime",
            retryable: false,
        });
    }

    which_on_path(trimmed, cwd, search_path)
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or_else(|| WorkspaceError::Tool {
            code: "COMMAND_REJECTED",
            message: format!("Program not found on PATH: {trimmed}"),
            category: "runtime",
            retryable: false,
        })
}

fn which_on_path(program: &str, cwd: &Path, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let Some(paths) = search_path else {
        return which::which(program).ok();
    };

    for directory in std::env::split_paths(paths) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        for candidate in executable_candidates(directory.join(program)) {
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn executable_candidates(candidate: PathBuf) -> Vec<PathBuf> {
    vec![candidate]
}

#[cfg(windows)]
fn executable_candidates(candidate: PathBuf) -> Vec<PathBuf> {
    if candidate.extension().is_some() {
        return vec![candidate];
    }
    let extensions = std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .filter_map(|item| {
                    let extension = item.trim().trim_start_matches('.');
                    (!extension.is_empty()).then(|| extension.to_ascii_lowercase())
                })
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| vec!["com".into(), "exe".into(), "bat".into(), "cmd".into()]);

    extensions
        .into_iter()
        .map(|extension| candidate.with_extension(extension))
        .collect()
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn command_for_execution(
    program: &str,
    args: &[String],
    tty: bool,
) -> Result<(Command, bool, bool), WorkspaceError> {
    if !tty {
        return Ok((command_for_program(program, args), false, false));
    }

    #[cfg(target_os = "linux")]
    {
        let script = which::which("script").map_err(|_| pty_unavailable(
            "Linux TTY execution requires util-linux 'script' on PATH.",
        ))?;
        let mut command = Command::new(script);
        command.args(["-q", "-f", "-e", "-c"]);
        command.arg(posix_command_line(program, args));
        command.arg("/dev/null");
        return Ok((command, true, true));
    }

    #[cfg(target_os = "macos")]
    {
        let script = which::which("script").map_err(|_| pty_unavailable(
            "macOS TTY execution requires /usr/bin/script.",
        ))?;
        let mut command = Command::new(script);
        command.arg("-q").arg("/dev/null").arg(program).args(args);
        return Ok((command, true, true));
    }

    #[cfg(windows)]
    {
        if is_wsl_program(program) {
            let wrapped_args = wsl_tty_args(program, args)?;
            return Ok((command_for_program(program, &wrapped_args), true, true));
        }
        return Err(pty_unavailable(
            "Native Windows ConPTY is not provided by this runtime. tty=true is supported for WSL --exec/-e commands; use tty=false for native Windows commands.",
        ));
    }

    #[allow(unreachable_code)]
    Err(pty_unavailable("TTY execution is not supported on this platform."))
}

fn pty_unavailable(message: &str) -> WorkspaceError {
    WorkspaceError::ToolDetails {
        code: "PTY_UNAVAILABLE",
        message: message.into(),
        category: "runtime",
        retryable: false,
        details: json!({
            "interactive_requested": true,
            "pty_attached": false,
            "suggestion": "Install the documented PTY backend or retry with tty=false; the runtime never reports a pipe as a TTY."
        }),
    }
}

fn posix_command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(posix_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn posix_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn is_wsl_program(program: &str) -> bool {
    Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("wsl"))
}

#[cfg(windows)]
fn wsl_tty_args(program: &str, args: &[String]) -> Result<Vec<String>, WorkspaceError> {
    let exec_index = args
        .iter()
        .position(|arg| arg == "-e" || arg == "--exec")
        .ok_or_else(|| pty_unavailable("WSL tty=true requires an explicit -e/--exec payload."))?;
    let payload = args.get(exec_index + 1..).unwrap_or_default();
    if payload.is_empty() {
        return Err(pty_unavailable("WSL -e/--exec requires a command payload."));
    }
    let prefix = &args[..exec_index];
    ensure_wsl_script_available(program, prefix)?;
    let command_line = posix_command_line(&payload[0], &payload[1..]);
    let mut wrapped = prefix.to_vec();
    wrapped.extend([
        "-e".into(),
        "script".into(),
        "-q".into(),
        "-f".into(),
        "-e".into(),
        "-c".into(),
        command_line,
        "/dev/null".into(),
    ]);
    Ok(wrapped)
}

#[cfg(windows)]
fn ensure_wsl_script_available(program: &str, prefix: &[String]) -> Result<(), WorkspaceError> {
    let mut probe = std::process::Command::new(program);
    probe.args(prefix).args([
        "-e",
        "sh",
        "-lc",
        "command -v script >/dev/null 2>&1",
    ]);
    probe.creation_flags(windows_hidden_creation_flags());
    if probe.status().is_ok_and(|status| status.success()) {
        Ok(())
    } else {
        Err(pty_unavailable(
            "The selected WSL distribution does not provide util-linux 'script'. Install util-linux or retry with tty=false.",
        ))
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

#[cfg(windows)]
fn windows_hidden_creation_flags() -> u32 {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
}

fn command_for_program(program: &str, args: &[String]) -> Command {
    #[cfg(windows)]
    {
        let extension = Path::new(program)
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("bat") | Some("cmd") => {
                let mut command = Command::new("cmd.exe");
                command.args(["/d", "/s", "/c"]);
                command
                    .as_std_mut()
                    .raw_arg(windows_batch_command_line(program, args));
                command.creation_flags(windows_hidden_creation_flags());
                return command;
            }
            Some("ps1") => {
                let shell = which::which("pwsh")
                    .or_else(|_| which::which("powershell"))
                    .unwrap_or_else(|_| PathBuf::from("powershell.exe"));
                let mut command = Command::new(shell);
                command
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        windows_command_path(program).as_str(),
                    ])
                    .args(args);
                command.creation_flags(windows_hidden_creation_flags());
                return command;
            }
            _ => {}
        }
    }

    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    command.creation_flags(windows_hidden_creation_flags());
    command
}

#[cfg(windows)]
fn windows_batch_command_line(program: &str, args: &[String]) -> String {
    let mut command_line = String::from("call ");
    command_line.push_str(&windows_batch_token(&windows_command_path(program)));
    for arg in args {
        command_line.push(' ');
        command_line.push_str(&windows_batch_token(arg));
    }
    command_line
}

#[cfg(windows)]
fn windows_batch_token(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn platform_command_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(windows_command_path(&path.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn windows_command_path(path: &str) -> String {
    path.strip_prefix("\\\\?\\").unwrap_or(path).to_string()
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::tools::context::ToolContext;
    use crate::tools::dispatch::call_tool;
    use serde_json::json;
    use tempfile::tempdir;

    fn assert_failure_result(error: WorkspaceError, expected_code: &str) {
        let result = execution_failure_result(&error, "missing-command", Path::new("C:/workspace"))
            .expect("应转换为统一执行结果");
        assert_eq!(result["transport_ok"], true);
        assert_eq!(result["command_ok"], false);
        assert_eq!(result["status"], "spawn_failed");
        assert_eq!(result["error"]["code"], expected_code);
    }

    #[cfg(unix)]
    #[test]
    fn configured_path_resolution_uses_declared_order() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().expect("root");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).expect("first dir");
        std::fs::create_dir_all(&second).expect("second dir");
        for directory in [&first, &second] {
            let executable = directory.join("path-probe");
            std::fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("probe");
            let mut permissions = std::fs::metadata(&executable).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions).expect("permissions");
        }
        let search_path = std::env::join_paths([&first, &second]).expect("join path");

        let resolved = which_on_path("path-probe", root.path(), Some(search_path.as_os_str()))
            .expect("resolved executable");

        assert_eq!(resolved, first.join("path-probe"));
    }

    #[test]
    fn 程序不存在时返回统一执行结果() {
        assert_failure_result(
            WorkspaceError::Tool {
                code: "COMMAND_REJECTED",
                message: "Program not found on PATH: missing-command".into(),
                category: "runtime",
                retryable: false,
            },
            "COMMAND_REJECTED",
        );
    }

    #[test]
    fn 启动失败时返回统一执行结果() {
        assert_failure_result(
            WorkspaceError::ToolDetails {
                code: "COMMAND_SPAWN_FAILED",
                message: "Failed to start command".into(),
                category: "runtime",
                retryable: true,
                details: json!({"recoverable": true}),
            },
            "COMMAND_SPAWN_FAILED",
        );
    }

    #[test]
    fn resolves_an_arbitrarily_named_workspace_local_entry() {
        let workspace = tempdir().expect("workspace");
        let entry = workspace.path().join("scripts").join("anything.cmd");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("scripts");
        std::fs::write(&entry, "echo test").expect("entry");
        let resolved = resolve_program(
            "scripts/anything.cmd",
            workspace.path(),
            workspace.path(),
            &crate::tools::policy::PolicySettings::default(),
            None,
        )
        .expect("workspace entry resolves");
        assert_eq!(Path::new(&resolved), entry.canonicalize().unwrap());
    }

    #[test]
    fn posix_quoting_preserves_single_quotes_and_spaces() {
        assert_eq!(posix_quote("a b"), "'a b'");
        assert_eq!(posix_quote("a'b"), "'a'\"'\"'b'");
    }

    #[cfg(windows)]
    #[test]
    fn windows_hidden_creation_flags_match_frpc_no_window_pattern() {
        assert_eq!(
            windows_hidden_creation_flags(),
            0x0000_0200 | 0x0800_0000,
            "must keep CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_scripts_use_their_platform_runners() {
        let batch = command_for_program("C:/workspace/run-anything.cmd", &[]);
        assert_eq!(batch.as_std().get_program().to_string_lossy(), "cmd.exe");
        assert!(batch.as_std().get_args().any(|arg| arg == "/c"));
        assert_eq!(
            windows_batch_command_line(
                r"\\?\C:\workspace\Life Brain\run & tooling.cmd",
                &["argument & value".to_string()]
            ),
            r#"call "C:\workspace\Life Brain\run & tooling.cmd" "argument & value""#
        );

        let script = command_for_program("C:/workspace/run-anything.ps1", &[]);
        let runner = script
            .as_std()
            .get_program()
            .to_string_lossy()
            .to_ascii_lowercase();
        assert!(runner.contains("powershell") || runner.contains("pwsh"));
        assert!(script.as_std().get_args().any(|arg| arg == "-File"));

        let python = command_for_program("C:/Python312/python.exe", &["-c".into(), "print(1)".into()]);
        assert_eq!(
            python.as_std().get_program().to_string_lossy(),
            "C:/Python312/python.exe"
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_tty_is_rejected_instead_of_faked() {
        let error = command_for_execution("C:/Python312/python.exe", &[], true)
            .expect_err("native pipe must not be reported as PTY");
        assert_eq!(error.code(), "PTY_UNAVAILABLE");
    }

    #[cfg(windows)]
    #[test]
    fn windows_workspace_scripts_and_python_unicode_execute_successfully() {
        let workspace = tempdir().expect("workspace");
        let harness = tempdir().expect("harness");
        std::fs::write(
            workspace.path().join("any-name.cmd"),
            "@echo tooling-cmd-ok\r\n",
        )
        .expect("cmd script");
        std::fs::write(
            workspace.path().join("any-name.ps1"),
            "Write-Output 'tooling-powershell-ok'\r\n",
        )
        .expect("powershell script");
        std::fs::write(
            workspace.path().join("workflow_probe.py"),
            "print('workflow-ok')\n",
        )
        .expect("python module");
        let ctx = ToolContext::for_test(workspace.path().to_path_buf(), harness.path().to_path_buf())
            .expect("context");

        for command in [
            "any-name.cmd",
            "any-name.ps1",
            "cmd /c echo tooling-cmd-ok",
            "powershell -NoProfile -Command \"Write-Output tooling-powershell-ok\"",
            "python -c \"print('中文输出正常 ✅')\"",
        ] {
            let output = call_tool(
                &ctx,
                "exec_command",
                &json!({ "cmd": command, "timeout_ms": 10_000, "yield_time_ms": 10_000 }),
            );
            assert_eq!(output["ok"], true, "{command}: {output}");
            assert_eq!(output["command_ok"], true, "{command}: {output}");
        }

        for _ in 0..10 {
            let output = call_tool(
                &ctx,
                "exec_command",
                &json!({ "cmd": "python -m workflow_probe", "timeout_ms": 10_000 }),
            );
            assert_eq!(output["command_ok"], true, "{output}");
            assert!(output["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("workflow-ok"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_batch_scripts_preserve_space_paths_and_arguments() {
        let parent = tempdir().expect("workspace parent");
        let workspace = parent.path().join("Life Brain 中文");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let harness = tempdir().expect("harness");
        let ctx = ToolContext::for_test(workspace.clone(), harness.path().to_path_buf())
            .expect("context");

        for extension in ["cmd", "bat"] {
            let script_name = format!("run & tooling.{extension}");
            std::fs::write(
                workspace.join(&script_name),
                "@echo off\r\nif not \"%~1\"==\"argument & value\" exit /b 7\r\necho tooling-space-path-ok\r\n",
            )
            .expect("batch script");

            let command = format!(r#""{script_name}" "argument & value""#);
            let output = call_tool(
                &ctx,
                "exec_command",
                &json!({ "cmd": command, "timeout_ms": 10_000, "yield_time_ms": 10_000 }),
            );
            assert_eq!(output["command_ok"], true, "{script_name}: {output}");
            let stdout = output["stdout"].as_str().unwrap_or_default();
            assert!(stdout.contains("tooling-space-path-ok"), "{output}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_workspace_scripts_preserve_space_paths_and_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempdir().expect("workspace parent");
        let workspace = parent.path().join("Life Brain 中文");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let harness = tempdir().expect("harness");
        let script_name = "run tooling";
        let script_path = workspace.join(script_name);
        std::fs::write(
            &script_path,
            "#!/bin/sh\nprintf 'tooling-space-path-ok\\n'\nprintf 'argument=[%s]\\n' \"$1\"\n",
        )
        .expect("shell script");
        let mut permissions = std::fs::metadata(&script_path)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).expect("script executable");

        let ctx = ToolContext::for_test(workspace, harness.path().to_path_buf()).expect("context");
        let command = format!(r#""{script_name}" "argument with spaces""#);
        let output = call_tool(
            &ctx,
            "exec_command",
            &json!({ "cmd": command, "timeout_ms": 10_000, "yield_time_ms": 10_000 }),
        );
        assert_eq!(output["command_ok"], true, "{output}");
        let stdout = output["stdout"].as_str().unwrap_or_default();
        assert!(stdout.contains("tooling-space-path-ok"), "{output}");
        assert!(stdout.contains("argument=[argument with spaces]"), "{output}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_tty_is_a_real_pty() {
        if which::which("script").is_err() || which::which("python3").is_err() {
            return;
        }
        let workspace = tempdir().expect("workspace");
        let harness = tempdir().expect("harness");
        let ctx = ToolContext::for_test(workspace.path().to_path_buf(), harness.path().to_path_buf())
            .expect("context");
        let output = call_tool(
            &ctx,
            "exec_command",
            &json!({
                "cmd": "python3 -c \"import sys; print(sys.stdin.isatty(), sys.stdout.isatty(), sys.stderr.isatty())\"",
                "tty": true,
                "yield_time_ms": 5000,
                "timeout_ms": 10000
            }),
        );
        assert_eq!(output["command_ok"], true, "{output}");
        assert_eq!(output["pty_attached"], true, "{output}");
        let stdout = output["stdout"].as_str().unwrap_or_default();
        assert!(stdout.contains("True True True"), "{output}");
    }

    #[cfg(unix)]
    #[test]
    fn active_task_long_command_yields_and_completed_output_remains_readable() {
        if which::which("python3").is_err() {
            return;
        }
        let workspace = tempdir().expect("workspace");
        let harness_root = tempdir().expect("harness");
        let ctx = ToolContext::for_test(
            workspace.path().to_path_buf(),
            harness_root.path().to_path_buf(),
        )
        .expect("context");
        ctx.harness.start_task("long command").expect("task");
        let started = Instant::now();
        let output = call_tool(
            &ctx,
            "exec_command",
            &json!({
                "cmd": "python3 -c \"import time; print('start', flush=True); time.sleep(0.4); print('done')\"",
                "yield_time_ms": 30,
                "timeout_ms": 5000
            }),
        );
        assert_eq!(output["status"], "running", "{output}");
        assert!(started.elapsed() < Duration::from_millis(1000), "{output}");
        let session_id = output["session_id"].as_str().expect("session id");
        std::thread::sleep(Duration::from_millis(600));
        let polled = call_tool(
            &ctx,
            "write_stdin",
            &json!({"session_id": session_id, "yield_time_ms": 0}),
        );
        assert_eq!(polled["command_ok"], true, "{polled}");
        let output_ref = output["output_refs"]["stdout"].as_str().expect("output ref");
        let retained = call_tool(
            &ctx,
            "read_output",
            &json!({"output_ref": output_ref, "offset": 0, "limit": 4096}),
        );
        assert_eq!(retained["ok"], true, "{retained}");
        assert!(retained["content"].as_str().unwrap_or_default().contains("done"));
    }
}
