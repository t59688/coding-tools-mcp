use coding_tools_mcp_desktop_lib::tools::list_tools_for_profile;

#[test]
fn compact_task_manage_catalog_exposes_baseline_recovery_contract() {
    let tools = list_tools_for_profile("compact");
    let task_manage = tools
        .iter()
        .find(|tool| tool["name"] == "task_manage")
        .expect("compact profile must expose task_manage");
    let schema = &task_manage["inputSchema"];
    let actions = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("task_manage action enum");

    assert!(
        actions.iter().any(|action| action == "refresh_baseline"),
        "tools/list contract must expose the preferred baseline recovery action: {schema}"
    );
    assert!(
        actions.iter().any(|action| action == "resume"),
        "resume must remain available for stale-schema compatibility: {schema}"
    );
    assert!(
        schema["properties"]["change_id"].is_object(),
        "change_id is the optimistic-concurrency token used by stale clients: {schema}"
    );
    assert!(
        schema["properties"]["summary"].is_object(),
        "summary carries the reviewed-baseline audit reason: {schema}"
    );
}

#[test]
fn compact_profile_has_single_stable_task_entry_point() {
    let tools = list_tools_for_profile("compact");
    let names = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"task_manage"));
    assert!(!names.contains(&"refresh_baseline"));
    assert!(!names.contains(&"resume_task"));
    assert!(!names.contains(&"project_state"));
}
