use std::fs;
use std::process::Command;

use coding_tools_mcp_desktop_lib::tools::{call_tool, ToolContext};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, ToolContext) {
    let temp = tempfile::tempdir().expect("create tempdir");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::write(workspace.join("README.md"), "initial\n").expect("write initial file");
    let ctx = ToolContext::for_test(workspace.clone(), temp.path().join("harness"))
        .expect("create tool context")
        .with_tool_profile("compact");
    (temp, workspace, ctx)
}

fn git(workspace: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn reviewed_worktree_advertises_stale_schema_resume_recovery() {
    let (_temp, workspace, ctx) = fixture();
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "discover baseline recovery"}),
    );
    assert_eq!(started["ok"], true, "start task: {started}");

    fs::write(workspace.join("README.md"), "reviewed change\n").expect("change worktree");

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["ok"], true, "status: {status}");
    assert_eq!(status["baseline_matches"], false, "status: {status}");
    assert_eq!(
        status["recovery"]["type"],
        "reviewed_worktree_baseline",
        "status: {status}"
    );
    assert_eq!(
        status["recovery"]["recoverable_via_baseline_acceptance"],
        true,
        "status: {status}"
    );

    let next_actions = status["next_actions"]
        .as_array()
        .expect("next_actions array");
    assert!(
        next_actions
            .iter()
            .any(|action| action == "task_manage:refresh_baseline"),
        "new clients need the preferred recovery action: {status}"
    );
    assert!(
        next_actions
            .iter()
            .any(|action| action == "task_manage:resume"),
        "stale-schema clients need the compatibility recovery action to be discoverable: {status}"
    );
}

#[test]
fn git_revision_drift_does_not_advertise_baseline_acceptance_actions() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::write(workspace.join("README.md"), "initial\n").expect("write initial file");
    git(&workspace, &["init"]);
    git(&workspace, &["config", "user.email", "harness@example.com"]);
    git(&workspace, &["config", "user.name", "Harness Test"]);
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-m", "initial"]);

    let ctx = ToolContext::for_test(workspace.clone(), temp.path().join("harness"))
        .expect("create tool context")
        .with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "do not discover unsafe recovery"}),
    );
    assert_eq!(started["ok"], true, "start task: {started}");

    fs::write(workspace.join("README.md"), "external revision\n").expect("change file");
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-m", "external revision"]);

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["ok"], true, "status: {status}");
    assert_eq!(status["baseline_matches"], false, "status: {status}");
    assert_eq!(
        status["recovery"]["type"],
        "git_revision_drift",
        "status: {status}"
    );
    assert_eq!(
        status["recovery"]["recoverable_via_baseline_acceptance"],
        false,
        "status: {status}"
    );

    let next_actions = status["next_actions"]
        .as_array()
        .expect("next_actions array");
    assert!(
        next_actions
            .iter()
            .all(|action| action != "task_manage:refresh_baseline"),
        "refresh_baseline must never be suggested across Git revision drift: {status}"
    );
    assert!(
        next_actions
            .iter()
            .all(|action| action != "task_manage:resume"),
        "resume compatibility recovery must never be suggested across Git revision drift: {status}"
    );
    assert!(
        next_actions
            .iter()
            .any(|action| action == "task_manage:project_state"),
        "revision drift must direct clients to review project state: {status}"
    );
}
