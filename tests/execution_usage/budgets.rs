use super::fixture::*;
use ironcrew::usage::{
    UsageTracker,
    budget::{BudgetState, TokenBudget},
};
use std::sync::atomic::Ordering;

fn scope(limit: u64) -> UsageTracker {
    UsageTracker::with_budget(TokenBudget::new(limit).unwrap())
}

#[tokio::test]
async fn standalone_conversation_requests_use_fresh_caller_budgets_not_session_history() {
    let root = tempfile::tempdir().unwrap();
    let lua = vm(root.path(), runtime(root.path(), Recorder::default()));
    let original = scope(1);
    lua.set_app_data(original.clone());
    let handle: mlua::AnyUserData = lua
        .load(format!(
            "{CREW} return crew:conversation({{agent='alice',stream=false}})"
        ))
        .eval_async()
        .await
        .unwrap();
    let conversation = handle
        .borrow::<ironcrew::lua::conversation::LuaConversation>()
        .unwrap()
        .0
        .clone();
    for _ in 0..2 {
        let caller = scope(13);
        let context = ironcrew::tools::ToolCallContext {
            usage_tracker: Some(caller.clone()),
            ..Default::default()
        };
        conversation
            .run_turn_with_ctx("work", None, &context)
            .await
            .unwrap();
        assert!(
            conversation
                .run_turn_with_ctx("over budget", None, &context)
                .await
                .is_err()
        );
        assert_eq!(caller.snapshot().unwrap().budget.charged, 13);
    }
    assert_eq!(original.snapshot().unwrap().budget.charged, 0);
    assert_eq!(conversation.usage_snapshot().unwrap().settled.requests(), 2);
    assert_eq!(
        conversation.usage_snapshot().unwrap().budget.state,
        BudgetState::Disabled
    );
}

#[tokio::test]
async fn repeated_runs_and_retries_cannot_reset_budget_or_report_success() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::new(vec![Step::Fail]);
    let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
    let tracker = scope(26);
    lua.set_app_data(tracker.clone());
    lua.load(format!(r#"{CREW}
        crew:add_task({{name="retry",agent="alice",description="work",max_retries=2,retry_backoff_secs=0.001}})
        assert(crew:run()[1].success)
        assert(not pcall(function() crew:run() end))
        assert(not pcall(function() crew:run() end))
        assert(crew:flow_usage().budget.state == "exhausted")
    "#)).exec_async().await.unwrap();
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tracker.snapshot().unwrap().budget.charged, 26);
    let store = ironcrew::engine::store::create_store(root.path().join(".ironcrew"))
        .await
        .unwrap();
    let id: String = lua.globals().get("__ironcrew_last_run_id").unwrap();
    let record = store.get_run(&id).await.unwrap();
    assert_eq!(
        record.status,
        ironcrew::engine::run_history::RunStatus::Failed
    );
    assert_eq!(record.usage.budget.state, BudgetState::Exhausted);
}

#[tokio::test]
async fn parallel_foreach_and_dialog_calls_share_one_ceiling() {
    for work in [
        r#"for i=1,8 do crew:add_task({name="t"..i,agent="alice",description="work"}) end crew:run()"#,
        r#"crew:memory_set("items", json_stringify({"a","b","c","d"}))
            crew:add_foreach_task({name="each",agent="alice",description="work ${item}",foreach="items",foreach_parallel=true}) crew:run()"#,
        r#"crew:dialog({agents={"alice","bob"},starter="talk",max_turns=4}):run()"#,
        r#"local c=crew:conversation({agent="alice",stream=false}) c:send("one") c:send("two") c:send("three")"#,
    ] {
        let root = tempfile::tempdir().unwrap();
        let recorder = Recorder::default();
        let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
        let tracker = scope(26);
        lua.set_app_data(tracker.clone());
        assert!(
            lua.load(format!("{CREW}{work}"))
                .exec_async()
                .await
                .is_err()
        );
        assert_eq!(recorder.calls.load(Ordering::SeqCst), 2);
        let snapshot = tracker.snapshot().unwrap();
        assert_eq!(snapshot.budget.charged, 26);
        assert_eq!(snapshot.budget.state, BudgetState::Exhausted);
    }
}

#[tokio::test]
async fn delegation_and_nested_lua_flows_cannot_allocate_fresh_budget() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("child.lua"),
        format!("{CREW}{ONE_TASK} return 'child'"),
    )
    .unwrap();
    let recorder = Recorder::new(vec![Step::Tool("agent__bob")]);
    let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
    let tracker = scope(39);
    lua.set_app_data(tracker.clone());
    lua.load(
        r#"
        local crew=Crew.new({goal="delegate",provider="openai",model="fixture"})
        crew:add_agent(Agent.new({name="alice",goal="work",tools={"agent__bob"}}))
        crew:add_agent(Agent.new({name="bob",goal="help"}))
        crew:add_task({name="parent",agent="alice",description="delegate"})
        assert(crew:run()[1].success)
        assert(not pcall(function() run_flow("child.lua") end))
        assert(not pcall(function() crew:subworkflow("child.lua") end))
    "#,
    )
    .exec_async()
    .await
    .unwrap();
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 3);
    assert_eq!(tracker.snapshot().unwrap().budget.charged, 39);
}

#[tokio::test]
async fn independent_flows_keep_independent_budget_identity() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::default();
    let runtime = runtime(root.path(), recorder.clone());
    for _ in 0..2 {
        let lua = vm(root.path(), runtime.clone());
        lua.set_app_data(scope(13));
        lua.load(format!("{CREW}{ONE_TASK}"))
            .exec_async()
            .await
            .unwrap();
        assert_eq!(tracker(&lua).snapshot().unwrap().budget.charged, 13);
    }
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn timeout_retry_keeps_potentially_billed_reservation() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::new(vec![Step::Hang(Default::default())]);
    let lua = vm(root.path(), runtime(root.path(), recorder.clone()));
    let tracker = scope(13);
    lua.set_app_data(tracker.clone());
    assert!(lua.load(format!(r#"{CREW}
        crew:add_task({{name="timeout",agent="alice",description="work",timeout_secs=1,max_retries=2,retry_backoff_secs=0.001}})
        crew:run()
    "#)).exec_async().await.is_err());
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
    let snapshot = tracker.snapshot().unwrap().budget;
    assert_eq!(
        (snapshot.charged, snapshot.retained, snapshot.reserved),
        (13, 13, 0)
    );
    assert_eq!(snapshot.state, BudgetState::Exhausted);
}
