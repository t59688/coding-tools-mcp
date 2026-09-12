use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::model::{ProjectBaseline, TaskSession};
use super::state::capture_baseline;
use super::store::{HarnessError, HarnessResult, HarnessStore};
use super::Harness;

/// Persist a user-reviewed worktree as the task's new expected state.
///
/// The immutable task-start baseline (branch, HEAD, and original entries) is never
/// rewritten. Callers may provide the exact worktree fingerprint they reviewed;
/// when present, the current worktree must still match it at the moment we persist
/// the acceptance. This turns the fingerprint into an optimistic-concurrency token
/// and prevents a later, unreviewed edit from being silently blessed.
pub fn accept_reviewed_baseline(
    harness: &Harness,
    workspace_root: &Path,
    task_id: &str,
    reviewed_worktree_fingerprint: Option<&str>,
) -> HarnessResult<(TaskSession, ProjectBaseline)> {
    let mut task = harness.task(task_id)?;
    let current = capture_baseline(workspace_root);

    if current.branch != task.baseline.branch || current.head != task.baseline.head {
        return Err(HarnessError::new(
            "BASELINE_STALE",
            "Git 分支或 HEAD 已发生变化；不能通过工作区基线恢复静默接受新的 Git revision",
        ));
    }

    if let Some(reviewed) = reviewed_worktree_fingerprint {
        let reviewed = reviewed.trim();
        if reviewed.is_empty() {
            return Err(HarnessError::new(
                "INVALID_ARGUMENT",
                "reviewed worktree fingerprint 不能为空",
            ));
        }
        if current.worktree_fingerprint != reviewed {
            return Err(HarnessError::new(
                "FILE_CHANGED_EXTERNALLY",
                "工作区在审查后再次变化；请重新读取 project_state/git_diff 并使用新的 worktree fingerprint",
            ));
        }
    }

    task.expected_fingerprint = current.worktree_fingerprint.clone();
    task.updated_at = timestamp();
    HarnessStore::new(harness.store_root().to_path_buf())?.save_task(&task)?;

    Ok((task, current))
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".into())
}
