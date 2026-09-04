use std::fs;

use coding_tools_mcp_desktop_lib::harness::{Harness, TaskStatus};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("创建临时目录");
    let workspace = temp.path().join("workspace");
    let harness_root = temp.path().join("harness");
    fs::create_dir_all(&workspace).expect("创建工作区");
    fs::write(workspace.join("README.md"), "初始内容\n").expect("写入夹具");
    (temp, workspace, harness_root)
}

#[test]
fn 任务创建会捕获基线并在重启后恢复() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace.clone(), harness_root.clone()).expect("创建 Harness");

    let task = harness
        .start_task("实现 Harness 基础能力")
        .expect("启动任务");

    assert_eq!(task.status, TaskStatus::Active);
    assert_eq!(task.objective, "实现 Harness 基础能力");
    assert!(!task.baseline.worktree_fingerprint.is_empty());
    assert_eq!(
        harness
            .current_task()
            .expect("读取任务")
            .expect("活动任务")
            .id,
        task.id
    );

    let restarted = Harness::new(workspace, harness_root).expect("重启 Harness");
    assert_eq!(
        restarted
            .current_task()
            .expect("恢复任务")
            .expect("活动任务")
            .id,
        task.id
    );
}

#[test]
fn 同一工作区只允许一个可写任务且拒绝非法迁移() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace, harness_root).expect("创建 Harness");
    let task = harness.start_task("第一个任务").expect("启动任务");

    let duplicate = harness
        .start_task("第二个任务")
        .expect_err("应拒绝第二个任务");
    assert_eq!(duplicate.code(), "TASK_ALREADY_ACTIVE");

    let invalid = harness
        .transition(&task.id, TaskStatus::Completed)
        .expect_err("active 不应直接完成");
    assert_eq!(invalid.code(), "INVALID_TASK_TRANSITION");

    let paused = harness
        .transition(&task.id, TaskStatus::Paused)
        .expect("暂停任务");
    assert_eq!(paused.status, TaskStatus::Paused);
    let resumed = harness
        .transition(&task.id, TaskStatus::Active)
        .expect("恢复任务");
    assert_eq!(resumed.status, TaskStatus::Active);
}

#[test]
fn 外部文件变化会被识别且操作会留下事件() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("验证外部变更").expect("启动任务");

    fs::write(workspace.join("README.md"), "外部修改\n").expect("模拟外部修改");
    let stale = harness
        .check_baseline(&task.id)
        .expect_err("应识别外部修改");
    assert_eq!(stale.code(), "FILE_CHANGED_EXTERNALLY");

    harness
        .record_event(
            &task.id,
            "operation_finished",
            Some("read_file"),
            json!({"reason": "确认外部变更"}),
            json!({"ok": true}),
        )
        .expect("记录事件");
    let events = harness.list_events(&task.id, 0, 10).expect("读取事件");
    assert!(events.len() >= 2);
    assert!(events
        .iter()
        .any(|event| event.tool_name.as_deref() == Some("read_file")));
}

#[test]
fn project_state包含分支任务和脏状态摘要() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace, harness_root).expect("创建 Harness");
    let task = harness.start_task("生成项目状态").expect("启动任务");

    let state = harness.project_state(20).expect("读取项目状态");

    assert_eq!(state.active_task_id.as_deref(), Some(task.id.as_str()));
    assert!(!state.files.is_empty());
    assert!(state.task.is_some());
}

#[test]
fn planning_state_write_is_not_external_change() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("验证规划写入忽略").expect("启动任务");

    let planning_dir = workspace.join(".coding-tools").join("planning");
    fs::create_dir_all(&planning_dir).expect("创建规划目录");
    fs::write(
        planning_dir.join("state.json"),
        r#"{"schema_version":1,"revision":1}"#,
    )
    .expect("写入规划状态");

    harness
        .check_baseline(&task.id)
        .expect("自写 planning state 不应触发 FILE_CHANGED_EXTERNALLY");
    let status = harness.status().expect("读取状态");
    assert!(status.writable, "执行权限不应因 planning state 被锁");
    assert_eq!(status.baseline_matches, Some(true));
}

#[test]
fn 测试缓存目录不是外部文件变化() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("验证缓存忽略").expect("启动任务");

    let cache = workspace.join("__pycache__");
    fs::create_dir_all(&cache).expect("创建缓存目录");
    fs::write(cache.join("mod.cpython-312.pyc"), b"\0cache").expect("写入编译缓存");
    fs::create_dir_all(workspace.join(".pytest_cache")).expect("pytest 缓存");
    fs::write(workspace.join(".pytest_cache").join("v"), "1").expect("写入 pytest 缓存");
    fs::create_dir_all(workspace.join(".gitnexus")).expect("gitnexus");
    fs::write(workspace.join(".gitnexus").join("index.json"), "{}").expect("写入图谱索引");

    harness
        .check_baseline(&task.id)
        .expect("测试进程写入的缓存不应触发 FILE_CHANGED_EXTERNALLY");
    let status = harness.status().expect("读取状态");
    assert!(status.writable);
    assert_eq!(status.baseline_matches, Some(true));
}

#[test]
fn 测试快照和覆盖率文件不是外部文件变化() {
    let (_temp, workspace, harness_root) = fixture();
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("验证测试产物忽略").expect("启动任务");

    fs::write(workspace.join("spec.snap.new"), "pending snapshot").expect("insta 产物");
    fs::write(workspace.join("README.md~"), "editor backup").expect("编辑器备份");
    fs::write(workspace.join("notes.bak"), "backup").expect("bak");
    fs::write(
        workspace.join(".README.md.harness-stage-deadbeef"),
        "staged",
    )
    .expect("patch 暂存残留");
    fs::write(workspace.join("lcov.info"), "TN:\n").expect("覆盖率");
    fs::create_dir_all(workspace.join("docs/history-session")).expect("history");
    fs::write(
        workspace.join("docs/history-session/1.md"),
        "# session\n",
    )
    .expect("history archive");

    harness
        .check_baseline(&task.id)
        .expect("测试/会话产物不应触发 FILE_CHANGED_EXTERNALLY");
    assert!(harness.status().expect("status").writable);
}

#[test]
fn 根目录产物忽略但嵌套源码目录仍受锁() {
    let (_temp, workspace, harness_root) = fixture();
    fs::create_dir_all(workspace.join("src/scratch")).expect("嵌套 scratch");
    fs::write(workspace.join("src/scratch/lib.rs"), "fn old() {}\n").expect("源码");
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("嵌套源码仍受锁").expect("启动任务");

    fs::create_dir_all(workspace.join("scratch")).expect("根 scratch");
    fs::write(workspace.join("scratch/tmp.txt"), "noise").expect("根 scratch 产物");
    fs::create_dir_all(workspace.join("coverage")).expect("根 coverage");
    fs::write(workspace.join("coverage/out.txt"), "cov").expect("根 coverage");
    harness
        .check_baseline(&task.id)
        .expect("根目录 scratch/coverage 不应锁写");

    fs::write(workspace.join("src/scratch/lib.rs"), "fn new() {}\n").expect("改嵌套源码");
    let stale = harness
        .check_baseline(&task.id)
        .expect_err("src/scratch 源码修改应识别为外部变化");
    assert_eq!(stale.code(), "FILE_CHANGED_EXTERNALLY");
}

#[test]
fn agents目录中的源码修改仍是外部变化() {
    let (_temp, workspace, harness_root) = fixture();
    fs::create_dir_all(workspace.join(".agents/skills")).expect("agents");
    fs::write(workspace.join(".agents/skills/guide.md"), "old\n").expect("skill");
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("agents 源码受锁").expect("启动任务");

    fs::write(workspace.join(".agents/skills/guide.md"), "new\n").expect("改 skill");
    let stale = harness
        .check_baseline(&task.id)
        .expect_err(".agents 修改应识别为外部变化");
    assert_eq!(stale.code(), "FILE_CHANGED_EXTERNALLY");
}

#[test]
fn 删除已跟踪源码是外部文件变化() {
    let (_temp, workspace, harness_root) = fixture();
    init_git_repo(&workspace);
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("删除源码应锁").expect("启动任务");

    fs::remove_file(workspace.join("README.md")).expect("删除跟踪文件");
    let stale = harness
        .check_baseline(&task.id)
        .expect_err("删除已跟踪源码应识别为外部变化");
    assert_eq!(stale.code(), "FILE_CHANGED_EXTERNALLY");
}

#[test]
fn gitignore_中的产物不是外部文件变化但源码修改仍是() {
    let (_temp, workspace, harness_root) = fixture();
    init_git_repo(&workspace);
    let harness = Harness::new(workspace.clone(), harness_root).expect("创建 Harness");
    let task = harness.start_task("验证 gitignore 基线").expect("启动任务");

    fs::write(workspace.join("local-artifact.dat"), "from test run").expect("写入被忽略产物");
    harness
        .check_baseline(&task.id)
        .expect("gitignore 产物不应触发 FILE_CHANGED_EXTERNALLY");

    fs::write(workspace.join("README.md"), "源码被改了\n").expect("修改源码");
    let stale = harness
        .check_baseline(&task.id)
        .expect_err("源码修改仍应识别为外部变化");
    assert_eq!(stale.code(), "FILE_CHANGED_EXTERNALLY");
}

fn init_git_repo(workspace: &std::path::Path) {
    fs::write(workspace.join(".gitignore"), "local-artifact.dat\n").expect("gitignore");
    git(workspace, &["init"]);
    git(workspace, &["config", "user.email", "harness@example.com"]);
    git(workspace, &["config", "user.name", "Harness Test"]);
    git(workspace, &["add", "README.md", ".gitignore"]);
    git(workspace, &["commit", "-m", "init"]);
}

fn git(workspace: &std::path::Path, args: &[&str]) {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(workspace).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000200 | 0x08000000);
    }
    let output = cmd.output().expect("运行 git");
    assert!(
        output.status.success(),
        "git {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
