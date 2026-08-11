use super::*;

/// Keep the three process pairs sequential. Besides making each admission
/// scope independent, this avoids six extra child processes competing for
/// memory and PostgreSQL connections in the CI pod.
#[tokio::test]
async fn ic019_process_local_admission_and_shared_quota_are_truthful() {
    ic019_limits::run().await;
    ic019_rates::run().await;
    ic019_quota::run().await;
}
