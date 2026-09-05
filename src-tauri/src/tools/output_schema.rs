use serde_json::{json, Map, Value};

/// MCP / OpenAI `outputSchema` for `structuredContent`.
///
/// Root schemas stay GPT-compatible: object-only, no `oneOf` / `anyOf` / `$ref`.
/// Dispatch may attach planning and harness recovery fields to any tool result.
pub fn output_schema(name: &str) -> Value {
    let mut properties = common_envelope();
    merge(&mut properties, tool_properties(name));
    json!({
        "type": "object",
        "description": "MCP structuredContent returned by this tool. `ok` is the primary success flag.",
        "properties": properties,
        "required": ["ok"],
        "additionalProperties": false
    })
}

fn common_envelope() -> Map<String, Value> {
    props(&[
        ("ok", boolean("True when the tool completed without a structured error.")),
        ("status", string("High-level result status such as success, error, granted, or unsupported.")),
        ("summary", string("Short human-readable outcome.")),
        ("error", error_schema()),
        ("diagnostics", object_open("Optional diagnostic details.")),
        ("permission_request", object_open("Present when a permission or confirmation follow-up is required.")),
        (
            "planning_context",
            json!({
                "type": "object",
                "description": "Authoritative Goal/Plan snapshot attached by the desktop planning mode.",
                "additionalProperties": false,
                "properties": {
                    "mode": { "type": "string" },
                    "revision": { "type": "integer" },
                    "storage_path": { "type": "string" },
                    "goal": nullable_object("Currently focused Goal, or null."),
                    "plan": nullable_object("Currently focused Plan, or null.")
                }
            }),
        ),
        ("harness", object_open("Durable task/harness status attached on failed mutating operations.")),
        ("harness_mode", string("standalone when the operation ran without an active task.")),
        ("task_required", boolean("Whether a durable task is required for this class of operation.")),
        ("next_actions", string_array("Suggested next tool or user actions.")),
        ("recovery_hint", string("How to recover from a failed standalone operation.")),
        ("recovery", object_open("Structured recovery guidance, usually for capability discovery mismatches.")),
        ("operation_id", string("Workspace operation log id when the call was recorded.")),
        ("warnings", string_array("Non-fatal warnings produced by the tool.")),
    ])
}

fn tool_properties(name: &str) -> Map<String, Value> {
    match name {
        "server_info" => server_info_properties(),
        "capability_health_check" => capability_properties(),
        "check_exec_environment" => exec_environment_properties(),
        "exec_health_check" => exec_health_properties(),
        "get_default_cwd" | "set_default_cwd" => cwd_properties(),
        "read_file" => read_file_properties(),
        "list_dir" => list_dir_properties(),
        "list_files" => list_files_properties(),
        "search_text" | "grep_text" | "grep" => search_properties(),
        "apply_patch" | "patch_check" => patch_properties(),
        "exec_command" | "write_stdin" | "kill_session" => exec_session_properties(),
        "read_output" => read_output_properties(),
        "git_status" => git_status_properties(),
        "git_diff" => git_diff_properties(),
        "git_log" => git_log_properties(),
        "git_show" => git_show_properties(),
        "git_blame" => git_blame_properties(),
        "list_skills" => list_skills_properties(),
        "get_skill" => get_skill_properties(),
        "view_image" => view_image_properties(),
        "request_permissions" => request_permissions_properties(),
        "planning_state" => planning_state_properties(),
        "create_goal" | "update_goal" | "request_goal_review" => goal_result_properties(),
        "create_plan" | "update_plan" | "request_plan_review" => plan_result_properties(),
        "planning_manage" => {
            let mut properties = planning_state_properties();
            merge(&mut properties, goal_result_properties());
            merge(&mut properties, plan_result_properties());
            properties
        }
        "history_session_bootstrap" | "history_session_checkpoint" | "history_session_validate"
        | "history_session_search" | "history_session_read" | "history_manage" => history_properties(),
        "harness_status" => harness_status_properties(),
        "operation_log" => operation_log_properties(),
        "project_state" => project_state_properties(),
        "start_task" | "update_task" | "pause_task" | "resume_task" | "finish_task" | "task_context" => {
            task_result_properties()
        }
        "list_task_events" => list_task_events_properties(),
        "change_summary" => change_summary_properties(),
        "task_manage" => {
            let mut properties = harness_status_properties();
            merge(&mut properties, operation_log_properties());
            merge(&mut properties, project_state_properties());
            merge(&mut properties, task_result_properties());
            merge(&mut properties, list_task_events_properties());
            merge(&mut properties, change_summary_properties());
            properties
        }
        _ => Map::new(),
    }
}

fn server_info_properties() -> Map<String, Value> {
    props(&[
        ("server", string("Server implementation name.")),
        ("title", string("Human-readable server title.")),
        ("version", string("Desktop/runtime version.")),
        ("protocol_version", string("Advertised MCP protocol version.")),
        ("workspace", string("Configured workspace root.")),
        ("permission_mode", string("Current permission mode.")),
        ("default_cwd", string("Default relative working directory.")),
        ("network_allowed", boolean("Whether outbound network is allowed.")),
        ("tool_profile", string("Active tool profile.")),
        ("history_recording", boolean("Whether session recording is enabled.")),
        ("history_context_sessions", integer("Number of history sessions included in context.")),
        ("history_context_revision", nullable_integer("History context revision.")),
        ("context_audit", object_open("Recent MCP context blocks recorded for this session.")),
        ("auth_enabled", boolean("Whether MCP/Actions auth is enabled.")),
        ("auth_type", string("Configured auth type.")),
        ("endpoint_path", string("MCP HTTP path.")),
        ("tool_api", object_open("Stable Tool API descriptor.")),
        ("tools", string_array("Currently exposed tool names.")),
        ("tool_count", integer("Number of exposed tools.")),
    ])
}

fn capability_properties() -> Map<String, Value> {
    props(&[
        ("authentication", object_open("Authentication availability.")),
        ("authorization", object_open("Authorization mode.")),
        ("workspace", object_open("Workspace access status.")),
        ("capability", object_open("Advertised tool capability fingerprint.")),
        ("recommendation", string("What the client should do when tools appear missing.")),
    ])
}

fn exec_environment_properties() -> Map<String, Value> {
    props(&[
        ("workspace", string("Workspace root.")),
        ("permission_mode", string("Current permission mode.")),
        ("network_allowed", boolean("Whether outbound network is allowed.")),
        ("landlock_enabled", boolean("Whether OS landlock is enabled.")),
        ("filesystem_sandbox", object_open("Filesystem sandbox status.")),
        ("global_tmp_write", string("Temp write policy.")),
        ("workspace_exec_available", boolean("Whether workspace command execution is available.")),
        ("workspace_exec_sandbox_enforced", boolean("Whether an OS filesystem sandbox is enforced.")),
        ("workspace_exec_boundary", string("Execution boundary currently enforced.")),
        ("system_command_allowlist", string_array("Allowed system commands.")),
        ("configured_executable_paths", string_array("Configured extra executable paths.")),
        ("workspace_local_entries", object_open("Workspace-local script resolution policy.")),
        ("allowed_commands", string_array("Backward-compatible allowlist alias.")),
    ])
}

fn exec_health_properties() -> Map<String, Value> {
    let mut properties = exec_session_properties();
    merge(
        &mut properties,
        props(&[
            ("worker", object_open("Exec worker liveness.")),
            ("session_create", boolean("Whether a probe session was created.")),
            ("command_run", boolean("Whether the probe command ran.")),
            ("stdout_capture", boolean("Whether stdout capture worked.")),
            ("stderr_capture", boolean("Whether stderr capture worked.")),
            ("probe", object_open("Raw probe session snapshot.")),
            ("duration_ms", integer("Health-check duration in milliseconds.")),
        ]),
    );
    properties
}

fn cwd_properties() -> Map<String, Value> {
    props(&[
        ("workspace", string("Workspace root.")),
        ("default_cwd", string("Default cwd relative to the workspace.")),
        ("resolved_cwd", string("Absolute resolved cwd.")),
    ])
}

fn read_file_properties() -> Map<String, Value> {
    props(&[
        ("path", string("Workspace-relative file path.")),
        ("content", string("UTF-8 text slice that was read.")),
        ("encoding", string("Always utf-8 for this tool.")),
        ("start_line", integer("First returned 1-based line.")),
        ("end_line", integer("Last returned 1-based line.")),
        ("next_start_line", nullable_integer("Next line to read when content was truncated.")),
        ("total_lines", integer("Total lines in the file.")),
        ("total_bytes", integer("Total UTF-8 bytes in the file.")),
        ("bytes_read", integer("Bytes returned in this page.")),
        ("truncated", boolean("Whether the page was truncated.")),
        ("truncated_by", nullable_string("What caused truncation, if any.")),
    ])
}

fn list_dir_properties() -> Map<String, Value> {
    props(&[
        ("path", string("Listed directory.")),
        (
            "entries",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string" },
                    "path": { "type": "string" },
                    "type": { "type": "string" },
                    "size_bytes": { "type": "integer" },
                    "modified": nullable_string("Last modified timestamp if available."),
                    "is_hidden": { "type": "boolean" },
                    "is_ignored": { "type": "boolean" }
                }
            })),
        ),
        ("truncated", boolean("Whether the entry limit was reached.")),
    ])
}

fn list_files_properties() -> Map<String, Value> {
    props(&[
        ("path", string("Search root.")),
        (
            "files",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string" },
                    "type": { "type": "string" },
                    "size_bytes": { "type": "integer" },
                    "modified": nullable_string("Last modified timestamp if available.")
                }
            })),
        ),
        ("truncated", boolean("Whether the result limit was reached.")),
    ])
}

fn search_properties() -> Map<String, Value> {
    props(&[
        ("query", string("Search query that was executed.")),
        (
            "matches",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string" },
                    "line": { "type": "integer" },
                    "column": { "type": "integer" },
                    "preview": { "type": "string" },
                    "before": { "type": "array", "items": { "type": "string" } },
                    "after": { "type": "array", "items": { "type": "string" } }
                }
            })),
        ),
        ("total_matches", integer("Number of matches returned in this page.")),
        ("truncated", boolean("Whether the scan stopped early.")),
        ("max_file_bytes", integer("Per-file size cap used for the scan.")),
        ("skipped_large_files", integer("Files skipped for exceeding the size cap.")),
        ("skipped_binary_files", integer("Binary or non-UTF-8 files skipped.")),
    ])
}

fn patch_properties() -> Map<String, Value> {
    props(&[
        ("dry_run", boolean("True when the patch was only validated.")),
        ("preflight", boolean("True for patch_check or apply_patch dry_run.")),
        ("clean", boolean("True when the patch applied or would apply cleanly.")),
        ("change_id", string("Id assigned to a committed patch.")),
        (
            "affected_files",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "operation"],
                "properties": {
                    "path": { "type": "string" },
                    "operation": { "type": "string", "enum": ["add", "update", "delete"] }
                }
            })),
        ),
        ("files_created", string_array("Files created by a committed patch.")),
        ("files_modified", string_array("Files modified by a committed patch.")),
        ("files_deleted", string_array("Files deleted by a committed patch.")),
        ("would_create", string_array("Files a dry-run would create.")),
        ("would_modify", string_array("Files a dry-run would modify.")),
        ("would_delete", string_array("Files a dry-run would delete.")),
        ("recovery", json!({
            "description": "Patch recovery strategy, usually git, or structured recovery guidance.",
            "type": ["string", "object"],
            "additionalProperties": true
        })),
    ])
}

fn exec_session_properties() -> Map<String, Value> {
    props(&[
        ("command", string("Command that was executed.")),
        ("resolved_cwd", string("Absolute working directory.")),
        ("session_id", string("Server-managed session id when retained.")),
        ("interactive", boolean("Whether the session is a TTY/interactive session.")),
        ("stdin_open", boolean("Whether stdin is still open.")),
        ("termination_reason", string("Why the process stopped, or running.")),
        ("recoverable", boolean("Whether a retry is likely useful.")),
        ("suggestion", string("What to do next after this command result.")),
        ("exit_code", nullable_integer("Process exit code, or null if still running.")),
        ("transport_ok", boolean("Whether the session transport is healthy.")),
        ("command_ok", nullable_boolean("True when the command exited 0; null if still running.")),
        ("stdout", string("Captured stdout (possibly truncated).")),
        ("stderr", string("Captured stderr (possibly truncated).")),
        ("stdout_truncated", boolean("Whether stdout was truncated.")),
        ("stderr_truncated", boolean("Whether stderr was truncated.")),
        ("duration_ms", integer("Elapsed milliseconds.")),
        ("elapsed_ms", integer("Elapsed milliseconds alias.")),
        ("execution_mode", string("direct or native_builtin.")),
        ("command_runner", string("Runner used for native diagnostics.")),
        ("filesystem_scope", string("Requested filesystem scope.")),
        ("sandbox_enforced", boolean("Whether an OS sandbox was enforced.")),
        ("execution_boundary", string("Policy boundary that actually applied.")),
        ("child_process", boolean("Whether a child process was spawned.")),
        ("output_refs", object_open("Stable stdout/stderr paging refs.")),
        ("killed", boolean("True when kill_session terminated the process.")),
        ("evicted", boolean("True when the session was removed from the store.")),
    ])
}

fn read_output_properties() -> Map<String, Value> {
    props(&[
        ("output_ref", string("Requested output ref.")),
        ("stream_output_ref", string("Canonical stream-specific ref.")),
        ("stream", string("stdout or stderr.")),
        ("offset", integer("Actual byte offset used.")),
        ("requested_offset", integer("Caller-requested offset.")),
        ("limit", integer("Page size in bytes.")),
        ("content", string("Returned stream page.")),
        ("next_offset", nullable_integer("Next offset when more data remains.")),
        ("total_retained_bytes", integer("Bytes retained for this stream page.")),
        ("total_stream_bytes", integer("Total captured stream bytes.")),
        ("truncated", boolean("Whether another page is available.")),
    ])
}

fn git_status_properties() -> Map<String, Value> {
    props(&[
        ("is_repo", boolean("Whether the path is a git repository.")),
        ("branch", string("Current branch name.")),
        ("head", string("HEAD commit.")),
        ("upstream", string("Upstream branch if configured.")),
        ("ahead", integer("Commits ahead of upstream.")),
        ("behind", integer("Commits behind upstream.")),
        ("clean", boolean("True when the worktree has no porcelain entries.")),
        (
            "entries",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string" },
                    "index_status": { "type": "string" },
                    "worktree_status": { "type": "string" },
                    "original_path": { "type": "string" }
                }
            })),
        ),
        ("truncated", boolean("Whether the entry limit was reached.")),
    ])
}

fn git_diff_properties() -> Map<String, Value> {
    props(&[
        ("diff", string("Unified diff text.")),
        (
            "files",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string" },
                    "status": { "type": "string" },
                    "binary": { "type": "boolean" }
                }
            })),
        ),
        ("truncated", boolean("Whether the diff was truncated.")),
    ])
}

fn git_log_properties() -> Map<String, Value> {
    props(&[
        ("is_repo", boolean("Whether the workspace is a git repository.")),
        ("ref", string("Revision that was listed.")),
        ("path", string("Optional path filter.")),
        (
            "commits",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "hash": { "type": "string" },
                    "short_hash": { "type": "string" },
                    "author_name": { "type": "string" },
                    "author_email": { "type": "string" },
                    "author_date": { "type": "string" },
                    "subject": { "type": "string" }
                }
            })),
        ),
        ("truncated", boolean("Whether more commits exist beyond this page.")),
    ])
}

fn git_show_properties() -> Map<String, Value> {
    props(&[
        ("is_repo", boolean("Whether the workspace is a git repository.")),
        ("rev", string("Shown revision.")),
        ("content", string("git show output.")),
        (
            "files",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string" },
                    "status": { "type": "string" },
                    "binary": { "type": "boolean" }
                }
            })),
        ),
        ("truncated", boolean("Whether the output was truncated.")),
        ("output_bytes", integer("Returned output size.")),
    ])
}

fn git_blame_properties() -> Map<String, Value> {
    props(&[
        ("is_repo", boolean("Whether the workspace is a git repository.")),
        ("path", string("Blamed file.")),
        ("rev", nullable_string("Optional revision.")),
        ("start_line", integer("First blamed line.")),
        ("end_line", integer("Last blamed line.")),
        (
            "lines",
            array_of(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "commit": { "type": "string" },
                    "original_line": nullable_integer("Original line number."),
                    "line": nullable_integer("Current line number."),
                    "author": { "type": "string" },
                    "author_mail": { "type": "string" },
                    "author_time": { "type": ["integer", "string", "null"] },
                    "summary": { "type": "string" },
                    "content": { "type": "string" }
                }
            })),
        ),
        ("truncated", boolean("Whether the line limit was reached.")),
    ])
}

fn list_skills_properties() -> Map<String, Value> {
    props(&[
        ("skills", array_of(object_open("Skill descriptor without body."))),
        ("count", integer("Number of discovered skills.")),
    ])
}

fn get_skill_properties() -> Map<String, Value> {
    props(&[
        ("skill", object_open("Matched skill descriptor.")),
        ("content", string("Full SKILL.md body.")),
    ])
}

fn view_image_properties() -> Map<String, Value> {
    props(&[
        ("path", string("Workspace-relative image path.")),
        ("mime_type", string("Returned image MIME type.")),
        ("bytes", integer("Encoded image size.")),
        ("width", integer("Returned image width.")),
        ("height", integer("Returned image height.")),
        ("resized", boolean("Whether the image was resized.")),
        ("original", object_open("Original image metadata before resize.")),
        ("base64", string("Image bytes encoded as base64.")),
        ("data_url", string("data: URL for the returned image.")),
    ])
}

fn request_permissions_properties() -> Map<String, Value> {
    props(&[
        ("grant_id", nullable_string("Grant id when auto-granted in dangerous mode.")),
        ("expires_at", nullable_string("Grant expiry, if any.")),
        ("constraints", object_open("Grant constraints or requested arguments.")),
    ])
}

fn planning_state_properties() -> Map<String, Value> {
    props(&[
        ("storage_path", string("Project-local planning store path.")),
        ("state", object_open("Full Goal/Plan state document.")),
    ])
}

fn goal_result_properties() -> Map<String, Value> {
    props(&[
        ("goal", object_open("Created or updated Goal.")),
        ("focused", boolean("Whether the Goal is now focused.")),
        ("awaiting_human_acceptance", boolean("True after request_goal_review.")),
        ("storage_path", string("Project-local planning store path.")),
    ])
}

fn plan_result_properties() -> Map<String, Value> {
    props(&[
        ("plan", object_open("Created or updated Plan.")),
        ("focused", boolean("Whether the Plan is now focused.")),
        ("awaiting_human_acceptance", boolean("True after request_plan_review.")),
        ("storage_path", string("Project-local planning store path.")),
    ])
}

fn history_properties() -> Map<String, Value> {
    props(&[
        ("context_mode", string("compact when the bootstrap payload was reduced.")),
        ("index_only", boolean("True when only index metadata was returned.")),
        ("is_new_session", boolean("True when a new archive session was created.")),
        ("session_key", string("Stable history session key.")),
        ("session_key_source", string("How the session key was resolved.")),
        ("platform_conversation_id", boolean("True when the host conversation id was used.")),
        ("current_number", integer("Current archive number.")),
        ("current_path", string("Current archive path.")),
        ("created", boolean("Whether a new archive file was created.")),
        ("resumed", boolean("Whether an existing archive was resumed.")),
        ("initial_input_captured", boolean("Whether the first user input was stored.")),
        ("sequence_valid", boolean("Whether archive numbering is contiguous.")),
        ("history_count", integer("Number of archives.")),
        ("total_history_bytes", integer("Total archive bytes.")),
        ("state_revision", integer("Derived state revision.")),
        ("archive_revision", integer("Manifest revision.")),
        ("state", object_open("Bounded derived history state.")),
        ("state_truncated", boolean("True when bootstrap state was reduced.")),
        ("history_read_mode", string("How history should be read after bootstrap.")),
        ("persistence_mode", string("How checkpoints are persisted.")),
        ("assistant_instructions", string("Required follow-up workflow.")),
        ("required_next_actions", string_array("Bootstrap follow-up actions.")),
        ("checkpoint_policy", object_open("How to call history_session_checkpoint.")),
        ("search_guide", object_open("How to search and read archives.")),
        ("recorded", boolean("Whether a checkpoint was written.")),
        ("reason", string("Why a checkpoint was skipped.")),
        ("session_number", integer("Archive number that was checkpointed.")),
        ("path", string("Archive path.")),
        ("expected_path", string("Expected archive path for the next checkpoint.")),
        ("host_session_key_mismatch", boolean("True when the host session id changed.")),
        ("turn_id", string("Checkpoint turn id.")),
        ("revision", integer("Turn revision.")),
        ("supersedes", nullable_string("Previous revision this turn replaces.")),
        ("updated", boolean("Whether an existing turn was updated.")),
        ("duplicate_ignored", boolean("Whether a duplicate checkpoint was ignored.")),
        ("user_input_captured", boolean("Whether raw_user_input was stored.")),
        ("content_hash", string("SHA-256 of the archive or page.")),
        ("numbers", array_of(json!({ "type": "integer" }))),
        ("missing_numbers", array_of(json!({ "type": "integer" }))),
        ("duplicate_session_keys", string_array("Duplicate session keys found during validation.")),
        ("invalid_files", string_array("Invalid history files.")),
        ("empty_files", string_array("Empty history files.")),
        ("latest_number", nullable_integer("Latest archive number.")),
        ("latest_path", nullable_string("Latest archive path.")),
        ("archive_count", integer("Archive count from validation.")),
        ("total_archive_bytes", integer("Total bytes from validation.")),
        ("index_status", string("Derived index status.")),
        ("manifest_status", string("Derived manifest status.")),
        ("state_status", string("Derived state status.")),
        ("repaired", boolean("Whether validate repaired derived files.")),
        ("query", string("History search query.")),
        ("total_matches", integer("Total ranked matches.")),
        ("cursor", integer("Current search or read cursor.")),
        ("limit", integer("Search page size.")),
        ("next_cursor", nullable_integer("Next search or read cursor.")),
        ("results", array_of(object_open("Ranked history search hit."))),
        ("number", integer("Archive number that was read.")),
        ("content", string("Archive page text.")),
        ("total_bytes", integer("Complete archive size.")),
        ("max_bytes", integer("Requested page size.")),
    ])
}

fn harness_status_properties() -> Map<String, Value> {
    props(&[
        ("schema_version", integer("Harness status schema version.")),
        ("workspace_id", string("Harness workspace id.")),
        ("task_id", nullable_string("Active task id, if any.")),
        ("task_state", nullable_string("Active task state.")),
        ("task_updated_at", nullable_string("When the active task last changed.")),
        ("writable", boolean("Whether the workspace is currently writable.")),
        ("reason", string("Human-readable harness reason.")),
        ("recoverable", boolean("Whether the current harness block is recoverable.")),
        ("branch", nullable_string("Current git branch.")),
        ("head", nullable_string("Current git HEAD.")),
        ("baseline_matches", nullable_boolean("Whether the workspace still matches the task baseline.")),
        ("capabilities", object_open("Named capability statuses.")),
    ])
}

fn operation_log_properties() -> Map<String, Value> {
    props(&[
        ("operations", array_of(object_open("Recorded workspace operation."))),
        ("next_cursor", integer("Cursor for the next operation page.")),
    ])
}

fn project_state_properties() -> Map<String, Value> {
    props(&[
        ("schema_version", integer("Project state schema version.")),
        ("workspace_id", string("Harness workspace id.")),
        ("branch", nullable_string("Current git branch.")),
        ("head", nullable_string("Current git HEAD.")),
        ("clean", boolean("Whether the worktree is clean.")),
        ("files", array_of(object_open("Per-file project state."))),
        ("total_files", integer("Total files considered.")),
        ("truncated", boolean("Whether the file list was truncated.")),
        ("active_task_id", nullable_string("Active task id.")),
        ("task", nullable_object("Active task, if any.")),
        ("recent_events", integer("Recent event count.")),
    ])
}

fn task_result_properties() -> Map<String, Value> {
    let mut properties = change_summary_properties();
    merge(
        &mut properties,
        props(&[
            ("task", nullable_object("Task session after the action.")),
            ("next", string_array("Suggested follow-up tools.")),
            ("message", string("Status message when no task is active.")),
            ("events", array_of(object_open("Recent task events."))),
            ("truncated", boolean("Whether task context events were truncated.")),
            ("change_summary", object_open("Nested change summary after finish_task.")),
        ]),
    );
    properties
}

fn list_task_events_properties() -> Map<String, Value> {
    props(&[
        ("events", array_of(object_open("Task event page."))),
        ("next_cursor", integer("Cursor for the next event page.")),
    ])
}

fn change_summary_properties() -> Map<String, Value> {
    props(&[
        ("task_id", string("Task that was summarized.")),
        ("objective", string("Task objective.")),
        ("why", object_open("Why the change was made.")),
        ("files", array_of(object_open("Changed files."))),
        ("evidence", array_of(object_open("Supporting task events."))),
        ("verification", array_of(json!({ "type": ["string", "object"] }))),
        ("risks", array_of(json!({ "type": ["string", "object"] }))),
        ("rollback_capability", string("Current rollback capability.")),
    ])
}

fn error_schema() -> Value {
    json!({
        "type": "object",
        "description": "Structured tool error.",
        "additionalProperties": false,
        "properties": {
            "code": { "type": "string" },
            "message": { "type": "string" },
            "category": { "type": "string" },
            "retryable": { "type": "boolean" },
            "details": {
                "type": "object",
                "additionalProperties": true
            }
        }
    })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn boolean(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

fn integer(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

fn string_array(description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": { "type": "string" }
    })
}

fn object_open(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": true
    })
}

fn array_of(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn nullable_string(description: &str) -> Value {
    json!({ "type": ["string", "null"], "description": description })
}

fn nullable_integer(description: &str) -> Value {
    json!({ "type": ["integer", "null"], "description": description })
}

fn nullable_boolean(description: &str) -> Value {
    json!({ "type": ["boolean", "null"], "description": description })
}

fn nullable_object(description: &str) -> Value {
    json!({
        "type": ["object", "null"],
        "description": description,
        "additionalProperties": true
    })
}

fn props(entries: &[(&str, Value)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

fn merge(target: &mut Map<String, Value>, source: Map<String, Value>) {
    for (key, value) in source {
        target.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::output_schema;
    use crate::tools::registry::{list_tools_for_profile, P0_TOOLS};

    #[test]
    fn every_declared_tool_has_a_gpt_compatible_output_schema() {
        for (name, ..) in P0_TOOLS {
            let schema = output_schema(name);
            assert_eq!(schema["type"], "object", "{name} output type");
            assert_eq!(schema["required"], serde_json::json!(["ok"]), "{name} required");
            assert_eq!(
                schema["additionalProperties"], false,
                "{name} additionalProperties"
            );
            assert!(schema["properties"]["ok"].is_object(), "{name} ok");
            assert!(
                schema["properties"]["planning_context"].is_object(),
                "{name} planning_context"
            );
            assert!(schema.get("oneOf").is_none(), "{name} oneOf");
            assert!(schema.get("anyOf").is_none(), "{name} anyOf");
            assert!(schema.get("$ref").is_none(), "{name} ref");
        }
    }

    #[test]
    fn advertised_tools_include_output_schema() {
        for profile in ["core", "compact", "advanced"] {
            for tool in list_tools_for_profile(profile) {
                let name = tool["name"].as_str().expect("tool name");
                assert_eq!(
                    tool["outputSchema"],
                    output_schema(name),
                    "{profile}/{name} outputSchema"
                );
            }
        }
    }
}
