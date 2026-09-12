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

fn init_git(workspace: &std::path::Path) {
    git(workspace, &["init"]);
    git(workspace, &["config", "user.email", "harness@example.com"]);
    git(workspace, &["config", "user.name", "Harness Test"]);
    git(workspace, &["add", "README.md"]);
    git(workspace, &["commit", "-m", "initial"]);
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
    assert_eq!(status["recovery"]["requires_summary"], true);
    assert!(status["recovery"]["change_id"].as_str().is_some());

    let next_actions = status["next_actions"]
        .as_array()
        .expect("next_actions array");
    assert!(next_actions
        .iter()
        .any(|action| action == "task_manage:refresh_baseline"));
    assert!(next_actions
        .iter()
        .any(|action| action == "task_manage:resume"));
}

#[test]
fn same_branch_committed_head_drift_remains_review_recoverable() {
    let (temp, workspace, _ctx) = fixture();
    init_git(&workspace);
    let ctx = ToolContext::for_test(workspace.clone(), temp.path().join("git-harness"))
        .expect("create git context")
        .with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "discover committed recovery"}),
    );
    assert_eq!(started["ok"], true);

    fs::write(workspace.join("README.md"), "committed change\n").expect("change file");
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-m", "same branch change"]);

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["baseline_matches"], false, "status: {status}");
    assert_eq!(status["recovery"]["type"], "reviewed_worktree_baseline");
    assert_eq!(status["recovery"]["head_changed"], true);
    assert_eq!(
        status["recovery"]["recoverable_via_baseline_acceptance"], true
    );
    assert!(status["next_actions"]
        .as_array()
        .expect("actions")
        .iter()
        .any(|action| action == "task_manage:refresh_baseline"));
    assert!(status["next_actions"]
        .as_array()
        .expect("actions")
        .iter()
        .any(|action| action == "task_manage:resume"));
}

#[test]
fn branch_revision_drift_does_not_advertise_baseline_acceptance_actions() {
    let (temp, workspace, _ctx) = fixture();
    init_git(&workspace);
    let ctx = ToolContext::for_test(workspace.clone(), temp.path().join("branch-harness"))
        .expect("create git context")
        .with_tool_profile("compact");
    let started = call_tool(
        &ctx,
        "task_manage",
        &json!({"action": "start", "objective": "do not discover unsafe recovery"}),
    );
    assert_eq!(started["ok"], true, "start task: {started}");

    git(&workspace, &["checkout", "-b", "other-branch"]);
    fs::write(workspace.join("README.md"), "other branch\n").expect("change file");

    let status = call_tool(&ctx, "task_manage", &json!({"action": "status"}));
    assert_eq!(status["ok"], true, "status: {status}");
    assert_eq!(status["baseline_matches"], false, "status: {status}");
    assert_eq!(status["recovery"]["type"], "git_revision_drift");
    assert_eq!(
        status["recovery"]["recoverable_via_baseline_acceptance"], false
    );

    let next_actions = status["next_actions"]
        .as_array()
        .expect("next_actions array");
    assert!(next_actions
        .iter()
        .all(|action| action != "task_manage:refresh_baseline"));
    assert!(next_actions
        .iter()
        .all(|action| action != "task_manage:resume"));
    assert!(next_actions
        .iter()
        .any(|action| action == "task_manage:project_state"));
}
