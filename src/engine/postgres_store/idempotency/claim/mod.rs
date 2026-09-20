mod conversation;
mod exclusive;
mod existing;
mod persist;

use crate::engine::idempotency::{
    IdempotencyClaim, IdempotencyClaimOutcome, IdempotencyLimits, RUN_OPERATION,
};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::parse_timestamp;

pub(super) struct ClaimDecision {
    pub(super) outcome: IdempotencyClaimOutcome,
    pub(super) commit_error: &'static str,
}

pub(super) enum ExclusiveScopeResolution {
    Continue(Option<String>),
    Return(Box<ClaimDecision>),
}

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn claim_idempotency_record(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        claim.validate()?;
        limits.validate()?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency claim transaction failed: {error}"
            ))
        })?;
        // Every capacity mutation follows one order: global quota, principal,
        // optional run fence, resource, exclusive scope, and finally key.
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &claim.principal_id)
            .await?;
        if claim.operation == RUN_OPERATION {
            self.lock_run_fence(&mut tx, true).await?;
        }
        self.lock_resource(
            &mut tx,
            &claim.operation,
            if claim.operation == RUN_OPERATION {
                ""
            } else {
                &claim.scope
            },
            &claim.resource_id,
        )
        .await?;
        if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
            self.lock_idempotency_scope(&mut tx, exclusive_scope)
                .await?;
        }
        self.lock_idempotency_key(&mut tx, &claim.key_hash).await?;
        let (database_now, lease_expires_at) = self
            .database_clock_with_deadline(&mut tx, self.lease.ttl().as_secs(), "idempotency claim")
            .await?;
        let database_timestamp = parse_timestamp("PostgreSQL idempotency clock", &database_now)?;

        self.prune_idempotency_for_claim(&mut tx, &database_now, limits.prune_batch)
            .await?;
        if let Some(decision) = self
            .resolve_existing_idempotency_claim(&mut tx, &claim, database_timestamp)
            .await?
        {
            return Self::commit_claim_decision(tx, decision).await;
        }
        if !self.conversation_claim_is_current(&mut tx, &claim).await? {
            return Self::commit_claim_decision(
                tx,
                ClaimDecision {
                    outcome: IdempotencyClaimOutcome::Conflict,
                    commit_error: "PostgreSQL conversation idempotency conflict commit failed",
                },
            )
            .await;
        }
        let recovery_hazard_key = match self
            .resolve_exclusive_scope_claim(&mut tx, &claim, database_timestamp)
            .await?
        {
            ExclusiveScopeResolution::Continue(key) => key,
            ExclusiveScopeResolution::Return(decision) => {
                return Self::commit_claim_decision(tx, *decision).await;
            }
        };
        let decision = self
            .persist_idempotency_claim(
                &mut tx,
                claim,
                limits,
                database_now,
                lease_expires_at,
                recovery_hazard_key.as_deref(),
            )
            .await?;
        Self::commit_claim_decision(tx, decision).await
    }

    async fn commit_claim_decision(
        tx: sqlx::Transaction<'_, sqlx::Postgres>,
        decision: ClaimDecision,
    ) -> Result<IdempotencyClaimOutcome> {
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("{}: {error}", decision.commit_error))
        })?;
        Ok(decision.outcome)
    }
}
