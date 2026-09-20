use crate::engine::idempotency::PrincipalId;
use crate::utils::error::Result;

use super::super::PostgresStore;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn lock_idempotency_quota(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-quota", "global", false)
            .await
    }

    pub(in crate::engine::postgres_store) async fn lock_idempotency_key(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_hash: &str,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-key", key_hash, false)
            .await
    }

    pub(in crate::engine::postgres_store) async fn lock_idempotency_principal(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: &PrincipalId,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-principal", principal_id.as_str(), false)
            .await
    }

    pub(in crate::engine::postgres_store) async fn lock_idempotency_scope(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        exclusive_scope: &str,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-scope", exclusive_scope, false)
            .await
    }
}
