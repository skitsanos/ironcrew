use super::fixture::*;
use ironcrew::usage::UsageCoverage;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::test]
async fn retries_and_separate_runs_in_one_flow_keep_all_attempts() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::new(vec![Step::Fail]);
    let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
    lua.load(format!(
        r#"{CREW}
        crew:add_task({{ name = "retry", agent = "alice", description = "work",
                         max_retries = 1, retry_backoff_secs = 0.001 }})
        assert(crew:run()[1].success)
        assert(crew:run()[1].success)
    "#
    ))
    .exec_async()
    .await
    .unwrap();
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 3);
    assert_usage(&tracker(&lua), 3, UsageCoverage::Complete);
}

#[tokio::test]
async fn parallel_tasks_foreach_and_collaboration_count_dispatches_once() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::default();
    let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
    lua.load(format!(
        r#"{CREW}
        for i = 1, 4 do
            crew:add_task({{ name = "parallel" .. i, agent = "alice", description = "work" }})
        end
        crew:memory_set("items", json_stringify({{"a", "b", "c"}}))
        crew:add_foreach_task({{ name = "each", agent = "alice", description = "work ${{item}}",
                                foreach = "items", foreach_parallel = true }})
        crew:add_collaborative_task({{ name = "debate", description = "discuss",
                                      agents = {{"alice", "bob"}}, max_turns = 2 }})
        for _, result in ipairs(crew:run()) do assert(result.success, result.output) end
    "#
    ))
    .exec_async()
    .await
    .unwrap();
    let calls = recorder.calls.load(Ordering::SeqCst);
    assert_eq!(calls, 12); // 4 ordinary + 3 foreach + 4 discussion + 1 synthesis
    assert_usage(&tracker(&lua), calls, UsageCoverage::Complete);
}

#[tokio::test]
async fn shared_runtime_keeps_concurrent_lua_flows_isolated() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path(), Recorder::default());
    let first = vm(root.path(), runtime.clone());
    let second = vm(root.path(), runtime.clone());
    let script = format!("{CREW}{ONE_TASK}");
    let (a, b) = tokio::join!(
        first.load(&script).exec_async(),
        second.load(&script).exec_async()
    );
    a.unwrap();
    b.unwrap();
    assert_usage(&tracker(&first), 1, UsageCoverage::Complete);
    assert_usage(&tracker(&second), 1, UsageCoverage::Complete);
    assert!(runtime.provider.usage_tracker().is_none());
}

#[tokio::test]
async fn delegated_agent_and_nested_flows_inherit_parent_scope() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("child.lua"),
        format!("{CREW}{ONE_TASK} return 'child'"),
    )
    .unwrap();
    let recorder = Recorder::new(vec![Step::Tool("agent__bob")]);
    let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
    lua.load(
        r#"
        local crew = Crew.new({ goal = "delegate", provider = "openai", model = "fixture" })
        crew:add_agent(Agent.new({ name = "alice", goal = "work", tools = {"agent__bob"} }))
        crew:add_agent(Agent.new({ name = "bob", goal = "help" }))
        crew:add_task({ name = "parent", agent = "alice", description = "delegate" })
        assert(crew:run()[1].success)
        assert(run_flow("child.lua") == "child")
        assert(crew:subworkflow("child.lua") == "child")
    "#,
    )
    .exec_async()
    .await
    .unwrap();
    assert_usage(&tracker(&lua), 5, UsageCoverage::Complete);
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn conversations_dialogs_and_failed_final_content_share_flow_scope() {
    for stream in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(vec![Step::Reply(""), Step::Reply("recovered")]);
        let lua = vm(root.path(), runtime(root.path(), recorder));
        lua.load(format!(
            r#"{CREW}
            local chat = crew:conversation({{ agent = "alice", stream = {stream} }})
            assert(not pcall(function() chat:send("blank") end))
            assert(chat:send("recover") == "recovered")
            local dialog = crew:dialog({{ agents = {{"alice", "bob"}}, starter = "talk",
                                          max_turns = 2, stream = {stream} }})
            assert(#dialog:run() == 2)
        "#
        ))
        .exec_async()
        .await
        .unwrap();
        assert_usage(&tracker(&lua), 4, UsageCoverage::Complete);
    }
}

#[tokio::test]
async fn cancelling_lua_execution_retains_partial_provider_receipt() {
    let root = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let lua = vm(
        root.path(),
        runtime(
            root.path(),
            Recorder::new(vec![Step::Hang(entered.clone())]),
        ),
    );
    let script = format!("{CREW}{ONE_TASK}");
    {
        let execution = lua.load(&script).exec_async();
        tokio::pin!(execution);
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                result = &mut execution => panic!("expected active provider: {result:?}"),
                () = entered.notified() => {}
            }
        })
        .await
        .unwrap();
        assert_eq!(tracker(&lua).snapshot().unwrap().in_flight, 1);
    }
    assert_usage(&tracker(&lua), 1, UsageCoverage::Partial);
}

#[tokio::test]
async fn task_timeout_and_retry_preserve_cancelled_attempt_usage() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::new(vec![Step::Hang(Arc::new(tokio::sync::Notify::new()))]);
    let lua = vm(root.path(), runtime(root.path(), recorder));
    lua.load(format!(
        r#"{CREW}
        crew:add_task({{ name = "timeout", agent = "alice", description = "work",
                         timeout_secs = 1, max_retries = 1, retry_backoff_secs = 0.001 }})
        assert(crew:run()[1].success)
    "#
    ))
    .exec_async()
    .await
    .unwrap();
    assert_usage(&tracker(&lua), 2, UsageCoverage::Partial);
}

#[tokio::test]
async fn explicit_conversation_caller_scope_overrides_session_for_all_paths() {
    use ironcrew::lua::conversation::LuaConversation;
    use ironcrew::tools::ToolCallContext;
    use ironcrew::usage::UsageTracker;
    for stream in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let lua = vm(root.path(), runtime(root.path(), Recorder::default()));
        let handle: mlua::AnyUserData = lua
            .load(format!(
                r#"{CREW}
            return crew:conversation({{ agent = "alice", stream = {stream} }})
        "#
            ))
            .eval_async()
            .await
            .unwrap();
        let conversation = handle.borrow::<LuaConversation>().unwrap().0.clone();
        let scope = UsageTracker::default();
        conversation
            .run_turn_with_ctx(
                "caller",
                None,
                &ToolCallContext {
                    usage_tracker: Some(scope.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_usage(&scope, 1, UsageCoverage::Complete);
        assert_eq!(tracker(&lua).snapshot().unwrap().settled.requests(), 0);
    }
}

#[tokio::test]
async fn lua_tool_subflow_inherits_scope_across_fresh_tool_vm() {
    use ironcrew::engine::runtime::Runtime;
    use ironcrew::lua::api::load_tool_defs_from_files;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("child.lua"),
        format!("{CREW}{ONE_TASK} return 'child'"),
    )
    .unwrap();
    let tool_path = root.path().join("nested.lua");
    std::fs::write(
        &tool_path,
        r#"
        return { name = "nested", description = "run a child crew",
                 parameters = { prompt = { type = "string", description = "input" } },
                 execute = function(_) return run_flow("child.lua") end }
    "#,
    )
    .unwrap();
    let recorder = Recorder::new(vec![Step::Tool("nested")]);
    let mut runtime = Runtime::new(Box::new(recorder.clone()), Some(root.path()));
    runtime
        .register_lua_tools(load_tool_defs_from_files(&[tool_path]).unwrap())
        .unwrap();
    let runtime = Arc::new(runtime);
    runtime.set_self_ref(Arc::downgrade(&runtime));
    let lua = vm(root.path(), runtime);
    lua.load(
        r#"
        local crew = Crew.new({ goal = "nested", provider = "openai", model = "fixture" })
        crew:add_agent(Agent.new({ name = "alice", goal = "work", tools = {"nested"} }))
        crew:add_task({ name = "parent", agent = "alice", description = "delegate" })
        assert(crew:run()[1].success)
    "#,
    )
    .exec_async()
    .await
    .unwrap();
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 3);
    assert_usage(&tracker(&lua), 3, UsageCoverage::Complete);
}
