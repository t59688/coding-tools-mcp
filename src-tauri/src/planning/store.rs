use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt;

use crate::error::{AppError, AppResult};

use super::{PlanningState, PLANNING_RELATIVE_PATH};

const PRIVATE_PLANNING_RELATIVE_PATH: &str = "coding-tools/planning/state.json";
const PRIVATE_PLANNING_DISPLAY_PATH: &str = "git-private:coding-tools/planning/state.json";

#[derive(Debug, Clone)]
pub struct PlanningStore {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
    lock_path: PathBuf,
}

impl PlanningStore {
    pub fn new(workspace_root: &Path) -> Self {
        let legacy_path = workspace_root.join(PLANNING_RELATIVE_PATH);
        let private_path = git_private_state_path(workspace_root);
        let legacy_is_tracked = git_tracks_path(workspace_root, PLANNING_RELATIVE_PATH);
        let path = if legacy_is_tracked {
            private_path.unwrap_or_else(|| legacy_path.clone())
        } else {
            legacy_path.clone()
        };
        let legacy_path = (path != legacy_path).then_some(legacy_path);
        let lock_path = path.with_extension("json.lock");
        Self {
            path,
            legacy_path,
            lock_path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn display_path(&self) -> &'static str {
        if self.legacy_path.is_some() {
            PRIVATE_PLANNING_DISPLAY_PATH
        } else {
            PLANNING_RELATIVE_PATH
        }
    }

    pub fn load(&self) -> AppResult<PlanningState> {
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock)?;
        let result = self.load_unlocked();
        let _ = FileExt::unlock(&lock);
        result
    }

    pub fn save(&self, state: &PlanningState) -> AppResult<()> {
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)?;
        let result = self.save_unlocked(state);
        let _ = FileExt::unlock(&lock);
        result
    }

    pub fn update<R>(&self, mutate: impl FnOnce(&mut PlanningState) -> AppResult<R>) -> AppResult<R> {
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)?;
        let result = (|| {
            let mut state = self.load_unlocked()?;
            state.revision = state.revision.saturating_add(1);
            let result = mutate(&mut state)?;
            self.save_unlocked(&state)?;
            Ok(result)
        })();
        let _ = FileExt::unlock(&lock);
        result
    }

    fn open_lock(&self) -> AppResult<File> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.lock_path)?)
    }

    fn load_unlocked(&self) -> AppResult<PlanningState> {
        let source = if self.path.exists() {
            Some(self.path.as_path())
        } else {
            self.legacy_path
                .as_deref()
                .filter(|legacy| legacy.exists())
        };
        let Some(source) = source else {
            return Ok(PlanningState::default());
        };
        let raw = fs::read_to_string(source)?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn save_unlocked(&self, state: &PlanningState) -> AppResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| AppError::Message("planning state path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let raw = serde_json::to_vec_pretty(state)?;
        let temp = self.path.with_extension("json.tmp");
        let backup = self.path.with_extension("json.bak");

        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temp)?;
            file.write_all(&raw)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }

        replace_file_recoverably(&temp, &self.path, &backup)?;
        sync_parent(parent);
        Ok(())
    }
}

fn replace_file_recoverably(temp: &Path, target: &Path, backup: &Path) -> AppResult<()> {
    if fs::rename(temp, target).is_ok() {
        let _ = fs::remove_file(backup);
        return Ok(());
    }

    if target.exists() {
        let _ = fs::remove_file(backup);
        fs::rename(target, backup)?;
    }

    match fs::rename(temp, target) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            if backup.exists() && !target.exists() {
                let _ = fs::rename(backup, target);
            }
            Err(error.into())
        }
    }
}

fn sync_parent(parent: &Path) {
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

fn git_tracks_path(workspace_root: &Path, relative: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["ls-files", "--error-unmatch", "--", relative])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn git_private_state_path(workspace_root: &Path) -> Option<PathBuf> {
    let dot_git = workspace_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join(PRIVATE_PLANNING_RELATIVE_PATH));
    }
    if dot_git.is_file() {
        let raw = fs::read_to_string(dot_git).ok()?;
        let gitdir = raw.trim().strip_prefix("gitdir:")?.trim();
        let gitdir = Path::new(gitdir);
        let gitdir = if gitdir.is_absolute() {
            gitdir.to_path_buf()
        } else {
            workspace_root.join(gitdir)
        };
        return Some(gitdir.join(PRIVATE_PLANNING_RELATIVE_PATH));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn update_is_atomic_and_serializes_concurrent_writers() {
        let workspace = tempdir().expect("workspace");
        let store = Arc::new(PlanningStore::new(workspace.path()));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            threads.push(thread::spawn(move || {
                for _ in 0..25 {
                    store
                        .update(|state| {
                            state.execution.state = state.revision.to_string();
                            Ok(())
                        })
                        .expect("update");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("join");
        }
        let state = store.load().expect("load");
        assert_eq!(state.revision, 200);
        assert!(!store.path.with_extension("json.tmp").exists());
    }

    #[test]
    fn tracked_legacy_state_moves_runtime_writes_out_of_worktree() {
        let workspace = tempdir().expect("workspace");
        let status = Command::new("git")
            .arg("init")
            .arg(workspace.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let legacy = workspace.path().join(PLANNING_RELATIVE_PATH);
        fs::create_dir_all(legacy.parent().unwrap()).expect("planning dir");
        fs::write(&legacy, serde_json::to_vec_pretty(&PlanningState::default()).unwrap())
            .expect("legacy state");
        assert!(Command::new("git")
            .arg("-C")
            .arg(workspace.path())
            .args(["add", PLANNING_RELATIVE_PATH])
            .status()
            .expect("git add")
            .success());

        let store = PlanningStore::new(workspace.path());
        assert_ne!(store.path(), legacy.as_path());
        assert_eq!(store.display_path(), PRIVATE_PLANNING_DISPLAY_PATH);
        assert!(store.path().to_string_lossy().contains(".git"));
        store
            .update(|state| {
                state.execution.state = "completed".into();
                Ok(())
            })
            .expect("update private state");
        assert!(store.path().exists());
        let legacy_after = fs::read_to_string(&legacy).expect("legacy unchanged");
        let legacy_state: PlanningState = serde_json::from_str(&legacy_after).expect("legacy json");
        assert_eq!(legacy_state.revision, 0);
    }
}
