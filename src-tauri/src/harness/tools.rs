use serde_json::{json, Value};

use crate::tools::workspace::{tool_ok, WorkspaceError};
use crate::tools::ToolContext;

use super::model::TaskStatus;
use super::recovery::accept_reviewed_baseline;
use super::state::capture_baseline;
use super::store::HarnessError;

pub const TOOL_NAMES: &[&str] = &[
    "harness_status",
    "operation_log",
    "project_state",
    "start_task",
    "update_task",
    "pause_task",
    "resume_task",
    "refresh_baseline",
    "finish_task",
    "task_context",
    "list_task_events",
    "change_summary",
];

pub fn call(ctx: &ToolContext, name: &str, args: &Value) -> Result<Value, WorkspaceError> {
    let value = match name {
        "harness_status" => harness_status(ctx),
        "operation_log" => operation_log(ctx, args),
        "project_state" => project_state(ctx, args),
        "start_task" => start_task(ctx, args),
        "update_task" => update_task(ctx, args),
        "pause_task" => transition(ctx, args, TaskStatus::Paused),
        "resume_task" => resume_task(ctx, args),
        "refresh_baseline" => refresh_baseline(ctx, args),
        "finish_task" => finish_task(ctx, args),
        "task_context" => task_context(ctx, args),
        "list_task_events" => list_task_events(ctx, args),
        "change_summary" => change_summary(ctx, args),
        _ => return Err(tool_error("INVALID_ARGUMENT", "未知 Harness 工具")),
    }?;
    Ok(tool_ok(value))
}

fn harness_status(ctx: &ToolContext) -> Result<Value, WorkspaceError> {
    let mut status = ctx.harness.status().map_err(map_error)?;
    status.next_actions = status
        .next_actions
        .into_iter()
        .map(|action| match action.as_str() {
            "start_task" => "task_manage:start".to_string(),
            "project_state" => "task_manage:project_state".to_string(),
            "resume_task" => "task_manage:resume".to_string(),
            "refresh_baseline" => "task_manage:refresh_baseline".to_string(),
            other => other.to_string(),
        })
        .collect();

    let recovery = if status.baseline_matches == Some(false) {
        ctx.harness
            .current_task()
            .map_err(map_error)?
            .map(|task| {
                let current = capture_baseline(ctx.workspace.root());
                if current.branch != task.baseline.branch || current.head != task.baseline.head {
                    json!({
                        "type": "git_revision_drift",
                        "recoverable_via_baseline_acceptance": false,
                        "preferred_action": "task_manage:project_state",
                        "message": "Git branch/HEAD 已变化；必须先审查 revision，refresh_baseline/resume 兼容恢复不会接受该变化"
                    })
                } else {
                    json!({
                        "type": "reviewed_worktree_baseline",
                        "recoverable_via_baseline_acceptance": true,
                        "preferred_action": "task_manage:refresh_baseline",
                        "compatibility_action": "task_manage:resume",
                        "task_id": task.id,
                        "change_id": current.worktree_fingerprint,
                        "requires_review": true,
                        "requires_summary": true,
                        "message": "先审查 project_state/git_diff。若客户端缓存的 task_manage schema 尚未包含 refresh_baseline，可调用 resume 并同时传 task_id、这里的 change_id 与非空 summary；服务端仅在 worktree 仍精确匹配该 fingerprint 时接受恢复。"
                    })
                }
            })
    } else {
        None
    };

    let mut value = serde_json::to_value(status)
        .map_err(|e| tool_error("SERIALIZE_FAILED", e.to_string()))?;
    if let (Some(object), Some(recovery)) = (value.as_object_mut(), recovery) {
        object.insert("recovery".into(), recovery);
    }
    Ok(value)
}

fn operation_log(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let offset = args.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let operations = ctx
        .harness
        .list_operations(offset, limit)
        .map_err(map_error)?;
    Ok(json!({
        "operations": operations,
        "next_cursor": offset + operations.len()
    }))
}

fn project_state(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let max_files = args.get("max_files").and_then(Value::as_u64).unwrap_or(200) as usize;
    serde_json::to_value(ctx.harness.project_state(max_files).map_err(map_error)?)
        .map_err(|e| tool_error("SERIALIZE_FAILED", e.to_string()))
}

fn start_task(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let objective = args
        .get("objective")
        .and_then(Value::as_str)
        .ok_or_else(|| tool_error("INVALID_ARGUMENT", "objective 是必填项"))?;
    let task = ctx.harness.start_task(objective).map_err(map_error)?;
    Ok(json!({"task": task, "next": ["task_manage:project_state", "task_manage:context"]}))
}

fn update_task(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let task_id = task_id(args)?;
    let completed_steps = string_list(args.get("completed_steps"))?;
    let pending_steps = string_list(args.get("pending_steps"))?;
    let task = ctx
        .harness
        .update_steps(task_id, completed_steps, pending_steps)
        .map_err(map_error)?;
    Ok(json!({"task": task}))
}

fn transition(
    ctx: &ToolContext,
    args: &Value,
    status: TaskStatus,
) -> Result<Value, WorkspaceError> {
    let task = ctx
        .harness
        .transition(task_id(args)?, status)
        .map_err(map_error)?;
    Ok(json!({"task": task}))
}

/// Resume normally when the baseline still matches. For stale clients whose cached
/// `task_manage` schema predates the explicit `refresh_baseline` action, `resume`
/// also provides a compatibility recovery path using fields that already existed in
/// the v2 schema: `change_id` carries the reviewed worktree fingerprint and `summary`
/// records why that exact state is being accepted.
fn resume_task(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let requested_task_id = task_id(args)?;
    let Some(reviewed_fingerprint) = args.get("change_id").and_then(Value::as_str) else {
        return transition(ctx, args, TaskStatus::Active);
    };
    let reviewed_fingerprint = reviewed_fingerprint.trim();
    if reviewed_fingerprint.is_empty() {
        return Err(tool_error(
            "INVALID_ARGUMENT",
            "兼容基线恢复的 change_id 必须是非空 worktree fingerprint",
        ));
    }
    let review_summary = args
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            tool_error(
                "BASELINE_REVIEW_REQUIRED",
                "兼容基线恢复必须提供非空 summary，说明已审查当前 project_state/git_diff",
            )
        })?;

    let active_task = ctx
        .harness
        .current_task()
        .map_err(map_error)?
        .ok_or_else(|| tool_error("TASK_STATE_REQUIRED", "当前没有可恢复的活动任务"))?;
    if active_task.id != requested_task_id {
        return Err(tool_error(
            "TASK_STATE_REQUIRED",
            "只能恢复当前活动任务的工作区基线",
        ));
    }
    if !matches!(
        active_task.status,
        TaskStatus::Active | TaskStatus::Paused | TaskStatus::Failed
    ) {
        return Err(tool_error(
            "INVALID_TASK_TRANSITION",
            "当前任务状态不能通过 resume 兼容路径恢复为 active",
        ));
    }

    let previous_expected_fingerprint = active_task.expected_fingerprint.clone();
    let (accepted_task, current) = accept_reviewed_baseline(
        &ctx.harness,
        ctx.workspace.root(),
        requested_task_id,
        Some(reviewed_fingerprint),
    )
    .map_err(map_error)?;
    let event = ctx
        .harness
        .record_event(
            requested_task_id,
            "task_baseline_refreshed",
            Some("task_manage:resume"),
            json!({
                "source": "task_manage:resume",
                "compatibility_mode": true,
                "review_summary": review_summary,
                "reviewed_worktree_fingerprint": reviewed_fingerprint
            }),
            json!({
                "ok": true,
                "previous_expected_fingerprint": previous_expected_fingerprint.clone(),
                "expected_fingerprint": accepted_task.expected_fingerprint.clone(),
                "branch": current.branch.clone(),
                "head": current.head.clone()
            }),
        )
        .map_err(map_error)?;

    let task = if accepted_task.status == TaskStatus::Active {
        accepted_task
    } else {
        ctx.harness
            .transition(requested_task_id, TaskStatus::Active)
            .map_err(map_error)?
    };
    let status = harness_status(ctx)?;

    Ok(json!({
        "task": task,
        "harness": status,
        "summary": "已接受审查过的工作区 fingerprint 并恢复任务",
        "diagnostics": {
            "baseline_refreshed": true,
            "compatibility_mode": true,
            "previous_expected_fingerprint": previous_expected_fingerprint,
            "accepted_worktree_fingerprint": current.worktree_fingerprint,
            "event_id": event.id
        },
        "warnings": [
            "这是针对缓存旧 task_manage schema 的兼容恢复路径；能调用 refresh_baseline 的客户端应优先使用显式恢复 action"
        ]
    }))
}

/// Accept the currently reviewed worktree as the task's expected state without
/// rewriting the immutable task-start baseline. Branch/HEAD changes are never
/// blessed by this recovery action. When `change_id` is supplied it is treated as
/// the exact reviewed worktree fingerprint and provides optimistic concurrency.
fn refresh_baseline(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let requested_task_id = task_id(args)?;
    let active_task = ctx
        .harness
        .current_task()
        .map_err(map_error)?
        .ok_or_else(|| tool_error("TASK_STATE_REQUIRED", "当前没有可刷新基线的活动任务"))?;
    if active_task.id != requested_task_id {
        return Err(tool_error(
            "TASK_STATE_REQUIRED",
            "只能刷新当前活动任务的工作区基线",
        ));
    }

    let reviewed_fingerprint = match args.get("change_id").and_then(Value::as_str) {
        Some(value) if value.trim().is_empty() => {
            return Err(tool_error("INVALID_ARGUMENT", "change_id 不能为空"))
        }
        Some(value) => Some(value.trim()),
        None => None,
    };
    let review_summary = args
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let previous_expected_fingerprint = active_task.expected_fingerprint.clone();
    let (refreshed, current) = accept_reviewed_baseline(
        &ctx.harness,
        ctx.workspace.root(),
        requested_task_id,
        reviewed_fingerprint,
    )
    .map_err(map_error)?;
    let expected_fingerprint = refreshed.expected_fingerprint.clone();
    let event = ctx
        .harness
        .record_event(
            requested_task_id,
            "task_baseline_refreshed",
            Some("task_manage:refresh_baseline"),
            json!({
                "source": "task_manage:refresh_baseline",
                "review_summary": review_summary,
                "reviewed_worktree_fingerprint": reviewed_fingerprint
            }),
            json!({
                "ok": true,
                "previous_expected_fingerprint": previous_expected_fingerprint.clone(),
                "expected_fingerprint": expected_fingerprint.clone(),
                "branch": current.branch.clone(),
                "head": current.head.clone()
            }),
        )
        .map_err(map_error)?;
    let status = harness_status(ctx)?;

    Ok(json!({
        "task": refreshed,
        "harness": status,
        "summary": "当前已审查工作区已接受为任务预期状态",
        "diagnostics": {
            "baseline_refreshed": true,
            "previous_expected_fingerprint": previous_expected_fingerprint,
            "accepted_worktree_fingerprint": current.worktree_fingerprint,
            "event_id": event.id
        },
        "warnings": ["原始 task.baseline 保持不变；Git branch/HEAD 漂移不会被该操作接受"]
    }))
}

fn finish_task(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let task_id = task_id(args)?;
    let allow_unverified = args
        .get("allow_unverified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = if allow_unverified {
        TaskStatus::CompletedUnverified
    } else {
        TaskStatus::Verifying
    };
    let task = ctx.harness.transition(task_id, status).map_err(map_error)?;
    let summary = change_summary(ctx, &json!({"task_id": task_id}))?;
    Ok(json!({"task": task, "change_summary": summary}))
}

fn task_context(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let task = if let Some(task_id) = args.get("task_id").and_then(Value::as_str) {
        Some(ctx.harness.task(task_id).map_err(map_error)?)
    } else {
        ctx.harness.current_task().map_err(map_error)?
    };
    let Some(task) = task else {
        return Ok(json!({"task": null, "message": "当前没有活动任务"}));
    };
    let events = ctx
        .harness
        .list_events(&task.id, 0, 100)
        .map_err(map_error)?;
    Ok(json!({"task": task, "events": events, "truncated": false}))
}

fn list_task_events(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let task_id = task_id(args)?;
    let offset = args.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let events = ctx
        .harness
        .list_events(task_id, offset, limit)
        .map_err(map_error)?;
    Ok(json!({"events": events, "next_cursor": offset + events.len()}))
}

fn change_summary(ctx: &ToolContext, args: &Value) -> Result<Value, WorkspaceError> {
    let task = if let Some(task_id) = args.get("task_id").and_then(Value::as_str) {
        ctx.harness.task(task_id).map_err(map_error)?
    } else {
        ctx.harness
            .current_task()
            .map_err(map_error)?
            .ok_or_else(|| tool_error("TASK_STATE_REQUIRED", "没有可总结的活动任务"))?
    };
    let state = ctx.harness.project_state(200).map_err(map_error)?;
    let files = state
        .files
        .iter()
        .filter(|file| file.status != "unchanged")
        .cloned()
        .collect::<Vec<_>>();
    let events = ctx
        .harness
        .list_events(&task.id, 0, 100)
        .map_err(map_error)?;
    Ok(json!({
        "task_id": task.id,
        "objective": task.objective,
        "why": {"text": task.objective, "source": "task_objective"},
        "files": files,
        "evidence": events,
        "verification": [],
        "risks": [],
        "rollback_capability": "not_available_in_foundation"
    }))
}

fn task_id(args: &Value) -> Result<&str, WorkspaceError> {
    args.get("task_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| tool_error("INVALID_ARGUMENT", "task_id 是必填项"))
}

fn string_list(value: Option<&Value>) -> Result<Option<Vec<String>>, WorkspaceError> {
    let Some(value) = value else { return Ok(None) };
    let list = value
        .as_array()
        .ok_or_else(|| tool_error("INVALID_ARGUMENT", "步骤必须是字符串数组"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| tool_error("INVALID_ARGUMENT", "步骤必须是字符串数组"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(list))
}

fn map_error(error: HarnessError) -> WorkspaceError {
    tool_error(error.code(), error.to_string())
}

fn tool_error(code: &'static str, message: impl Into<String>) -> WorkspaceError {
    WorkspaceError::Tool {
        code,
        message: message.into(),
        category: "permission",
        retryable: matches!(
            code,
            "TASK_ALREADY_ACTIVE"
                | "FILE_CHANGED_EXTERNALLY"
                | "BASELINE_STALE"
                | "BASELINE_REVIEW_REQUIRED"
        ),
    }
}
