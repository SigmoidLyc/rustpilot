use super::*;

#[test]
fn title_is_trimmed_and_bounded() {
    let title = make_title("  first   task   with   spacing  ");
    assert_eq!(title, "first task with spacing");

    let long = make_title(&"a".repeat(100));
    assert_eq!(long.chars().count(), 59);
    assert!(long.ends_with("..."));
}

#[test]
fn max_agent_steps_default_to_100_and_preserve_custom_values() {
    assert_eq!(default_settings().max_steps, 100);
    assert_eq!(normalize_max_steps(0), 1);
    assert_eq!(normalize_max_steps(250), 250);
}

#[test]
fn legacy_task_summaries_default_to_unarchived() {
    let summary: TaskSummary = serde_json::from_value(json!({
        "id": "task-1",
        "title": "Legacy task",
        "status": "completed",
        "updated_at": 1,
        "demo_mode": true,
        "error": null
    }))
    .expect("legacy task summary should remain readable");

    assert!(!summary.archived);
}

#[test]
fn legacy_settings_and_approval_records_accept_missing_authorization_fields() {
    let settings: PersistedSettings = serde_json::from_value(json!({
        "api_base_url": "https://api.example.test/v1",
        "model": "legacy-model",
        "max_steps": 12,
        "timeout_secs": 30
    }))
    .expect("legacy settings should remain readable");
    assert!(settings.approval_mode.is_none());
    assert!(settings.approval_rules.is_empty());

    let request: ApprovalRequest = serde_json::from_value(json!({
        "id": "approval-1",
        "task_id": "task-1",
        "tool_name": "rust_shell",
        "reason": "legacy reason",
        "details": "{}",
        "created_at": 1,
        "status": "pending"
    }))
    .expect("legacy approval request should remain readable");
    assert!(!request.rememberable);
    assert!(request.remember_action.is_none());
    assert!(request.remember_pattern.is_none());
}

fn test_task(task_id: &str) -> Task {
    let message_id = format!("{task_id}-message");
    Task {
        id: task_id.to_string(),
        title: "Test task".to_string(),
        prompt: "test prompt".to_string(),
        workspace: std::env::current_dir().unwrap().display().to_string(),
        status: AgentStatus::Idle,
        created_at: 1,
        updated_at: 1,
        demo_mode: false,
        archived: false,
        agent_name: default_agent_name(),
        agent_kind: default_agent_kind(),
        messages: vec![TaskMessage {
            id: message_id,
            task_id: task_id.to_string(),
            role: "assistant".to_string(),
            content: String::new(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 1,
            streaming: false,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        }],
        memory: Vec::new(),
        plans: Vec::new(),
        active_plan_id: None,
        steps: Vec::new(),
        tool_calls: Vec::new(),
        approval_requests: Vec::new(),
        llm_usage: llm::TokenUsage::default(),
        final_answer: None,
        error: None,
        event_seq: 0,
        persistence_revision: 1,
    }
}

#[test]
fn invalid_task_files_are_preserved_and_recovery_files_are_read() {
    let directory =
        std::env::temp_dir().join(format!("rustpilot-task-recovery-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let path = directory.join(LEGACY_TASK_FILE);
    fs::write(&path, "{ invalid json").expect("invalid task file should be written");
    fs::write(legacy_task_temp_path(&path), "[]").expect("recovery file should be written");

    let tasks = load_legacy_task_records(&path).expect("recovery file should be readable");
    assert!(tasks.is_empty());
    assert!(fs::read_dir(&directory)
        .expect("test directory should be readable")
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains("corrupt-")));

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn legacy_tasks_migrate_to_sqlite_and_are_removed_after_commit() {
    let directory =
        std::env::temp_dir().join(format!("rustpilot-task-migration-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let path = directory.join(LEGACY_TASK_FILE);
    let task = test_task("task-1");
    fs::write(
        &path,
        serde_json::to_string(&vec![task.clone()]).expect("task should be encoded"),
    )
    .expect("legacy task file should be written");

    let loaded = load_task_store(&directory).expect("legacy task should migrate");
    assert_eq!(loaded.tasks.len(), 1);
    assert_eq!(loaded.tasks["task-1"].prompt, task.prompt);
    assert!(task_database_path(&directory).exists());
    assert!(!path.exists());
    assert!(!legacy_task_temp_path(&path).exists());
    assert!(!legacy_task_backup_path(&path).exists());

    let reopened = load_task_store(&directory).expect("migrated task should reopen");
    assert_eq!(reopened.tasks["task-1"].prompt, "test prompt");

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn sqlite_replays_stream_events_without_rewriting_the_task_snapshot() {
    let directory = std::env::temp_dir().join(format!("rustpilot-task-events-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let task = test_task("task-1");
    let mut connection =
        open_task_database(&task_database_path(&directory)).expect("task database should open");
    let transaction = connection
        .transaction()
        .expect("event transaction should begin");
    insert_task_state(&transaction, &task).expect("task snapshot should insert");
    insert_stream_event(
        &transaction,
        "task-1",
        "task-1-message",
        &PersistedStreamEvent::TextDelta {
            delta: "hello".to_string(),
        },
    )
    .expect("stream event should insert");
    transaction
        .commit()
        .expect("event transaction should commit");
    drop(connection);

    let loaded = load_task_store(&directory).expect("stream event should replay");
    assert_eq!(loaded.tasks["task-1"].messages[0].content, "hello");
    assert!(loaded.event_bytes["task-1"] > 0);

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn sqlite_replays_reasoning_stream_events_without_losing_order() {
    let directory =
        std::env::temp_dir().join(format!("rustpilot-reasoning-events-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let task = test_task("task-1");
    let mut connection =
        open_task_database(&task_database_path(&directory)).expect("task database should open");
    let transaction = connection
        .transaction()
        .expect("event transaction should begin");
    insert_task_state(&transaction, &task).expect("task snapshot should insert");
    insert_stream_event(
        &transaction,
        "task-1",
        "task-1-message",
        &PersistedStreamEvent::ReasoningDelta {
            delta: "thinking".to_string(),
        },
    )
    .expect("reasoning event should insert");
    insert_stream_event(
        &transaction,
        "task-1",
        "task-1-message",
        &PersistedStreamEvent::ReasoningOpaque {
            value: "copilot-signature".to_string(),
        },
    )
    .expect("opaque event should insert");
    insert_stream_event(
        &transaction,
        "task-1",
        "task-1-message",
        &PersistedStreamEvent::TextDelta {
            delta: "answer".to_string(),
        },
    )
    .expect("text event should insert");
    transaction
        .commit()
        .expect("event transaction should commit");
    drop(connection);

    let loaded = load_task_store(&directory).expect("stream events should replay");
    let message = &loaded.tasks["task-1"].messages[0];
    assert_eq!(message.reasoning, "thinking");
    assert_eq!(
        message.reasoning_opaque.as_deref(),
        Some("copilot-signature")
    );
    assert_eq!(message.content, "answer");
    assert!(matches!(message.parts[0], AssistantPart::Reasoning { .. }));
    assert!(matches!(message.parts[1], AssistantPart::Text { .. }));

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn task_event_pages_replay_after_a_cursor() {
    let directory =
        std::env::temp_dir().join(format!("rustpilot-task-event-pages-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let task = test_task("task-1");
    let mut connection =
        open_task_database(&task_database_path(&directory)).expect("task database should open");
    let transaction = connection
        .transaction()
        .expect("event transaction should begin");
    insert_task_state(&transaction, &task).expect("task snapshot should insert");
    insert_task_event(
        &transaction,
        "task-1",
        &PendingTaskEvent::Task {
            revision: 2,
            event: "task_status".to_string(),
            payload: json!({
                "task_id": "task-1",
                "status": "executing",
                "updated_at": 2,
                "error": null
            }),
        },
    )
    .expect("task event should insert");
    insert_stream_event(
        &transaction,
        "task-1",
        "task-1-message",
        &PersistedStreamEvent::TextDelta {
            delta: "hello".to_string(),
        },
    )
    .expect("stream event should insert");
    transaction
        .commit()
        .expect("event transaction should commit");
    drop(connection);

    let first = task_events::read_page(&directory, "task-1", None)
        .expect("initial event page should be readable");
    assert!(first.snapshot.is_some());
    assert_eq!(first.events.len(), 2);
    assert!(!first.reset);

    let second = task_events::read_page(&directory, "task-1", Some(first.events[0].seq))
        .expect("cursor event page should be readable");
    assert!(second.snapshot.is_none());
    assert_eq!(second.events.len(), 1);
    assert_eq!(second.events[0].seq, first.events[1].seq);

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn pending_snapshot_and_stream_delta_are_applied_once() {
    let base = test_task("task-1");
    let stream_event = PersistedStreamEvent::TextDelta {
        delta: "hello".to_string(),
    };
    let event = PendingTaskEvent::Stream {
        revision: 2,
        message_id: "task-1-message".to_string(),
        event: stream_event.clone(),
    };
    let stream_write = PendingTaskWrite::Events {
        events: vec![event.clone()],
    };
    let merged = merge_pending_task_writes(
        stream_write,
        PendingTaskWrite::Upsert {
            task: {
                let mut task = base.clone();
                apply_persisted_stream_event(&mut task, "task-1", "task-1-message", &stream_event)
                    .expect("stream event should apply");
                task.persistence_revision = 2;
                Box::new(task)
            },
            events: Vec::new(),
        },
    );
    let writes = PendingTaskWrites {
        by_task: HashMap::from([("task-1".to_string(), merged)]),
    };
    let durable = HashMap::from([("task-1".to_string(), base)]);
    let projected = project_task_writes(&durable, &HashMap::new(), &writes)
        .expect("merged task write should project");
    assert_eq!(
        projected.tasks["task-1"]
            .as_ref()
            .expect("task should be projected")
            .messages[0]
            .content,
        "hello"
    );
}

#[test]
fn stream_events_compact_back_into_the_task_snapshot() {
    let directory =
        std::env::temp_dir().join(format!("rustpilot-task-compaction-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let task = test_task("task-1");
    let events = (0..5000)
        .map(|revision| PendingTaskEvent::Stream {
            revision: revision + 2,
            message_id: "task-1-message".to_string(),
            event: PersistedStreamEvent::TextDelta {
                delta: "x".to_string(),
            },
        })
        .collect::<Vec<_>>();
    let writes = PendingTaskWrites {
        by_task: HashMap::from([("task-1".to_string(), PendingTaskWrite::Events { events })]),
    };
    let durable = HashMap::from([("task-1".to_string(), task.clone())]);
    let projected = project_task_writes(&durable, &HashMap::new(), &writes)
        .expect("stream batch should project");
    assert!(projected.compacted.contains("task-1"));
    let connection =
        open_task_database(&task_database_path(&directory)).expect("task database should open");
    let (_, _, _, _, result) = commit_task_writes(connection, writes, projected);
    result.expect("compaction transaction should commit");
    let loaded = load_task_store(&directory).expect("compacted database should reopen");
    assert_eq!(loaded.tasks["task-1"].messages[0].content.len(), 5000);
    assert_eq!(loaded.event_bytes.get("task-1").copied(), Some(0));
    let page = task_events::read_page(&directory, "task-1", Some(0))
        .expect("compacted event page should be readable");
    assert!(page.reset);
    assert!(page.snapshot.is_some());
    assert!(page.events.is_empty());

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn sqlite_delete_commit_does_not_resurrect_deleted_tasks() {
    let directory = std::env::temp_dir().join(format!("rustpilot-task-delete-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).expect("test directory should be created");
    let task = test_task("task-1");
    let mut connection =
        open_task_database(&task_database_path(&directory)).expect("task database should open");
    let transaction = connection
        .transaction()
        .expect("task transaction should begin");
    insert_task_state(&transaction, &task).expect("task snapshot should insert");
    transaction
        .commit()
        .expect("task transaction should commit");

    let writes = PendingTaskWrites {
        by_task: HashMap::from([(
            "task-1".to_string(),
            PendingTaskWrite::Delete {
                revision: 2,
                events: Vec::new(),
            },
        )]),
    };
    let projected = ProjectedTaskChanges {
        tasks: HashMap::from([("task-1".to_string(), None)]),
        event_bytes: HashMap::from([("task-1".to_string(), 0)]),
        compacted: HashSet::new(),
    };
    let (_, _, _, _, result) = commit_task_writes(connection, writes, projected);
    result.expect("delete transaction should commit");
    assert!(load_task_store(&directory)
        .expect("deleted database should reopen")
        .tasks
        .is_empty());

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn demo_capability_questions_do_not_run_unrelated_tools() {
    assert!(demo_tool_calls("What can you do?").is_empty());
    assert!(demo_tool_calls("What can you do?").is_empty());
}

#[test]
fn demo_answers_summarize_tool_evidence() {
    let result = ToolResult {
        id: "result-1".to_string(),
        task_id: "task-1".to_string(),
        tool_call_id: "call-1".to_string(),
        status: ToolCallStatus::Completed,
        output: Some("file src/main.rs\ndir src".to_string()),
        error: None,
        duration_ms: Some(2),
    };
    let answer = demo_answer("Inspect the project", &[("rust_files".to_string(), result)]);
    assert!(answer.contains("2 files and directories"));
    assert!(!answer.contains("src/main.rs"));
}

#[test]
fn status_uses_snake_case() {
    assert_eq!(
        serde_json::to_string(&AgentStatus::WaitingApproval).expect("status should serialize"),
        "\"waiting_approval\""
    );
}

#[test]
fn shell_and_file_writes_are_high_risk() {
    assert!(is_high_risk("rust_shell", &json!({"command": "echo hi"})));
    assert!(is_high_risk(
        "rust_files",
        &json!({"operation": "write", "path": "a.txt"})
    ));
    assert!(!is_high_risk(
        "rust_files",
        &json!({"operation": "list", "path": "."})
    ));
}

#[test]
fn explicit_file_outputs_are_high_risk_and_show_resolved_scope() {
    let visualization = json!({
        "kind": "bar",
        "output_path": "artifacts/chart.json"
    });
    assert!(is_high_risk(
        "rust_visualization_preparation",
        &visualization
    ));
    assert!(!is_high_risk(
        "rust_visualization_preparation",
        &json!({"kind": "bar"})
    ));

    let screenshot = json!({
        "action": "screenshot",
        "path": "artifacts/screen.bmp"
    });
    assert!(is_high_risk("rust_computer_use", &screenshot));

    let details = approval_details("rust_visualization_preparation", &visualization);
    assert!(details.contains("_rustpilot_path_authorization"));
    assert!(details.contains("resolved"));
    assert!(details.contains("workspace"));
}

#[test]
fn completion_url_accepts_common_base_url_shapes() {
    assert_eq!(
        llm::OpenAiCompatibleClient::completion_url("https://example.test/v1"),
        "https://example.test/v1/chat/completions"
    );
    assert_eq!(
        llm::OpenAiCompatibleClient::completion_url("https://example.test/v1/"),
        "https://example.test/v1/chat/completions"
    );
    assert_eq!(
        llm::OpenAiCompatibleClient::completion_url("https://example.test/v1/chat/completions",),
        "https://example.test/v1/chat/completions"
    );
}

#[test]
fn every_registered_tool_uses_rust_prefix() {
    let definitions = tool_definitions();
    assert!(definitions.len() >= 20);
    for definition in definitions {
        let name = definition["function"]["name"]
            .as_str()
            .expect("tool name should be a string");
        assert!(name.starts_with("rust_"), "unexpected tool name: {name}");
    }
}

#[test]
fn tool_snapshot_order_and_hash_are_stable() {
    let first = vec![
        json!({"function": {"name": "rust_b", "parameters": {"b": 1, "a": 2}}}),
        json!({"function": {"name": "rust_a", "parameters": {"nested": {"z": true, "x": false}}}}),
    ];
    let second = vec![
        json!({"function": {"name": "rust_b", "parameters": {"a": 2, "b": 1}}}),
        json!({"function": {"name": "rust_a", "parameters": {"nested": {"x": false, "z": true}}}}),
    ];
    assert_eq!(tool_schema_hash(&first), tool_schema_hash(&second));

    let state = AppState::new();
    let snapshot = tool_definitions_for_state(&state);
    let names = snapshot
        .definitions
        .iter()
        .filter_map(|definition| definition.pointer("/function/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(snapshot.schema_hash.len(), 32);
}

#[test]
fn agent_tool_snapshots_enforce_allowlists_and_dynamic_mcp_capabilities() {
    let state = AppState::new();
    mcp_tool::register_tools(
        &state,
        "demo",
        &json!({
            "result": {
                "tools": [{
                    "name": "read_rows",
                    "description": "Read rows",
                    "inputSchema": {"type": "object"}
                }]
            }
        }),
    )
    .expect("MCP tools should register");

    let browser = agents::AgentSpec::for_kind(agent::AgentKind::Browser, ".");
    let browser_snapshot = tool_definitions_for_agent(&state, &browser);
    let browser_names = browser_snapshot
        .definitions
        .iter()
        .filter_map(|definition| definition.pointer("/function/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(browser_names, vec!["rust_browser_use", "rust_terminate"]);
    assert!(!browser.allows_tool("rust_mcp"));
    assert!(!browser.allows_tool("rust_mcp_demo_read_rows"));
    assert!(!browser.allows_tool("rust_sandbox_shell"));
    assert!(browser.allows_tool("rust_terminate"));
    assert!(!agent_has_tool(&state, &browser, "rust_mcp_demo_read_rows"));

    let mcp = agents::AgentSpec::for_kind(agent::AgentKind::Mcp, ".");
    let mcp_snapshot = tool_definitions_for_agent(&state, &mcp);
    let mcp_names = mcp_snapshot
        .definitions
        .iter()
        .filter_map(|definition| definition.pointer("/function/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        mcp_names,
        vec!["rust_mcp", "rust_mcp_demo_read_rows", "rust_terminate"]
    );
    assert!(mcp.allows_tool("rust_mcp_demo_read_rows"));
    assert!(agent_has_tool(&state, &mcp, "rust_mcp_demo_read_rows"));
    assert!(!agent_has_tool(&state, &mcp, "rust_mcp_demo_missing"));
    assert_eq!(
        mcp_snapshot.schema_hash.as_ref(),
        tool_schema_hash(mcp_snapshot.definitions.as_ref())
    );
    assert_ne!(browser_snapshot.schema_hash, mcp_snapshot.schema_hash);
}

#[test]
fn identical_mcp_refresh_does_not_invalidate_tool_snapshot() {
    let state = AppState::new();
    let response = json!({
        "result": {
            "tools": [
                {
                    "name": "z_tool",
                    "description": "Z",
                    "inputSchema": {"type": "object"}
                },
                {
                    "name": "a_tool",
                    "description": "A",
                    "inputSchema": {"type": "object"}
                }
            ]
        }
    });
    mcp_tool::register_tools(&state, "demo", &response).unwrap();
    let first_revision = state
        .mcp_tools_revision
        .load(std::sync::atomic::Ordering::Acquire);
    let first_snapshot = tool_definitions_for_state(&state);
    mcp_tool::register_tools(&state, "demo", &response).unwrap();
    let second_snapshot = tool_definitions_for_state(&state);
    assert_eq!(
        state
            .mcp_tools_revision
            .load(std::sync::atomic::Ordering::Acquire),
        first_revision
    );
    assert_eq!(first_snapshot.schema_hash, second_snapshot.schema_hash);
}

#[test]
fn system_prompt_is_split_into_stable_cacheable_parts() {
    let workspace = workspace_root();
    let first = system_prompt_parts("manus", &workspace.to_string_lossy());
    let second = system_prompt_parts("manus", &workspace.to_string_lossy());
    assert_eq!(first, second);
    assert!(!first.0.is_empty());
    assert!(!first.1.is_empty());
}

#[test]
fn cargo_output_directories_are_not_workspace_candidates() {
    let profile = cargo_profile_directory().expect("test executable should have a Cargo profile");
    assert!(is_cargo_target_directory(&profile));
    assert!(is_rustpilot_artifact_directory(&profile));
    assert!(is_cargo_target_directory(&profile.join("nested")));
    assert!(!is_cargo_target_directory(Path::new("src")));
}

#[test]
fn agent_kind_tracks_agent_specializations() {
    assert_eq!(
        infer_agent_kind("Analyze this CSV and draw a chart"),
        "data_analysis"
    );
    assert_eq!(infer_agent_kind("Fix this code bug"), "swe");
    assert_eq!(infer_agent_kind("Open the browser page"), "browser");
    assert_eq!(infer_agent_kind("Ordinary task"), "manus");
}

#[test]
fn html_tools_extract_text_and_resolve_links() {
    let html = "<html><script>ignore()</script><title>Page</title><body>Hello <a href='/next'>Next page</a></body></html>";
    assert_eq!(html_title(html), "Page");
    assert_eq!(html_text(html), "Page Hello Next page");
    assert_eq!(
        html_links(html, "https://example.test/start")[0].1,
        "https://example.test/next"
    );
}

#[test]
fn planning_format_reports_expected_statuses() {
    let plan = AgentPlan {
        id: "p1".to_string(),
        title: "Inspect".to_string(),
        steps: vec![AgentPlanStep {
            id: "s1".to_string(),
            title: "Read".to_string(),
            description: "Read the input".to_string(),
            status: PlanStepStatus::InProgress,
            notes: "started".to_string(),
        }],
        created_at: 0,
        updated_at: 0,
    };
    let formatted = format_plan(&plan);
    assert!(formatted.contains("0/1 completed"));
    assert!(formatted.contains("[>] Read"));
    assert!(formatted.contains("notes: started"));
}

#[test]
fn browser_and_mcp_mutations_require_approval() {
    assert!(is_high_risk(
        "rust_browser_use",
        &json!({"action": "click", "text": "Submit"})
    ));
    assert!(!is_high_risk(
        "rust_browser_use",
        &json!({"action": "extract"})
    ));
    assert!(is_high_risk("rust_mcp", &json!({"action": "call_tool"})));
}

#[test]
fn csv_parser_handles_quoted_commas_and_mcp_names_are_safe() {
    let (headers, rows) = data_tool::table_from_contents("sample.csv", "name,value\n\"A, B\",2\n")
        .expect("CSV should parse");
    assert_eq!(headers, vec!["name", "value"]);
    assert_eq!(rows[0][0], "A, B");
    assert_eq!(
        mcp_tool::sanitize_name("Weather Tool/v2"),
        "weather_tool_v2"
    );
}

#[test]
fn screenshot_and_mcp_payload_helpers_are_real_encodings() {
    assert_eq!(base64_encode(b"Man"), "TWFu");
    assert_eq!(base64_encode(b"M"), "TQ==");
    let response =
        crate::mcp_transport::parse_response("event: message\ndata: {\"result\":{\"tools\":[]}}\n")
            .expect("SSE MCP response should parse");
    assert_eq!(response["result"]["tools"], json!([]));
}

#[test]
fn chart_png_writer_emits_a_valid_png_signature() {
    let path = std::env::temp_dir().join(format!("rustpilot-chart-{}.png", Uuid::new_v4()));
    data_tool::write_png_chart(&path, &[1.0, 3.0, 2.0]).expect("chart should be written");
    let bytes = fs::read(&path).expect("chart should be readable");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let _ = fs::remove_file(path);
}

#[test]
fn persisted_task_messages_accept_pre_feature_records() {
    let message: TaskMessage = serde_json::from_value(json!({
        "id": "m1",
        "task_id": "t1",
        "role": "user",
        "content": "hello",
        "created_at": 0,
        "streaming": false
    }))
    .expect("old task message should deserialize");
    assert!(message.parts.is_empty());
    assert!(message.tool_calls.is_empty());
    assert!(message.tool_call_id.is_none());
}

#[test]
fn assistant_parts_keep_text_and_tool_order_with_unicode_offsets() {
    let mut message = TaskMessage {
        id: "assistant-1".to_string(),
        task_id: "task-1".to_string(),
        role: "assistant".to_string(),
        content: String::new(),
        reasoning: String::new(),
        reasoning_opaque: None,
        created_at: 1,
        streaming: true,
        parts: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
        base64_image: None,
        attachments: Vec::new(),
    };

    apply_stream_event(
        &mut message,
        &llm::StreamEvent::TextDelta("first".to_string()),
    );
    apply_stream_event(
        &mut message,
        &llm::StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call-1".to_string()),
            name: Some("rust_clock".to_string()),
            arguments: Some("{}".to_string()),
        },
    );
    apply_stream_event(
        &mut message,
        &llm::StreamEvent::TextDelta("then".to_string()),
    );

    assert_eq!(message.content, "firstthen");
    assert!(matches!(
        &message.parts[0],
        AssistantPart::Text {
            start: 0,
            end: 5,
            ..
        }
    ));
    assert!(matches!(
        &message.parts[1],
        AssistantPart::Tool { index: 0, call_id, name, .. }
            if call_id == "call-1" && name == "rust_clock"
    ));
    assert!(matches!(
        &message.parts[2],
        AssistantPart::Text {
            start: 5,
            end: 9,
            ..
        }
    ));
}

#[test]
fn assistant_parts_keep_reasoning_before_text() {
    let mut message = TaskMessage {
        id: "assistant-reasoning".to_string(),
        task_id: "task-1".to_string(),
        role: "assistant".to_string(),
        content: String::new(),
        reasoning: String::new(),
        reasoning_opaque: None,
        created_at: 1,
        streaming: true,
        parts: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
        base64_image: None,
        attachments: Vec::new(),
    };
    apply_stream_event(
        &mut message,
        &llm::StreamEvent::ReasoningDelta("思考".to_string()),
    );
    apply_stream_event(
        &mut message,
        &llm::StreamEvent::TextDelta("答案".to_string()),
    );
    assert_eq!(message.reasoning, "思考");
    assert_eq!(message.content, "答案");
    assert!(matches!(
        message.parts[0],
        AssistantPart::Reasoning {
            start: 0,
            end: 2,
            ..
        }
    ));
    assert!(matches!(
        message.parts[1],
        AssistantPart::Text {
            start: 0,
            end: 2,
            ..
        }
    ));
}

#[test]
fn reasoning_is_forwarded_in_assistant_context() {
    let entries = vec![AgentMemoryEntry {
        id: "assistant-reasoning".to_string(),
        role: "assistant".to_string(),
        content: "answer".to_string(),
        reasoning: "thinking".to_string(),
        reasoning_opaque: None,
        created_at: 1,
        tool_call_id: None,
        tool_names: Vec::new(),
        tool_calls: Vec::new(),
        name: None,
        base64_image: None,
        attachments: Vec::new(),
    }];
    let messages = memory_to_chat_messages(&entries);
    assert_eq!(messages[0].reasoning_content.as_deref(), Some("thinking"));
    let value = serde_json::to_value(&messages[0]).expect("chat message should serialize");
    assert_eq!(value["reasoning_content"], "thinking");
}

#[test]
fn reasoning_opaque_is_forwarded_without_becoming_visible_content() {
    let entries = vec![AgentMemoryEntry {
        id: "assistant-copilot".to_string(),
        role: "assistant".to_string(),
        content: String::new(),
        reasoning: "thinking".to_string(),
        reasoning_opaque: Some("signature".to_string()),
        created_at: 1,
        tool_call_id: None,
        tool_names: Vec::new(),
        tool_calls: Vec::new(),
        name: None,
        base64_image: None,
        attachments: Vec::new(),
    }];
    let messages = memory_to_chat_messages(&entries);
    assert_eq!(messages[0].reasoning_opaque.as_deref(), Some("signature"));
    assert!(messages[0].content.is_none());
    let value = serde_json::to_value(&messages[0]).expect("chat message should serialize");
    assert_eq!(value["reasoning_opaque"], "signature");
}

#[test]
fn legacy_assistant_messages_get_compact_ordered_parts() {
    let mut message: TaskMessage = serde_json::from_value(json!({
        "id": "assistant-legacy",
        "task_id": "task-1",
        "role": "assistant",
        "content": "先检查",
        "created_at": 1,
        "streaming": false,
        "tool_calls": [{
            "id": "call-1",
            "type": "function",
            "function": {"name": "rust_clock", "arguments": "{}"}
        }]
    }))
    .expect("legacy assistant message should deserialize");

    ensure_assistant_parts(&mut message);
    assert!(matches!(
        &message.parts[0],
        AssistantPart::Text {
            start: 0,
            end: 3,
            ..
        }
    ));
    assert!(matches!(
        &message.parts[1],
        AssistantPart::Tool { index: 0, call_id, .. } if call_id == "call-1"
    ));
}

#[test]
fn interrupted_stream_placeholder_is_repaired_without_entering_context() {
    let mut task: Task = serde_json::from_value(json!({
        "id": "task-1",
        "title": "Interrupted request",
        "prompt": "question",
        "status": "failed",
        "created_at": 1,
        "updated_at": 1,
        "demo_mode": false,
        "messages": [
            {
                "id": "user-1",
                "task_id": "task-1",
                "role": "user",
                "content": "question",
                "created_at": 1,
                "streaming": false
            },
            {
                "id": "assistant-placeholder",
                "task_id": "task-1",
                "role": "assistant",
                "content": "",
                "created_at": 2,
                "streaming": true
            }
        ],
        "memory": [],
        "plans": [],
        "active_plan_id": null,
        "steps": [],
        "tool_calls": [],
        "approval_requests": [],
        "llm_usage": {},
        "final_answer": null,
        "error": "interrupted"
    }))
    .expect("interrupted task record should deserialize");

    assert!(repair_task_record(&mut task));
    assert!(!task.messages[1].streaming);
    assert_eq!(task.memory.len(), 1);
    assert_eq!(task.memory[0].id, "user-1");
}

#[test]
fn context_repairs_legacy_ui_tool_ids_to_model_ids() {
    let call = agent::MessageToolCall {
        id: "call-model-1".to_string(),
        call_type: "function".to_string(),
        function: agent::FunctionCall {
            name: "rust_clock".to_string(),
            arguments: "{}".to_string(),
        },
    };
    let entries = vec![
        AgentMemoryEntry {
            id: "assistant-1".to_string(),
            role: "assistant".to_string(),
            content: String::new(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 1,
            tool_call_id: None,
            tool_names: vec!["rust_clock".to_string()],
            tool_calls: vec![call.clone()],
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
        AgentMemoryEntry {
            id: "tool-1".to_string(),
            role: "tool".to_string(),
            content: "12:00".to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 2,
            tool_call_id: Some("tool-ui-1".to_string()),
            tool_names: vec!["rust_clock".to_string()],
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
    ];

    let (normalized, changed) = normalize_memory_for_context(&entries);
    assert!(changed);
    assert_eq!(normalized[1].tool_call_id.as_deref(), Some("call-model-1"));
    validate_chat_message_context(&memory_to_chat_messages(&normalized))
        .expect("repaired history should be valid for the model");
    assert_eq!(normalized[1].content, "12:00");
    assert_eq!(normalized[0].tool_calls, vec![call]);
}

#[test]
fn context_inserts_a_truthful_result_for_an_interrupted_tool_call() {
    let entries = vec![
        AgentMemoryEntry {
            id: "assistant-1".to_string(),
            role: "assistant".to_string(),
            content: String::new(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 1,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: vec![agent::MessageToolCall {
                id: "call-missing".to_string(),
                call_type: "function".to_string(),
                function: agent::FunctionCall {
                    name: "rust_shell".to_string(),
                    arguments: "{\"command\":\"pwd\"}".to_string(),
                },
            }],
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
        AgentMemoryEntry {
            id: "user-2".to_string(),
            role: "user".to_string(),
            content: "缁х画".to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 2,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
    ];

    let (normalized, changed) = normalize_memory_for_context(&entries);
    assert!(changed);
    assert_eq!(normalized.len(), 3);
    assert_eq!(normalized[1].role, "tool");
    assert_eq!(normalized[1].tool_call_id.as_deref(), Some("call-missing"));
    assert!(normalized[1].content.contains("No result was recorded"));
    validate_chat_message_context(&memory_to_chat_messages(&normalized))
        .expect("interrupted history should be made replayable");
}

#[test]
fn context_budget_keeps_assistant_and_tool_messages_together() {
    let mut entries = vec![
        AgentMemoryEntry {
            id: "old-user".to_string(),
            role: "user".to_string(),
            content: "old".to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 1,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
        AgentMemoryEntry {
            id: "old-assistant".to_string(),
            role: "assistant".to_string(),
            content: "done".to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 2,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
        AgentMemoryEntry {
            id: "new-user".to_string(),
            role: "user".to_string(),
            content: "new".to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 3,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
        AgentMemoryEntry {
            id: "new-assistant".to_string(),
            role: "assistant".to_string(),
            content: String::new(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 4,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: vec![agent::MessageToolCall {
                id: "call-new".to_string(),
                call_type: "function".to_string(),
                function: agent::FunctionCall {
                    name: "rust_clock".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
        AgentMemoryEntry {
            id: "new-tool".to_string(),
            role: "tool".to_string(),
            content: "now".to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 5,
            tool_call_id: Some("call-new".to_string()),
            tool_names: vec!["rust_clock".to_string()],
            tool_calls: Vec::new(),
            name: Some("rust_clock".to_string()),
            base64_image: None,
            attachments: Vec::new(),
        },
    ];

    trim_memory_to_budget(&mut entries, 3);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.role.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "tool"]
    );
    validate_chat_message_context(&memory_to_chat_messages(&entries))
        .expect("bounded history should remain protocol-valid");
}

#[tokio::test]
async fn visualization_tool_writes_a_real_html_artifact() {
    let workspace = std::env::temp_dir().join(format!("rustpilot-data-{}", Uuid::new_v4()));
    fs::create_dir_all(&workspace).expect("temporary workspace should be created");
    let source = workspace.join("source.csv");
    fs::write(&source, "label,value\nA,2\nB,5\n").expect("source should be written");
    let output = data_tool::run_data_visualization_tool(
        &json!({
            "path": "source.csv",
            "output_type": "html",
            "title": "Test chart"
        }),
        &workspace,
    )
    .await
    .expect("visualization should succeed");
    let value: Value = serde_json::from_str(&output).expect("visualization output should be JSON");
    let chart_path = value["results"][0]["chart_path"]
        .as_str()
        .expect("chart path should be returned");
    assert!(Path::new(chart_path).exists());
    let html = fs::read_to_string(chart_path).expect("chart HTML should be readable");
    assert!(html.contains("<svg"));
    let _ = fs::remove_dir_all(workspace);
}
