use super::fixture::*;

#[tokio::test]
async fn reused_error_handler_keeps_all_disjoint_invocations() {
    let root = tempfile::tempdir().unwrap();
    let lua = vm(
        root.path(),
        runtime(
            root.path(),
            Recorder::new(vec![
                Step::Fail,
                Step::Reply("recovered"),
                Step::Fail,
                Step::Reply("recovered"),
            ]),
        ),
    );
    lua.load(format!(r#"{CREW}
        crew:add_task({{name='first', agent='alice', description='work', on_error='recover'}})
        crew:add_task({{name='second', agent='alice', description='work', depends_on={{'first'}}, on_error='recover'}})
        crew:add_task({{name='recover', agent='bob', description='recover'}})
        for _, result in ipairs(crew:run()) do
            assert(result.success)
            assert(result.usage.settled.requests == (result.task == 'recover' and '2' or '1'))
        end
        assert(crew:usage().settled.requests == '4')
    "#)).exec_async().await.unwrap();
}

#[tokio::test]
async fn task_retry_receipts_match_run_history_and_events() {
    let root = tempfile::tempdir().unwrap();
    let lua = vm(
        root.path(),
        runtime(root.path(), Recorder::new(vec![Step::Fail])),
    );
    let bus = ironcrew::engine::eventbus::EventBus::default();
    lua.set_app_data(bus.clone());
    let store: std::sync::Arc<dyn ironcrew::engine::store::StateStore> = std::sync::Arc::new(
        ironcrew::engine::run_history::JsonFileStore::new(root.path().join("history")).unwrap(),
    );
    lua.set_app_data(store.clone());
    lua.load(format!(
        r#"{CREW}
        crew:add_task({{name='retry', agent='alice', description='work',
                       max_retries=1, retry_backoff_secs=0.001}})
        local result = crew:run()[1]
        assert(result.success)
        assert(result.token_usage == nil)
        assert(result.usage.settled.requests == '2')
        assert(result.usage.settled.total_tokens.known == '26')
        assert(result.usage.settled.reasoning_tokens.known == '4')
        assert(json_stringify(result.usage) == json_stringify(crew:usage()))
    "#
    ))
    .exec_async()
    .await
    .unwrap();
    let run_id: String = lua.globals().get("__ironcrew_last_run_id").unwrap();
    let record = store.get_run(&run_id).await.unwrap();
    assert_eq!(record.usage.settled.requests(), 2);
    assert_eq!(record.usage, record.task_results[0].usage);
    let (events, _) = bus.subscribe_with_replay();
    let completed = events
        .iter()
        .find_map(|event| match event.as_ref() {
            ironcrew::engine::eventbus::CrewEvent::TaskCompleted { usage, .. } => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(completed, &record.usage);
}

#[tokio::test]
async fn failed_handlers_keep_their_own_receipts_without_double_counting() {
    for collaborative in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let lua = vm(
            root.path(),
            runtime(root.path(), Recorder::new(vec![Step::Fail, Step::Fail])),
        );
        let task = if collaborative {
            "crew:add_collaborative_task({name='main', description='work', agents={'alice','bob'}, max_turns=1, on_error='recover'})"
        } else {
            "crew:add_task({name='main', agent='alice', description='work', on_error='recover'})"
        };
        lua.load(format!(
            r#"{CREW}
            {task}
            crew:add_task({{name='recover', agent='alice', description='recover'}})
            local by_name = {{}}
            for _, result in ipairs(crew:run()) do by_name[result.task] = result end
            assert(not by_name.main.success)
            assert(not by_name.recover.success)
            assert(by_name.main.usage.settled.requests == '1')
            assert(by_name.recover.usage.settled.requests == '1')
            assert(by_name.recover.usage.settled.reasoning_tokens.known == '2')
            assert(crew:usage().settled.requests == '2')
        "#
        ))
        .exec_async()
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn parallel_and_nested_task_results_use_disjoint_scopes() {
    let root = tempfile::tempdir().unwrap();
    let lua = vm(root.path(), runtime(root.path(), Recorder::default()));
    lua.load(format!(r#"{CREW}
        crew:add_task({{name='one', agent='alice', description='work'}})
        crew:add_task({{name='two', agent='bob', description='work'}})
        crew:memory_set('items', json_stringify({{'a','b','c'}}))
        crew:add_foreach_task({{name='each', agent='alice', description='work ${{item}}', foreach='items', foreach_parallel=true}})
        crew:add_collaborative_task({{name='debate', description='discuss', agents={{'alice','bob'}}, max_turns=1}})
        local counts = {{one='1', two='1', each='3', debate='3'}}
        for _, result in ipairs(crew:run()) do
            assert(result.success)
            assert(result.usage.settled.requests == counts[result.task])
        end
        assert(crew:usage().settled.requests == '8')
    "#)).exec_async().await.unwrap();
}
