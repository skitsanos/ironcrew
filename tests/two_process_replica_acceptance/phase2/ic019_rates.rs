use futures::FutureExt;

use super::*;

const A_RUN_KEY: &str = "ic019-rate-run-a";
const A_REJECTED_RUN_KEY: &str = "ic019-rate-run-a-rejected";
const B_RUN_KEY: &str = "ic019-rate-run-b";
const B_REJECTED_RUN_KEY: &str = "ic019-rate-run-b-rejected";

const WORK_ERROR: &str = "work request rate limit exceeded; retry later";
const CONTROL_ERROR: &str = "control request rate limit exceeded; retry later";
const OBSERVATION_ERROR: &str = "observation request rate limit exceeded; retry later";

pub(super) async fn run() {
    with_configured_process_pair(
        "ic019-local-rates",
        true,
        ic019_support::FIXTURE,
        ic019_support::RATE_ENV,
        |pair| {
            async move {
                let base_a = pair.owner_a.base_url.clone();
                let base_b = pair.survivor_b.base_url.clone();

                let run_a = ic019_http::accepted_run(
                    ic019_http::post_run(pair, &base_a, A_RUN_KEY).await,
                )
                .await;
                ic019_support::wait_for_waiting(pair, &run_a).await;
                ic019_http::assert_limited(
                    ic019_http::post_run(pair, &base_a, A_REJECTED_RUN_KEY).await,
                    WORK_ERROR,
                )
                .await;

                let metrics_a = ic019_http::scrape(pair, &base_a).await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(
                    &metrics_a,
                    "ironcrew_admission_requests_total{class=\"work\",outcome=\"admitted\"}",
                    1,
                );
                ic019_http::assert_sample(
                    &metrics_a,
                    "ironcrew_admission_requests_total{class=\"work\",outcome=\"limited\"}",
                    1,
                );
                ic019_http::assert_sample(
                    &metrics_b,
                    "ironcrew_admission_requests_total{class=\"work\",outcome=\"admitted\"}",
                    0,
                );
                ic019_http::assert_sample(
                    &metrics_b,
                    "ironcrew_admission_requests_total{class=\"work\",outcome=\"limited\"}",
                    0,
                );

                let run_b = ic019_http::accepted_run(
                    ic019_http::post_run(pair, &base_b, B_RUN_KEY).await,
                )
                .await;
                ic019_support::wait_for_waiting(pair, &run_b).await;
                ic019_http::assert_limited(
                    ic019_http::post_run(pair, &base_b, B_REJECTED_RUN_KEY).await,
                    WORK_ERROR,
                )
                .await;

                ic019_support::assert_question(
                    ic019_http::questions(pair, &base_a, &run_a).await,
                )
                .await;
                ic019_http::assert_limited(
                    ic019_http::questions(pair, &base_a, &run_a).await,
                    OBSERVATION_ERROR,
                )
                .await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(
                    &metrics_b,
                    "ironcrew_admission_requests_total{class=\"observation\",outcome=\"admitted\"}",
                    0,
                );
                ic019_support::assert_question(
                    ic019_http::questions(pair, &base_b, &run_b).await,
                )
                .await;
                ic019_http::assert_limited(
                    ic019_http::questions(pair, &base_b, &run_b).await,
                    OBSERVATION_ERROR,
                )
                .await;

                ic019_http::assert_abort_accepted(ic019_http::abort(pair, &base_a, &run_a).await)
                    .await;
                ic019_http::assert_limited(
                    ic019_http::abort(pair, &base_a, &run_a).await,
                    CONTROL_ERROR,
                )
                .await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(
                    &metrics_b,
                    "ironcrew_admission_requests_total{class=\"control\",outcome=\"admitted\"}",
                    0,
                );
                ic019_http::assert_abort_accepted(ic019_http::abort(pair, &base_b, &run_b).await)
                    .await;
                ic019_http::assert_limited(
                    ic019_http::abort(pair, &base_b, &run_b).await,
                    CONTROL_ERROR,
                )
                .await;
                ic019_support::assert_aborted(pair, &run_a).await;
                ic019_support::assert_aborted(pair, &run_b).await;

                for base_url in [&base_a, &base_b] {
                    ic019_http::wait_for_sample(
                        pair,
                        base_url,
                        "ironcrew_idempotency_global_usage{resource=\"records\"}",
                        2,
                    )
                    .await;
                }

                for base_url in [&base_a, &base_b] {
                    let metrics = ic019_http::scrape(pair, base_url).await;
                    for class in ["work", "control", "observation"] {
                        ic019_http::assert_sample(
                            &metrics,
                            &format!(
                                "ironcrew_admission_requests_total{{class=\"{class}\",outcome=\"admitted\"}}"
                            ),
                            1,
                        );
                        ic019_http::assert_sample(
                            &metrics,
                            &format!(
                                "ironcrew_admission_requests_total{{class=\"{class}\",outcome=\"limited\"}}"
                            ),
                            1,
                        );
                    }
                    ic019_http::assert_sample(
                        &metrics,
                        "ironcrew_admission_tracked_buckets",
                        3,
                    );
                    ic019_http::assert_sample(
                        &metrics,
                        "ironcrew_idempotency_global_usage{resource=\"records\"}",
                        2,
                    );
                    ic019_http::assert_sample(
                        &metrics,
                        "ironcrew_idempotency_quota_rejections_total{resource=\"global_records\"}",
                        0,
                    );
                    ic019_http::assert_metrics_hide_identities(
                        &metrics,
                        pair,
                        &[
                            A_RUN_KEY,
                            A_REJECTED_RUN_KEY,
                            B_RUN_KEY,
                            B_REJECTED_RUN_KEY,
                            run_a.as_str(),
                            run_b.as_str(),
                        ],
                    );
                }
            }
            .boxed_local()
        },
    )
    .await;
}
