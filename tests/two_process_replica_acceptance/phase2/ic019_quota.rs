use futures::FutureExt;
use sqlx::Row;

use super::*;

const ACCEPTED_KEY: &str = "ic019-shared-quota-accepted";
const REJECTED_A_KEY: &str = "ic019-shared-quota-rejected-a";
const REJECTED_B_KEY: &str = "ic019-shared-quota-rejected-b";

pub(super) async fn run() {
    with_configured_process_pair(
        "ic019-shared-quota",
        true,
        ic019_support::FIXTURE,
        ic019_support::QUOTA_ENV,
        |pair| {
            async move {
                let base_a = pair.owner_a.base_url.clone();
                let base_b = pair.survivor_b.base_url.clone();
                let run_id = ic019_http::accepted_run(
                    ic019_http::post_run(pair, &base_a, ACCEPTED_KEY).await,
                )
                .await;
                ic019_support::wait_for_waiting(pair, &run_id).await;

                ic019_http::assert_limited_contains(
                    ic019_http::post_run(pair, &base_a, REJECTED_A_KEY).await,
                    "Idempotency capacity is exhausted; retry after at least",
                )
                .await;
                ic019_http::assert_limited_contains(
                    ic019_http::post_run(pair, &base_b, REJECTED_B_KEY).await,
                    "Idempotency capacity is exhausted; retry after at least",
                )
                .await;

                let metrics_a = ic019_http::scrape(pair, &base_a).await;
                let metrics_b = ic019_http::scrape(pair, &base_b).await;
                for metrics in [&metrics_a, &metrics_b] {
                    for (series, expected) in [
                        (
                            "ironcrew_idempotency_global_usage{resource=\"records\"}",
                            1,
                        ),
                        (
                            "ironcrew_idempotency_global_limit{resource=\"records\"}",
                            1,
                        ),
                        ("ironcrew_idempotency_global_in_flight", 1),
                        (
                            "ironcrew_idempotency_max_principal_usage{resource=\"records\"}",
                            1,
                        ),
                        (
                            "ironcrew_idempotency_principal_limit{resource=\"records\"}",
                            1,
                        ),
                        (
                            "ironcrew_idempotency_quota_rejections_total{resource=\"global_records\"}",
                            1,
                        ),
                    ] {
                        ic019_http::assert_sample(metrics, series, expected);
                    }
                    ic019_http::assert_sample(
                        metrics,
                        "ironcrew_admission_requests_total{class=\"work\",outcome=\"limited\"}",
                        0,
                    );
                    ic019_http::assert_metrics_hide_identities(
                        metrics,
                        pair,
                        &[
                            ACCEPTED_KEY,
                            REJECTED_A_KEY,
                            REJECTED_B_KEY,
                            run_id.as_str(),
                        ],
                    );
                }
                ic019_http::assert_sample(
                    &metrics_a,
                    "ironcrew_admission_requests_total{class=\"work\",outcome=\"admitted\"}",
                    2,
                );
                ic019_http::assert_sample(
                    &metrics_b,
                    "ironcrew_admission_requests_total{class=\"work\",outcome=\"admitted\"}",
                    1,
                );

                let pool = sqlx::PgPool::connect(&pair.database_url)
                    .await
                    .expect("connect for IC-019 quota snapshot");
                let sql = format!(
                    "SELECT COUNT(*) AS records, \
                     COUNT(*) FILTER (WHERE key_hash = $1 OR key_hash = $2 OR key_hash = $3) AS raw_keys \
                     FROM {}idempotency",
                    pair.prefix
                );
                let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(ACCEPTED_KEY)
                    .bind(REJECTED_A_KEY)
                    .bind(REJECTED_B_KEY)
                    .fetch_one(&pool)
                    .await
                    .expect("read IC-019 durable quota rows");
                assert_eq!(row.get::<i64, _>("records"), 1);
                assert_eq!(row.get::<i64, _>("raw_keys"), 0);
                pool.close().await;

                ic019_support::assert_question(
                    ic019_http::questions(pair, &base_a, &run_id).await,
                )
                .await;
                ic019_support::assert_question(
                    ic019_http::questions(pair, &base_b, &run_id).await,
                )
                .await;
                ic019_http::assert_abort_accepted(
                    ic019_http::abort(pair, &base_b, &run_id).await,
                )
                .await;
                ic019_support::assert_aborted(pair, &run_id).await;

                let final_a = ic019_http::scrape(pair, &base_a).await;
                let final_b = ic019_http::scrape(pair, &base_b).await;
                ic019_http::assert_sample(
                    &final_a,
                    "ironcrew_admission_requests_total{class=\"observation\",outcome=\"admitted\"}",
                    1,
                );
                ic019_http::assert_sample(
                    &final_b,
                    "ironcrew_admission_requests_total{class=\"observation\",outcome=\"admitted\"}",
                    1,
                );
                ic019_http::assert_sample(
                    &final_b,
                    "ironcrew_admission_requests_total{class=\"control\",outcome=\"admitted\"}",
                    1,
                );
            }
            .boxed_local()
        },
    )
    .await;
}
