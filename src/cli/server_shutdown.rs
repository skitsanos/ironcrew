use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;

use crate::api::{self, AppState};
use crate::utils::error::{IronCrewError, Result};

const OWNER_FENCE_FALLBACK_TIMEOUT: Duration = Duration::from_secs(5);
const OWNER_FENCE_INITIAL_RETRY: Duration = Duration::from_millis(100);
const OWNER_FENCE_MAX_RETRY: Duration = Duration::from_secs(5);

pub struct ShutdownConfig {
    pub routing_grace: Duration,
    pub teardown_timeout: Duration,
    pub background_drain: Duration,
}

/// Run the HTTP listener and the process lifecycle coordinator together.
/// SIGUSR1 fences the owner without exiting; SIGTERM/Ctrl-C additionally
/// advance to stopping after the routing grace and tear down physical work.
pub async fn serve_with_lifecycle(
    listener: tokio::net::TcpListener,
    app: Router,
    state: Arc<AppState>,
    heartbeat_handle: tokio::task::JoinHandle<()>,
    idle_eviction_handle: tokio::task::JoinHandle<()>,
    config: ShutdownConfig,
) -> Result<()> {
    let ShutdownConfig {
        routing_grace,
        teardown_timeout,
        background_drain,
    } = config;
    let (stop_listener_tx, mut stop_listener_rx) = tokio::sync::watch::channel(false);
    let (stopping_tx, mut stopping_rx) = tokio::sync::oneshot::channel();
    let signal_state = state.clone();
    let mut coordinator = tokio::spawn(async move {
        coordinate_signals(signal_state, routing_grace, stop_listener_tx, stopping_tx).await
    });

    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = stop_listener_rx.wait_for(|stopping| *stopping).await;
    })
    .into_future();
    tokio::pin!(serve);

    let outcome = tokio::select! {
        // The coordinator sends `stopping` immediately before it closes the
        // listener. If both futures become ready in the same scheduler turn,
        // teardown must win; aborting the coordinator here could otherwise
        // leave an active durable run to expire as Abandoned.
        biased;
        stopping = &mut stopping_rx => {
            if stopping.is_err() {
                match coordinator.await {
                    Ok(Err(error)) => Err(error),
                    Ok(Ok(())) => Err(IronCrewError::Validation(
                        "Shutdown coordinator exited before stopping the listener".into(),
                    )),
                    Err(error) => Err(IronCrewError::Validation(format!(
                        "Shutdown coordinator failed: {error}"
                    ))),
                }
            } else {
                idle_eviction_handle.abort();
                match tokio::time::timeout(teardown_timeout, async {
                    let server_result = (&mut serve).await;
                    let coordinator_result = (&mut coordinator).await;
                    server_result.map_err(|error| {
                        IronCrewError::Validation(format!("Server error: {error}"))
                    })?;
                    coordinator_result.map_err(|error| {
                        IronCrewError::Validation(format!("Shutdown coordinator failed: {error}"))
                    })?
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            timeout_seconds = teardown_timeout.as_secs(),
                            "Graceful shutdown exceeded its teardown deadline"
                        );
                        coordinator.abort();
                        Ok(())
                    }
                }
            }
        }
        result = &mut serve => {
            coordinator.abort();
            result.map_err(|error| IronCrewError::Validation(format!("Server error: {error}")))
        }
    };

    heartbeat_handle.abort();
    let _ = heartbeat_handle.await;
    idle_eviction_handle.abort();
    let _ = idle_eviction_handle.await;

    if !background_drain.is_zero() {
        tracing::info!(
            drain_ms = background_drain.as_millis(),
            "Draining background shutdown tasks"
        );
        tokio::time::sleep(background_drain).await;
    }
    outcome
}

async fn coordinate_signals(
    state: Arc<AppState>,
    routing_grace: Duration,
    stop_listener: tokio::sync::watch::Sender<bool>,
    stopping: tokio::sync::oneshot::Sender<()>,
) -> Result<()> {
    wait_for_termination(&state).await?;

    let routing_deadline = tokio::time::Instant::now() + routing_grace;
    state.lifecycle.begin_fencing();
    if state.lifecycle.phase() == api::lifecycle::LifecyclePhase::Fencing {
        let count = fence_owner_until_committed(&state).await;
        state.lifecycle.mark_draining();
        tracing::info!(count, "Durably fenced owned keyed runs for drain");
    }
    tokio::time::sleep_until(routing_deadline).await;

    state.lifecycle.mark_stopping();
    let _ = stopping.send(());
    stop_listener.send_replace(true);
    teardown_active_state(&state).await;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_termination(state: &Arc<AppState>) -> Result<()> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| IronCrewError::Validation(format!("Register SIGTERM: {error}")))?;
    let mut sigusr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .map_err(|error| IronCrewError::Validation(format!("Register SIGUSR1: {error}")))?;

    loop {
        tokio::select! {
            result = &mut ctrl_c => {
                result.map_err(|error| IronCrewError::Validation(format!("Listen for Ctrl+C: {error}")))?;
                tracing::info!("Received Ctrl+C, draining before shutdown");
                return Ok(());
            }
            signal = sigterm.recv() => {
                if signal.is_none() {
                    return Err(IronCrewError::Validation("SIGTERM listener closed".into()));
                }
                tracing::info!("Received SIGTERM, draining before shutdown");
                return Ok(());
            }
            signal = sigusr1.recv() => {
                if signal.is_none() {
                    return Err(IronCrewError::Validation("SIGUSR1 listener closed".into()));
                }
                explicit_drain(state).await;
            }
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_termination(_state: &Arc<AppState>) -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .map_err(|error| IronCrewError::Validation(format!("Listen for Ctrl+C: {error}")))?;
    tracing::info!("Received Ctrl+C, draining before shutdown");
    Ok(())
}

#[cfg(unix)]
async fn explicit_drain(state: &Arc<AppState>) {
    state.lifecycle.begin_fencing();
    if state.lifecycle.phase() != api::lifecycle::LifecyclePhase::Fencing {
        tracing::info!(
            phase = state.lifecycle.phase().as_str(),
            "SIGUSR1 drain is already active"
        );
        return;
    }
    match fence_owner(state).await {
        Ok(count) => {
            state.lifecycle.mark_draining();
            tracing::info!(count, "SIGUSR1 entered explicit drain without stopping");
        }
        Err(error) => {
            tracing::error!(%error, "SIGUSR1 owner-drain fence failed; readiness remains down");
        }
    }
}

async fn fence_owner(state: &AppState) -> Result<usize> {
    let timeout = state
        .store
        .run_maintenance_watchdog()
        .unwrap_or(OWNER_FENCE_FALLBACK_TIMEOUT);
    tokio::time::timeout(timeout, state.store.begin_owner_drain())
        .await
        .map_err(|_| IronCrewError::Validation("Owner-drain store fence timed out".into()))?
}

async fn fence_owner_until_committed(state: &AppState) -> usize {
    let mut attempt = 1_u64;
    let mut retry_delay = OWNER_FENCE_INITIAL_RETRY;
    loop {
        match fence_owner(state).await {
            Ok(count) => return count,
            Err(error) => {
                tracing::warn!(
                    %error,
                    attempt,
                    retry_ms = retry_delay.as_millis(),
                    "Owner-drain fence failed; lifecycle remains fencing and shutdown will retry"
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = next_owner_fence_retry(retry_delay);
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

fn next_owner_fence_retry(current: Duration) -> Duration {
    current.saturating_mul(2).min(OWNER_FENCE_MAX_RETRY)
}

async fn teardown_active_state(state: &AppState) {
    let mut active_runs: Vec<api::ActiveRun> = {
        let mut map = state.active_runs.write().await;
        map.drain().map(|(_, run)| run).collect()
    };
    for run in &active_runs {
        run.abort_handle.abort();
    }
    let mut confirmed_runs = 0usize;
    for run in &mut active_runs {
        while !*run.terminal.borrow() {
            if run.terminal.changed().await.is_err() {
                tracing::warn!("Run terminal acknowledgement channel closed during shutdown");
                break;
            }
        }
        confirmed_runs += usize::from(*run.terminal.borrow());
    }
    if !active_runs.is_empty() {
        tracing::info!(
            total = active_runs.len(),
            confirmed_runs,
            "Stopped active runs"
        );
    }
    drop(active_runs);

    let conversations = {
        let mut map = state.active_conversations.write().await;
        map.drain().map(|(_, handle)| handle).collect::<Vec<_>>()
    };
    for handle in &conversations {
        handle.shutdown.send_replace(true);
    }
    for handle in &conversations {
        let _turn_guard = handle.turn_lock.lock().await;
        // HTTP conversation mutations are durably committed before their
        // response is exposed. Handles in this map are clean caches; saving
        // them during shutdown would create a synthetic revision and can
        // conflict with a peer that advanced the durable conversation.
    }
    if !conversations.is_empty() {
        tracing::info!(count = conversations.len(), "Stopped active conversations");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_fence_retry_starts_fast_and_caps_log_frequency() {
        let mut delay = OWNER_FENCE_INITIAL_RETRY;
        for _ in 0..10 {
            delay = next_owner_fence_retry(delay);
        }
        assert_eq!(delay, OWNER_FENCE_MAX_RETRY);
        assert_eq!(next_owner_fence_retry(delay), OWNER_FENCE_MAX_RETRY);
    }
}
