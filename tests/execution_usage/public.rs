use super::fixture::*;
use ironcrew::usage::{UsageCoverage, UsageSnapshot};

#[tokio::test]
async fn lua_run_snapshots_are_isolated_from_each_other_and_from_sessions() {
    let root = tempfile::tempdir().unwrap();
    let lua = vm(
        root.path(),
        runtime(root.path(), Recorder::new(vec![Step::Fail])),
    );
    let encoded: String = lua
        .load(format!(
            r#"{CREW}
        assert(crew:usage() == nil)
        assert(crew:flow_usage().settled.requests == "0")
        crew:add_task({{ name = "retry", agent = "alice", description = "work",
                         max_retries = 1, retry_backoff_secs = 0.001 }})
        assert(crew:run()[1].success)
        local first = crew:usage()
        assert(first.settled.requests == "2")
        assert(first.settled.total_tokens.known == "26")
        assert(first.settled.reasoning_tokens.known == "4")
        assert(first.coverage == "complete")
        assert(crew:run()[1].success)
        assert(crew:usage().settled.requests == "1")
        assert(first.settled.requests == "2") -- detached snapshot
        local chat = crew:conversation({{ agent = "alice", stream = false }})
        assert(chat:usage().settled.requests == "0")
        chat:send("one")
        chat:send("two")
        assert(chat:usage().settled.requests == "2")
        local dialog = crew:dialog({{ agents = {{"alice", "bob"}}, starter = "talk",
                                     max_turns = 2, stream = false }})
        assert(dialog:usage().settled.requests == "0")
        dialog:run()
        assert(dialog:usage().settled.requests == "2")
        assert(chat:usage().settled.requests == "2")
        assert(crew:usage().settled.requests == "1")
        return json_stringify(crew:flow_usage())
    "#
        ))
        .eval_async()
        .await
        .unwrap();
    let snapshot: UsageSnapshot = serde_json::from_str(&encoded).unwrap();
    assert_eq!(snapshot.settled.requests(), 7);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(91));
    assert_eq!(snapshot.coverage, UsageCoverage::Complete);
}

#[tokio::test]
async fn failed_output_still_exposes_its_provider_receipt() {
    let root = tempfile::tempdir().unwrap();
    let lua = vm(
        root.path(),
        runtime(root.path(), Recorder::new(vec![Step::Reply("")])),
    );
    lua.load(format!(
        r#"{CREW}
        local chat = crew:conversation({{ agent = "alice", stream = false }})
        assert(not pcall(function() chat:send("blank") end))
        assert(chat:usage().settled.requests == "1")
        assert(chat:usage().settled.total_tokens.known == "13")
        assert(chat:usage().coverage == "complete")
        assert(crew:flow_usage().settled.requests == "1")
    "#
    ))
    .exec_async()
    .await
    .unwrap();
}

#[tokio::test]
async fn json_bridge_preserves_unknowns_and_maximum_unsigned_counts() {
    use ironcrew::usage::{UsageCounts, UsageReceipt, UsageTracker};
    let root = tempfile::tempdir().unwrap();
    let lua = vm(root.path(), runtime(root.path(), Recorder::default()));
    let scope = UsageTracker::default();
    scope
        .start()
        .unwrap()
        .finish(UsageReceipt::from_counts(
            UsageCounts {
                prompt_tokens: Some(u64::MAX),
                completion_tokens: Some(0),
                total_tokens: Some(u64::MAX),
                ..Default::default()
            },
            true,
        ))
        .unwrap();
    scope
        .start()
        .unwrap()
        .finish(UsageReceipt::default())
        .unwrap();
    lua.set_app_data(scope.clone());
    let encoded: String = lua
        .load(format!(
            r#"{CREW}
        local snapshot = crew:flow_usage()
        assert(snapshot.settled.total_tokens.known == "18446744073709551615")
        assert(not snapshot.settled.total_tokens.complete)
        assert(snapshot.settled.cached_tokens.known ~= nil)
        assert(type(snapshot.settled.cached_tokens.known) ~= "string")
        return json_stringify(snapshot)
    "#
        ))
        .eval_async()
        .await
        .unwrap();
    let wire: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert!(
        wire["settled"]["cached_tokens"]
            .as_object()
            .unwrap()
            .contains_key("known")
    );
    assert_eq!(
        wire["settled"]["cached_tokens"]["known"],
        serde_json::Value::Null
    );
    assert_eq!(
        serde_json::from_value::<UsageSnapshot>(wire).unwrap(),
        scope.snapshot().unwrap()
    );
}

#[tokio::test]
async fn cancelled_run_remains_inspectable_from_its_lua_handle() {
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let lua = vm(
        root.path(),
        runtime(
            root.path(),
            Recorder::new(vec![Step::Hang(entered.clone())]),
        ),
    );
    lua.load(format!("{CREW} saved = crew; crew:add_task({{ name = 'one', agent = 'alice', description = 'work' }})"))
        .exec_async().await.unwrap();
    {
        let execution = lua.load("saved:run()").exec_async();
        tokio::pin!(execution);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::select! {
                result = &mut execution => panic!("expected active request: {result:?}"),
                () = entered.notified() => {}
            }
        })
        .await
        .unwrap();
        lua.load("assert(saved:usage().in_flight == '1'); assert(saved:usage().coverage == 'unavailable')")
            .exec().unwrap();
    }
    let encoded: String = lua
        .load("return json_stringify(saved:usage())")
        .eval()
        .unwrap();
    let snapshot: UsageSnapshot = serde_json::from_str(&encoded).unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.settled.requests(), 1);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(13));
    assert_eq!(snapshot.coverage, UsageCoverage::Partial);
}
