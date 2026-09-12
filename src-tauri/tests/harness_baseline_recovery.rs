use std::fs;
use std::process::Command;

use coding_tools_mcp_desktop_lib::tools::{call_tool, ToolContext};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, ToolContext) {
    let temp = tempfile::tempdir().expect("创建临时目录");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("创建工作区");
    fs::write(workspace.join("README.md"), "initial\n").expect("写入初始文件");
    let ctx = ToolContext::for_test(workspace.clone(), temp.path().join("harness"))
        .expect("创建上下文");
    (temp, workspace, ctx)
}

#[test]
fn reviewed_workspace_can_be_recovered_through_stable_task_manager() {
    let (_temp, workspace, ctx) = fixture();
    let ctx = ctx.with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "baseline recovery"}),
    );
    assert_eq!(started["ok"], true, "start: {started}");
    let task_id = started["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let original_baseline = started["task"]["baseline"]["worktree_fingerprint"]
        .as_str()
        .expect("baseline fingerprint")
        .to_string();

    fs::write(workspace.join("README.md"), "reviewed change\n").expect("模拟已审查变化");
    let blocked = call_tool(
        &ctx,
        "exec_command",
        &json!({"cmd": "python --version", "filesystem_scope": "workspace"}),
    );
    assert_eq!(blocked["ok"], false, "external change must block: {blocked}");
    assert_eq!(blocked["error"]["code"], "FILE_CHANGED_EXTERNALLY");
    assert!(blocked["harness"]["next_actions"]
        .as_array()
        .expect("blocked recovery actions")
        .iter()
        .any(|action| action == "task_manage:refresh_baseline"));

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["baseline_matches"], false, "status: {status}");
    assert!(status["next_actions"]
        .as_array()
        .expect("next actions")
        .iter()
        .any(|action| action == "task_manage:refresh_baseline"));
    assert_eq!(status["recovery"]["preferred_action"], "task_manage:refresh_baseline");
    assert_eq!(status["recovery"]["compatibility_action"], "task_manage:resume");
    let reviewed_fingerprint = status["recovery"]["change_id"]
        .as_str()
        .expect("reviewed worktree fingerprint")
        .to_string();

    let refreshed = call_tool(
        &ctx,
        "task_manage",
        &json!({
            "action": "refresh_baseline",
            "task_id": task_id,
            "change_id": reviewed_fingerprint,
            "summary": "reviewed project_state and git_diff"
        }),
    );
    assert_eq!(refreshed["ok"], true, "refresh: {refreshed}");
    assert_eq!(refreshed["diagnostics"]["baseline_refreshed"], true);
    assert_eq!(
        refreshed["task"]["baseline"]["worktree_fingerprint"],
        original_baseline,
        "immutable task-start baseline must be preserved"
    );
    assert_eq!(
        refreshed["task"]["expected_fingerprint"],
        refreshed["diagnostics"]["accepted_worktree_fingerprint"]
    );
    assert_eq!(refreshed["harness"]["baseline_matches"], true);
    assert!(refreshed.get("status").is_none(), "refresh output must match the declared common status:string schema");

    let next = call_tool(
        &ctx,
        "exec_command",
        &json!({"cmd": "python --version", "filesystem_scope": "workspace"}),
    );
    assert_eq!(next["ok"], true, "recovered task should execute: {next}");
}

#[test]
fn stale_task_manage_schema_can_recover_via_resume_compatibility_action() {
    let (_temp, workspace, ctx) = fixture();
    let ctx = ctx.with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "stale schema recovery"}),
    );
    assert_eq!(started["ok"], true, "start: {started}");
    let task_id = started["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();

    let paused = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "pause", "task_id": task_id}),
    );
    assert_eq!(paused["ok"], true, "pause: {paused}");
    fs::write(workspace.join("README.md"), "reviewed stale-schema change\n")
        .expect("模拟已审查变化");

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["baseline_matches"], false, "status: {status}");
    let change_id = status["recovery"]["change_id"]
        .as_str()
        .expect("compat recovery fingerprint")
        .to_string();

    // `resume`, `task_id`, `change_id`, and `summary` all existed in the old v2
    // task_manage schema, so this call remains valid even when a client cached the
    // pre-refresh_baseline enum.
    let resumed = call_tool(
        &ctx,
        "task_manage",
        &json!({
            "action": "resume",
            "task_id": task_id,
            "change_id": change_id,
            "summary": "reviewed project_state and git_diff; accept this exact fingerprint"
        }),
    );
    assert_eq!(resumed["ok"], true, "resume compatibility recovery: {resumed}");
    assert_eq!(resumed["task"]["status"], "active");
    assert_eq!(resumed["diagnostics"]["baseline_refreshed"], true);
    assert_eq!(resumed["diagnostics"]["compatibility_mode"], true);
    assert_eq!(resumed["harness"]["baseline_matches"], true);

    let next = call_tool(
        &ctx,
        "exec_command",
        &json!({"cmd": "python --version", "filesystem_scope": "workspace"}),
    );
    assert_eq!(next["ok"], true, "compatibility recovery should unblock exec: {next}");
}

#[test]
fn resume_compatibility_rejects_a_worktree_that_changed_after_review() {
    let (_temp, workspace, ctx) = fixture();
    let ctx = ctx.with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "optimistic baseline recovery"}),
    );
    let task_id = started["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let original_expected = started["task"]["expected_fingerprint"]
        .as_str()
        .expect("expected fingerprint")
        .to_string();
    let paused = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "pause", "task_id": task_id}),
    );
    assert_eq!(paused["ok"], true, "pause: {paused}");

    fs::write(workspace.join("README.md"), "reviewed once\n").expect("first external write");
    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    let reviewed_change_id = status["recovery"]["change_id"]
        .as_str()
        .expect("reviewed fingerprint")
        .to_string();
    fs::write(workspace.join("README.md"), "changed again after review\n")
        .expect("second external write");

    let rejected = call_tool(
        &ctx,
        "task_manage",
        &json!({
            "action": "resume",
            "task_id": task_id,
            "change_id": reviewed_change_id,
            "summary": "attempt to accept stale review"
        }),
    );
    assert_eq!(rejected["ok"], false, "stale reviewed state must be rejected: {rejected}");
    assert_eq!(rejected["error"]["code"], "FILE_CHANGED_EXTERNALLY");

    let context = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "context", "task_id": task_id}),
    );
    assert_eq!(
        context["task"]["expected_fingerprint"], original_expected,
        "failed optimistic recovery must not mutate the expected fingerprint"
    );
    assert_eq!(context["task"]["status"], "paused");
}

#[test]
fn resume_compatibility_requires_an_audit_summary() {
    let (_temp, workspace, ctx) = fixture();
    let ctx = ctx.with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "audited recovery"}),
    );
    let task_id = started["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let paused = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "pause", "task_id": task_id}),
    );
    assert_eq!(paused["ok"], true, "pause: {paused}");
    fs::write(workspace.join("README.md"), "reviewed\n").expect("external write");
    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    let change_id = status["recovery"]["change_id"]
        .as_str()
        .expect("reviewed fingerprint")
        .to_string();

    let rejected = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "resume", "task_id": task_id, "change_id": change_id}),
    );
    assert_eq!(rejected["ok"], false, "summary is required: {rejected}");
    assert_eq!(rejected["error"]["code"], "BASELINE_REVIEW_REQUIRED");
}

#[test]
fn refresh_baseline_refuses_to_bless_a_new_git_revision() {
    let temp = tempfile::tempdir().expect("创建临时目录");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("创建工作区");
    fs::write(workspace.join("README.md"), "initial\n").expect("初始文件");
    git(&workspace, &["init"]);
    git(&workspace, &["config", "user.email", "harness@example.com"]);
    git(&workspace, &["config", "user.name", "Harness Test"]);
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-m", "initial"]);

    let ctx = ToolContext::for_test(workspace.clone(), temp.path().join("harness"))
        .expect("创建上下文");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "do not bless head changes"}),
    );
    let task_id = started["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();

    fs::write(workspace.join("README.md"), "new revision\n").expect("修改文件");
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-m", "external revision"]);

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["recovery"]["type"], "git_revision_drift");
    assert_eq!(status["recovery"]["recoverable_via_baseline_acceptance"], false);

    let refreshed = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "refresh_baseline", "task_id": task_id}),
    );
    assert_eq!(refreshed["ok"], false, "refresh must fail: {refreshed}");
    assert_eq!(refreshed["error"]["code"], "BASELINE_STALE");
}

#[test]
fn task_tracked_noninteractive_exec_waits_for_delayed_workspace_writes() {
    let (_temp, workspace, ctx) = fixture();
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "settle delayed exec writes"}),
    );
    assert_eq!(started["ok"], true, "start: {started}");

    let delayed = call_tool(
        &ctx,
        "exec_command",
        &json!({
            "cmd": "python -c \"import time; time.sleep(0.15); open('late.txt','w',encoding='utf-8').write('late')\"",
            "filesystem_scope": "workspace",
            "yield_time_ms": 0,
            "timeout_ms": 5000
        }),
    );
    assert_eq!(delayed["ok"], true, "exec: {delayed}");
    assert_eq!(delayed["status"], "exited", "tracked exec must settle: {delayed}");
    assert_eq!(delayed["command_ok"], true, "exec should succeed: {delayed}");
    assert_eq!(
        fs::read_to_string(workspace.join("late.txt")).expect("late write exists"),
        "late"
    );

    let next = call_tool(
        &ctx,
        "exec_command",
        &json!({"cmd": "python --version", "filesystem_scope": "workspace"}),
    );
    assert_eq!(next["ok"], true, "task-generated delayed write must be absorbed: {next}");
    assert_ne!(
        next.get("error")
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str),
        Some("FILE_CHANGED_EXTERNALLY")
    );
}

#[test]
fn standalone_exec_keeps_background_session_behavior() {
    let (_temp, _workspace, ctx) = fixture();
    let background = call_tool(
        &ctx,
        "exec_command",
        &json!({
            "cmd": "python -c \"import time; time.sleep(1)\"",
            "yield_time_ms": 0,
            "timeout_ms": 5000
        }),
    );
    assert_eq!(background["ok"], true, "exec: {background}");
    assert_eq!(background["status"], "running", "standalone exec may yield: {background}");
    let session_id = background["session_id"].as_str().expect("session id");
    let killed = call_tool(
        &ctx,
        "kill_session",
        &json!({"session_id": session_id, "signal": "TERM", "wait_ms": 1000}),
    );
    assert_eq!(killed["ok"], true, "cleanup: {killed}");
}

fn git(workspace: &std::path::Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200 | 0x08000000);
    }
    let output = command.output().expect("运行 git");
    assert!(
        output.status.success(),
        "git {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
