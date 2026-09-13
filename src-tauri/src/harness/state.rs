use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

use super::model::{
    BaselineEntry, CapabilityStatus, FileChangeRecord, HarnessEvent, HarnessStatus, OperationRecord,
    ProjectBaseline, ProjectFileState, ProjectState, TaskSession, TaskStatus, WorkspaceHarnessState,
    SCHEMA_VERSION,
};
use super::store::{HarnessError, HarnessResult, HarnessStore};
use crate::tools::workspace::DEFAULT_EXCLUDED_NAMES;

#[derive(Debug, Clone)]
pub struct Harness {
    workspace_root: PathBuf,
    workspace_id: String,
    store: HarnessStore,
}

impl Harness {
    pub fn new(workspace_root: PathBuf, harness_root: PathBuf) -> HarnessResult<Self> {
        let workspace_root = workspace_root
            .canonicalize()
            .map_err(|e| HarnessError::new("WORKSPACE_UNAVAILABLE", e.to_string()))?;
        let workspace_id = workspace_id(&workspace_root);
        Ok(Self {
            workspace_root,
            workspace_id,
            store: HarnessStore::new(harness_root)?,
        })
    }

    pub fn default_root() -> HarnessResult<PathBuf> {
        let root = dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .ok_or_else(|| HarnessError::new("STORE_UNAVAILABLE", "无法确定应用数据目录"))?;
        Ok(root.join("coding-tools-mcp").join("harness"))
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn store_root(&self) -> &Path {
        self.store.root()
    }

    pub fn start_task(&self, objective: &str) -> HarnessResult<TaskSession> {
        if objective.trim().is_empty() {
            return Err(HarnessError::new("INVALID_ARGUMENT", "任务目标不能为空"));
        }
        if let Some(task) = self.current_task()? {
            return Err(HarnessError::new(
                "TASK_ALREADY_ACTIVE",
                format!("工作区已有活动任务 {}", task.id),
            ));
        }
        let baseline = capture_baseline_incremental(&self.workspace_root, None);
        let now = timestamp();
        let task = TaskSession {
            id: Uuid::new_v4().simple().to_string(),
            workspace_id: self.workspace_id.clone(),
            objective: objective.trim().to_string(),
            status: TaskStatus::Active,
            expected_fingerprint: baseline.worktree_fingerprint.clone(),
            expected_baseline: Some(baseline.clone()),
            baseline,
            completed_steps: Vec::new(),
            pending_steps: Vec::new(),
            latest_change_id: None,
            latest_verification_id: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.save_task(&task)?;
        self.save_workspace_state(Some(&task.id), &task.updated_at)?;
        self.record_event(
            &task.id,
            "task_started",
            None,
            json!({}),
            json!({"ok": true}),
        )?;
        Ok(task)
    }

    pub fn current_task(&self) -> HarnessResult<Option<TaskSession>> {
        Ok(self
            .store
            .list_tasks(&self.workspace_id)?
            .into_iter()
            .find(|task| task.status.is_open()))
    }

    pub fn task(&self, task_id: &str) -> HarnessResult<TaskSession> {
        self.store.load_task(&self.workspace_id, task_id)
    }

    pub fn transition(&self, task_id: &str, next: TaskStatus) -> HarnessResult<TaskSession> {
        let mut task = self.task(task_id)?;
        if !task.status.can_transition_to(next) {
            return Err(HarnessError::new(
                "INVALID_TASK_TRANSITION",
                format!("不允许从 {:?} 转换到 {:?}", task.status, next),
            ));
        }
        task.status = next;
        task.updated_at = timestamp();
        self.store.save_task(&task)?;
        if !task.status.is_open() {
            self.save_workspace_state(None, &task.updated_at)?;
        }
        self.record_event(
            task_id,
            "task_status_changed",
            None,
            json!({"status": next}),
            json!({"ok": true}),
        )?;
        Ok(task)
    }

    pub fn update_steps(
        &self,
        task_id: &str,
        completed_steps: Option<Vec<String>>,
        pending_steps: Option<Vec<String>>,
    ) -> HarnessResult<TaskSession> {
        let mut task = self.task(task_id)?;
        if let Some(steps) = completed_steps {
            task.completed_steps = steps;
        }
        if let Some(steps) = pending_steps {
            task.pending_steps = steps;
        }
        task.updated_at = timestamp();
        self.store.save_task(&task)?;
        self.record_event(
            task_id,
            "task_updated",
            None,
            json!({
                "completed_steps": task.completed_steps,
                "pending_steps": task.pending_steps
            }),
            json!({"ok": true}),
        )?;
        Ok(task)
    }

    pub fn check_baseline(&self, task_id: &str) -> HarnessResult<()> {
        let task = self.task(task_id)?;
        let expected = task.expected_baseline.as_ref().unwrap_or(&task.baseline);
        let current = capture_baseline_incremental(&self.workspace_root, Some(expected));
        if current.branch != task.baseline.branch {
            return Err(HarnessError::new("BASELINE_STALE", "Git 分支已发生变化"));
        }
        if current.worktree_fingerprint != task.expected_fingerprint {
            return Err(HarnessError::new(
                "FILE_CHANGED_EXTERNALLY",
                "工作区存在 Harness 未记录的外部文件变化",
            ));
        }
        Ok(())
    }

    pub fn expected_baseline(&self, task_id: &str) -> HarnessResult<ProjectBaseline> {
        let task = self.task(task_id)?;
        Ok(task.expected_baseline.unwrap_or(task.baseline))
    }

    pub fn capture_current_baseline(&self, previous: Option<&ProjectBaseline>) -> ProjectBaseline {
        capture_baseline_incremental(&self.workspace_root, previous)
    }

    pub fn refresh_expected_state(&self, task_id: &str) -> HarnessResult<TaskSession> {
        let mut task = self.task(task_id)?;
        let previous = task.expected_baseline.as_ref().unwrap_or(&task.baseline);
        let current = capture_baseline_incremental(&self.workspace_root, Some(previous));
        task.expected_fingerprint = current.worktree_fingerprint.clone();
        task.expected_baseline = Some(current);
        task.updated_at = timestamp();
        self.store.save_task(&task)?;
        Ok(task)
    }

    pub fn record_event(
        &self,
        task_id: &str,
        kind: &str,
        tool_name: Option<&str>,
        input_summary: serde_json::Value,
        result_summary: serde_json::Value,
    ) -> HarnessResult<HarnessEvent> {
        let affected_files = affected_files_from_value(&result_summary);
        let event = HarnessEvent {
            id: Uuid::new_v4().simple().to_string(),
            task_id: task_id.to_string(),
            operation_id: Uuid::new_v4().simple().to_string(),
            kind: kind.to_string(),
            tool_name: tool_name.map(str::to_string),
            input_summary: json!({"workspace_id": self.workspace_id, "payload": input_summary}),
            result_summary,
            reason: None,
            affected_files,
            created_at: timestamp(),
        };
        self.store
            .append_event_for_workspace(&self.workspace_id, &event)?;
        Ok(event)
    }

    pub fn list_events(
        &self,
        task_id: &str,
        offset: usize,
        limit: usize,
    ) -> HarnessResult<Vec<HarnessEvent>> {
        self.store
            .list_events(&self.workspace_id, task_id, offset, limit)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_operation(
        &self,
        operation_id: Option<&str>,
        task_id: Option<&str>,
        tool: &str,
        kind: &str,
        input_summary: serde_json::Value,
        result_summary: serde_json::Value,
    ) -> HarnessResult<OperationRecord> {
        let reason = input_summary
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let affected_files = affected_files_from_value(&result_summary);
        let operation = OperationRecord {
            id: operation_id
                .map(str::to_string)
                .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
            workspace_id: self.workspace_id.clone(),
            task_id: task_id.map(str::to_string),
            tool: tool.to_string(),
            kind: kind.to_string(),
            input_summary,
            result_summary,
            reason,
            affected_files,
            created_at: timestamp(),
        };
        self.store.append_operation(&self.workspace_id, &operation)?;
        Ok(operation)
    }

    pub fn list_operations(
        &self,
        offset: usize,
        limit: usize,
    ) -> HarnessResult<Vec<OperationRecord>> {
        self.store
            .list_operations(&self.workspace_id, offset, limit)
    }

    pub fn project_state(&self, max_files: usize) -> HarnessResult<ProjectState> {
        let task = self.current_task()?;
        let previous = task
            .as_ref()
            .and_then(|task| task.expected_baseline.as_ref())
            .or_else(|| task.as_ref().map(|task| &task.baseline));
        let current = capture_baseline_incremental(&self.workspace_root, previous);
        let baseline_map = task
            .as_ref()
            .map(|t| {
                t.baseline
                    .entries
                    .iter()
                    .map(|e| (e.path.clone(), e))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let current_map: HashMap<_, _> = current
            .entries
            .iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        let mut paths: Vec<String> = baseline_map
            .keys()
            .chain(current_map.keys())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        let total_files = paths.len();
        let files = paths
            .into_iter()
            .map(|path| {
                let before = baseline_map.get(&path).map(|e| e.sha256.clone());
                let entry = current_map.get(&path);
                let status = match (before, entry) {
                    (Some(before), Some(entry)) if before == entry.sha256 => "unchanged",
                    (Some(_), Some(_)) => "modified",
                    (Some(_), None) => "deleted",
                    (None, Some(_)) => "added",
                    (None, None) => "unknown",
                };
                ProjectFileState {
                    path,
                    status: status.to_string(),
                    sha256: entry.map(|e| e.sha256.clone()).unwrap_or_default(),
                    bytes: entry.map(|e| e.bytes).unwrap_or(0),
                }
            })
            .collect::<Vec<_>>();
        let truncated = files.len() > max_files.max(1);
        let files = files.into_iter().take(max_files.max(1)).collect::<Vec<_>>();
        let active_task_id = task.as_ref().map(|t| t.id.clone());
        let recent_events = task
            .as_ref()
            .and_then(|t| self.list_events(&t.id, 0, 100).ok())
            .map(|events| events.len())
            .unwrap_or(0);
        Ok(ProjectState {
            schema_version: SCHEMA_VERSION,
            workspace_id: self.workspace_id.clone(),
            branch: current.branch,
            head: current.head,
            clean: files.iter().all(|f| f.status == "unchanged"),
            files,
            total_files,
            truncated,
            active_task_id,
            task,
            recent_events,
        })
    }

    pub fn status(&self) -> HarnessResult<HarnessStatus> {
        let task = self.current_task()?;
        let previous = task
            .as_ref()
            .and_then(|task| task.expected_baseline.as_ref())
            .or_else(|| task.as_ref().map(|task| &task.baseline));
        let current = capture_baseline_incremental(&self.workspace_root, previous);
        let (task_id, task_state, task_updated_at, writable, baseline_matches, reason) =
            match task.as_ref() {
                Some(task) => {
                    let matches = task.baseline.branch == current.branch
                        && task.expected_fingerprint == current.worktree_fingerprint;
                    let writable = matches && task.status.is_writable();
                    let reason = if !matches {
                        "工作区基线已变化，写入和执行已暂停"
                    } else if task.status == TaskStatus::Paused {
                        "任务已暂停；读取和任务管理仍可用，工作区写入和执行已禁用"
                    } else if writable {
                        "任务可继续执行"
                    } else {
                        "任务当前状态不允许工作区写入或执行"
                    };
                    (
                        Some(task.id.clone()),
                        Some(task.status),
                        Some(task.updated_at.clone()),
                        writable,
                        Some(matches),
                        reason.to_string(),
                    )
                }
                None => (
                    None,
                    None,
                    None,
                    true,
                    None,
                    "当前没有活动任务，工作区采用无任务模式；修改不会进入任务事件流".to_string(),
                ),
            };

        let denied_reason = if baseline_matches == Some(false) {
            "工作区基线不匹配；必须先审查并显式恢复"
        } else if task_state == Some(TaskStatus::Paused) {
            "任务已暂停；请先恢复任务后再修改或执行"
        } else {
            "当前任务状态不允许工作区写入或执行"
        };

        let mut capabilities = HashMap::new();
        capabilities.insert(
            "read".into(),
            CapabilityStatus {
                status: "available".into(),
                reason: "工作区读取不依赖活动任务".into(),
                recoverable: true,
            },
        );
        capabilities.insert(
            "write".into(),
            CapabilityStatus {
                status: if writable { "available" } else { "denied" }.into(),
                reason: if writable {
                    if task_id.is_some() {
                        "活动任务和工作区基线有效"
                    } else {
                        "无任务模式允许直接修改，建议需要长期追踪时调用 start_task"
                    }
                } else {
                    denied_reason
                }
                .into(),
                recoverable: true,
            },
        );
        capabilities.insert(
            "exec".into(),
            CapabilityStatus {
                status: if writable { "available" } else { "denied" }.into(),
                reason: if writable {
                    if task_id.is_some() {
                        "活动任务和工作区基线有效"
                    } else {
                        "无任务模式允许直接执行，建议需要长期追踪时调用 start_task"
                    }
                } else {
                    denied_reason
                }
                .into(),
                recoverable: true,
            },
        );
        capabilities.insert(
            "git".into(),
            CapabilityStatus {
                status: if current.branch.is_some() && current.head.is_some() {
                    "available"
                } else {
                    "degraded"
                }
                .into(),
                reason: if current.branch.is_some() && current.head.is_some() {
                    "已读取当前分支和 HEAD"
                } else {
                    "当前工作区不是可读取 Git 状态的仓库"
                }
                .into(),
                recoverable: true,
            },
        );
        capabilities.insert(
            "network".into(),
            CapabilityStatus {
                status: "managed_by_policy".into(),
                reason: "网络权限由工具策略控制，不由 Harness 任务状态决定".into(),
                recoverable: true,
            },
        );

        let mut next_actions = Vec::new();
        if task_id.is_none() {
            next_actions.push("start_task".into());
        } else if baseline_matches == Some(false) {
            next_actions.push("project_state".into());
            next_actions.push("git_diff".into());
            if task
                .as_ref()
                .is_some_and(|task| task.baseline.branch == current.branch)
            {
                next_actions.push("refresh_baseline".into());
                next_actions.push("resume_task".into());
            }
        } else if !writable {
            next_actions.push("resume_task".into());
        }
        next_actions.push("read_file".into());
        next_actions.push("git_status".into());

        Ok(HarnessStatus {
            schema_version: SCHEMA_VERSION,
            workspace_id: self.workspace_id.clone(),
            task_id,
            task_state,
            task_updated_at,
            writable,
            reason,
            recoverable: true,
            branch: current.branch,
            head: current.head,
            baseline_matches,
            capabilities,
            next_actions,
        })
    }

    fn save_workspace_state(
        &self,
        active_task_id: Option<&str>,
        updated_at: &str,
    ) -> HarnessResult<()> {
        self.store.save_workspace_state(
            &self.workspace_id,
            &WorkspaceHarnessState {
                schema_version: SCHEMA_VERSION,
                active_task_id: active_task_id.map(str::to_string),
                recent_task_ids: self
                    .store
                    .list_tasks(&self.workspace_id)?
                    .into_iter()
                    .take(20)
                    .map(|t| t.id)
                    .collect(),
                updated_at: updated_at.to_string(),
            },
        )
    }
}

pub fn capture_baseline(root: &Path) -> ProjectBaseline {
    capture_baseline_incremental(root, None)
}

pub fn capture_baseline_incremental(
    root: &Path,
    previous: Option<&ProjectBaseline>,
) -> ProjectBaseline {
    let previous_map = previous
        .map(|baseline| {
            baseline
                .entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut entries = Vec::new();
    for path in baseline_files(root) {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let bytes_len = metadata.len();
        let modified_ns = modified_ns(&metadata);
        if let Some(previous_entry) = previous_map.get(rel.as_str()) {
            if previous_entry.bytes == bytes_len
                && previous_entry.modified_ns.is_some()
                && previous_entry.modified_ns == modified_ns
            {
                let mut reused = (*previous_entry).clone();
                reused.modified_ns = modified_ns;
                entries.push(reused);
                continue;
            }
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        entries.push(BaselineEntry {
            path: rel,
            exists: true,
            is_binary: bytes.contains(&0),
            sha256: format!("{:x}", hasher.finalize()),
            bytes: bytes.len() as u64,
            modified_ns,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut fingerprint = Sha256::new();
    for entry in &entries {
        fingerprint.update(entry.path.as_bytes());
        fingerprint.update(entry.sha256.as_bytes());
        fingerprint.update(entry.bytes.to_le_bytes());
    }
    ProjectBaseline {
        branch: git_value(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        head: git_value(root, &["rev-parse", "HEAD"]),
        worktree_fingerprint: format!("{:x}", fingerprint.finalize()),
        entries,
        captured_at: timestamp(),
    }
}

pub fn diff_baselines(before: &ProjectBaseline, after: &ProjectBaseline) -> Vec<FileChangeRecord> {
    let before_map = before
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let after_map = after
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut paths = before_map
        .keys()
        .chain(after_map.keys())
        .copied()
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| match (before_map.get(path), after_map.get(path)) {
            (Some(before), Some(after)) if before.sha256 == after.sha256 => None,
            (Some(before), Some(after)) => Some(FileChangeRecord {
                path: path.to_string(),
                status: "modified".into(),
                before_sha256: Some(before.sha256.clone()),
                after_sha256: Some(after.sha256.clone()),
            }),
            (Some(before), None) => Some(FileChangeRecord {
                path: path.to_string(),
                status: "deleted".into(),
                before_sha256: Some(before.sha256.clone()),
                after_sha256: None,
            }),
            (None, Some(after)) => Some(FileChangeRecord {
                path: path.to_string(),
                status: "added".into(),
                before_sha256: None,
                after_sha256: Some(after.sha256.clone()),
            }),
            (None, None) => None,
        })
        .collect()
}

fn affected_files_from_value(value: &Value) -> Vec<FileChangeRecord> {
    value
        .get("affected_files")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let path = item.get("path").and_then(Value::as_str)?.to_string();
                    let status = item
                        .get("status")
                        .or_else(|| item.get("operation"))
                        .and_then(Value::as_str)
                        .unwrap_or("modified")
                        .to_string();
                    Some(FileChangeRecord {
                        path,
                        status,
                        before_sha256: item
                            .get("before_sha256")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        after_sha256: item
                            .get("after_sha256")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn modified_ns(metadata: &fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    let nanos = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some(nanos.min(u64::MAX as u128) as u64)
}

fn baseline_files(root: &Path) -> Vec<PathBuf> {
    if let Some(relative) = git_visible_files(root) {
        return relative
            .into_iter()
            .map(|rel| join_relative(root, &rel))
            .filter(|path| path.is_file() && !should_skip(path, root))
            .collect();
    }
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|item| item.file_type().is_file())
        .map(|item| item.into_path())
        .filter(|path| path != root && !should_skip(path, root))
        .collect()
}

fn git_visible_files(root: &Path) -> Option<Vec<String>> {
    let toplevel = git_value(root, &["rev-parse", "--show-toplevel"])?;
    if normalize_path(Path::new(toplevel.trim())) != normalize_path(root) {
        return None;
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| String::from_utf8_lossy(chunk).replace('\\', "/"))
            .collect(),
    )
}

fn join_relative(root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .fold(root.to_path_buf(), |mut path, part| {
            path.push(part);
            path
        })
}

const HARNESS_CACHE_DIR_NAMES: &[&str] = &[
    ".coding-tools",
    ".mcp-probe-kit",
    ".gitnexus",
    ".worktrees",
    ".svelte-kit",
    ".vite",
    ".turbo",
    ".next",
    ".nuxt",
    ".cache",
    ".parcel-cache",
    ".nyc_output",
    ".idea",
    ".mcp",
    ".mcp-cache",
    ".tmp",
];

const HARNESS_ROOT_DIR_NAMES: &[&str] = &[
    "coverage",
    "scratch",
    ".scratch",
    "tmp-run",
    ".local-build",
];

fn should_skip(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let posix = relative.to_string_lossy().replace('\\', "/");
    if posix == "docs/history-session" || posix.starts_with("docs/history-session/") {
        return true;
    }
    let names: Vec<&str> = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if names.iter().any(|name| {
        DEFAULT_EXCLUDED_NAMES.contains(name) || HARNESS_CACHE_DIR_NAMES.contains(name)
    }) {
        return true;
    }
    if names
        .first()
        .is_some_and(|name| HARNESS_ROOT_DIR_NAMES.contains(name))
    {
        return true;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".DS_Store"
                    | "Thumbs.db"
                    | "desktop.ini"
                    | ".eslintcache"
                    | "lcov.info"
                    | "coverage.xml"
                    | ".coverage"
                    | "cobertura.xml"
            ) || name.ends_with(".tsbuildinfo")
                || name.ends_with(".pyc")
                || name.ends_with(".pyo")
                || name.ends_with(".snap.new")
                || name.ends_with(".pending-snap")
                || name.ends_with('~')
                || name.ends_with(".orig")
                || name.ends_with(".rej")
                || name.ends_with(".bak")
                || name.ends_with(".swp")
                || name.ends_with(".swo")
                || name.contains(".timestamp-")
                || name.contains(".harness-stage-")
        })
}

fn normalize_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        let candidate = msys_drive_path(&raw)
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
        let canonical = fs::canonicalize(&candidate).unwrap_or(candidate);
        PathBuf::from(normalize_windows_path_text(&canonical.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(windows)]
fn msys_drive_path(raw: &str) -> Option<String> {
    let rest = raw.trim().strip_prefix('/')?;
    let mut chars = rest.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    match chars.next() {
        None => Some(format!("{drive}:")),
        Some('/') => Some(format!("{drive}:\\{}", chars.as_str().replace('/', "\\"))),
        _ => None,
    }
}

#[cfg(windows)]
fn normalize_windows_path_text(raw: &str) -> String {
    let text = raw.replace('/', "\\");
    let stripped = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\unc\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    };
    stripped.trim_end_matches('\\').to_lowercase()
}

fn git_value(root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn workspace_id(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn status_keeps_read_available_without_task() {
        let workspace = tempdir().expect("workspace");
        let harness_root = tempdir().expect("harness");
        fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("file");
        let harness = Harness::new(
            workspace.path().to_path_buf(),
            harness_root.path().to_path_buf(),
        )
        .expect("harness");

        let status = harness.status().expect("status");
        assert!(status.writable);
        assert_eq!(status.capabilities["read"].status, "available");
        assert_eq!(status.capabilities["write"].status, "available");
        assert!(status.next_actions.contains(&"start_task".to_string()));
    }

    #[test]
    fn paused_task_stays_current_but_is_not_writable() {
        let workspace = tempdir().expect("workspace");
        let harness_root = tempdir().expect("harness");
        fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("file");
        let harness = Harness::new(
            workspace.path().to_path_buf(),
            harness_root.path().to_path_buf(),
        )
        .expect("harness");
        let task = harness.start_task("pause semantics").expect("start");
        harness
            .transition(&task.id, TaskStatus::Paused)
            .expect("pause task");

        assert_eq!(harness.current_task().expect("current").expect("task").id, task.id);
        let status = harness.status().expect("status");
        assert_eq!(status.task_state, Some(TaskStatus::Paused));
        assert_eq!(status.baseline_matches, Some(true));
        assert!(!status.writable);
        assert!(status.reason.contains("已暂停"));
        assert!(status.next_actions.contains(&"resume_task".to_string()));
    }

    #[test]
    fn starting_task_does_not_create_workspace_copies() {
        let workspace = tempdir().expect("workspace");
        let harness_root = tempdir().expect("harness");
        fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("file");
        let harness = Harness::new(
            workspace.path().to_path_buf(),
            harness_root.path().to_path_buf(),
        )
        .expect("harness");

        harness.start_task("测试任务").expect("start task");
        assert!(!harness
            .store_root()
            .join("workspaces")
            .join(harness.workspace_id())
            .join("snapshots")
            .exists());
    }

    #[cfg(windows)]
    #[test]
    fn normalize_path_strips_extended_and_msys_prefixes() {
        assert_eq!(
            normalize_path(Path::new(r"\\?\C:\repo\app")),
            PathBuf::from(r"c:\repo\app")
        );
        assert_eq!(
            normalize_path(Path::new(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
        assert_eq!(
            normalize_path(Path::new("/d/TianFeng/repo")),
            PathBuf::from(r"d:\tianfeng\repo")
        );
    }

    #[test]
    fn coding_tools_planning_state_is_not_external_change() {
        let workspace = tempdir().expect("workspace");
        let harness_root = tempdir().expect("harness");
        fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("file");
        let harness = Harness::new(
            workspace.path().to_path_buf(),
            harness_root.path().to_path_buf(),
        )
        .expect("harness");

        let task = harness.start_task("验证规划状态忽略").expect("start task");
        let planning_dir = workspace.path().join(".coding-tools").join("planning");
        fs::create_dir_all(&planning_dir).expect("planning dir");
        fs::write(
            planning_dir.join("state.json"),
            r#"{"schema_version":1,"goals":[]}"#,
        )
        .expect("write planning state");

        harness
            .check_baseline(&task.id)
            .expect("planning state must not count as external change");
        let status = harness.status().expect("status");
        assert!(status.writable);
        assert_eq!(status.baseline_matches, Some(true));
    }

    #[test]
    fn incremental_baseline_reuses_unchanged_hash_and_detects_real_change() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("file");
        let first = capture_baseline_incremental(workspace.path(), None);
        let second = capture_baseline_incremental(workspace.path(), Some(&first));
        assert_eq!(first.worktree_fingerprint, second.worktree_fingerprint);

        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(workspace.path().join("main.rs"), "fn main() { println!(\"changed\"); }\n")
            .expect("change");
        let third = capture_baseline_incremental(workspace.path(), Some(&second));
        assert_ne!(second.worktree_fingerprint, third.worktree_fingerprint);
        let changes = diff_baselines(&second, &third);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "main.rs");
        assert_eq!(changes[0].status, "modified");
    }
}
