use futures::FutureExt;

use super::*;

const A_RUN_KEY: &str = "ic019-local-run-a";
const A_REJECTED_RUN_KEY: &str = "ic019-local-run-a-rejected";
const B_RUN_KEY: &str = "ic019-local-run-b";
const B_REJECTED_RUN_KEY: &str = "ic019-local-run-b-rejected";
const A_CONVERSATION: &str = "ic019-local-conversation-a";
const A_REJECTED_CONVERSATION: &str = "ic019-local-conversation-a-rejected";
const B_CONVERSATION: &str = "ic019-local-conversation-b";
const B_REJECTED_CONVERSATION: &str = "ic019-local-conversation-b-rejected";

pub(super) async fn run() {
    with_configured_process_pair(
        "ic019-local-limits",
        true,
        ic019_support::FIXTURE,
        ic019_support::ROOMY_ENV,
        |pair| {
            async move {
                ic019_support::assert_capacity_envelope();
                let base_a = pair.owner_a.base_url.clone();
                let base_b = pair.survivor_b.base_url.clone();

                let run_a =
                    ic019_http::accepted_run(ic019_http::post_run(pair, &base_a, A_RUN_KEY).await)
                        .await;
                ic019_support::wait_for_waiting(pair, &run_a).await;

                let metrics_a = ic019_http::scrape(pair, &base_a).await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(&metrics_a, "ironcrew_process_active_runs", 1);
                ic019_http::assert_sample(&metrics_b, "ironcrew_process_active_runs", 0);
                ic019_http::assert_unavailable(
                    ic019_http::post_run(pair, &base_a, A_REJECTED_RUN_KEY).await,
                    "Active run limit reached (1 runs)",
                )
                .await;

                let run_b =
                    ic019_http::accepted_run(ic019_http::post_run(pair, &base_b, B_RUN_KEY).await)
                        .await;
                ic019_support::wait_for_waiting(pair, &run_b).await;
                ic019_http::assert_unavailable(
                    ic019_http::post_run(pair, &base_b, B_REJECTED_RUN_KEY).await,
                    "Active run limit reached (1 runs)",
                )
                .await;

                ic019_http::assert_conversation_started(
                    ic019_http::start_conversation(pair, &base_a, A_CONVERSATION).await,
                    A_CONVERSATION,
                )
                .await;
                let metrics_a = ic019_http::scrape(pair, &base_a).await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(&metrics_a, "ironcrew_process_active_conversations", 1);
                ic019_http::assert_sample(&metrics_b, "ironcrew_process_active_conversations", 0);
                ic019_http::assert_unavailable(
                    ic019_http::start_conversation(pair, &base_a, A_REJECTED_CONVERSATION).await,
                    "Active conversation limit reached (1 sessions)",
                )
                .await;

                ic019_http::assert_conversation_started(
                    ic019_http::start_conversation(pair, &base_b, B_CONVERSATION).await,
                    B_CONVERSATION,
                )
                .await;
                ic019_http::assert_unavailable(
                    ic019_http::start_conversation(pair, &base_b, B_REJECTED_CONVERSATION).await,
                    "Active conversation limit reached (1 sessions)",
                )
                .await;

                let sse_a = ic019_http::open_sse(pair, &base_a, &run_a).await;
                ic019_http::wait_for_sample(
                    pair,
                    &base_a,
                    "ironcrew_process_active_sse_connections",
                    1,
                )
                .await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(&metrics_b, "ironcrew_process_active_sse_connections", 0);
                ic019_http::assert_limited(
                    authenticated(
                        pair.client
                            .get(format!("{base_a}/flows/{FLOW}/events/{run_a}")),
                    )
                    .send()
                    .await
                    .expect("saturate replica A SSE"),
                    "SSE connection limit reached (1)",
                )
                .await;

                let sse_b = ic019_http::open_sse(pair, &base_b, &run_a).await;
                ic019_http::assert_limited(
                    authenticated(
                        pair.client
                            .get(format!("{base_b}/flows/{FLOW}/events/{run_a}")),
                    )
                    .send()
                    .await
                    .expect("saturate replica B SSE"),
                    "SSE connection limit reached (1)",
                )
                .await;

                for (base_url, keys) in [
                    (
                        base_a.as_str(),
                        [A_RUN_KEY, A_REJECTED_RUN_KEY, B_RUN_KEY, B_REJECTED_RUN_KEY],
                    ),
                    (
                        base_b.as_str(),
                        [A_RUN_KEY, A_REJECTED_RUN_KEY, B_RUN_KEY, B_REJECTED_RUN_KEY],
                    ),
                ] {
                    let metrics = ic019_http::scrape(pair, base_url).await;
                    for (series, value) in [
                        ("ironcrew_process_active_runs", 1),
                        ("ironcrew_process_active_runs_limit", 1),
                        ("ironcrew_process_active_conversations", 1),
                        ("ironcrew_process_active_conversations_limit", 1),
                        ("ironcrew_process_active_sse_connections", 1),
                        ("ironcrew_process_active_sse_connections_limit", 1),
                    ] {
                        ic019_http::assert_sample(&metrics, series, value);
                    }
                    ic019_http::assert_metrics_hide_identities(
                        &metrics,
                        pair,
                        &[
                            keys[0],
                            keys[1],
                            keys[2],
                            keys[3],
                            run_a.as_str(),
                            run_b.as_str(),
                        ],
                    );
                }

                drop(sse_a);
                drop(sse_b);
                ic019_http::wait_for_sample(
                    pair,
                    &base_a,
                    "ironcrew_process_active_sse_connections",
                    0,
                )
                .await;
                ic019_http::wait_for_sample(
                    pair,
                    &base_b,
                    "ironcrew_process_active_sse_connections",
                    0,
                )
                .await;

                ic019_http::delete_conversation(pair, &base_a, A_CONVERSATION).await;
                ic019_http::delete_conversation(pair, &base_b, B_CONVERSATION).await;
                ic019_http::assert_abort_accepted(ic019_http::abort(pair, &base_a, &run_a).await)
                    .await;
                ic019_http::assert_abort_accepted(ic019_http::abort(pair, &base_b, &run_b).await)
                    .await;
                ic019_support::assert_aborted(pair, &run_a).await;
                ic019_support::assert_aborted(pair, &run_b).await;

                for base_url in [&base_a, &base_b] {
                    let metrics = ic019_http::scrape(pair, base_url).await;
                    ic019_http::assert_sample(&metrics, "ironcrew_process_active_runs", 0);
                    ic019_http::assert_sample(&metrics, "ironcrew_process_active_conversations", 0);
                }
            }
            .boxed_local()
        },
    )
    .await;
}
