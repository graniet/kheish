//! Active ingress runners for daemon-managed connectors.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tracing::warn;

use kheish_core::ModelDriver;

use crate::DaemonState;
use crate::connectors::config::{ResolvedTelegramConnector, TelegramIngressMode};

mod external;
mod telegram;

struct RunningTelegramPoller {
    connector: ResolvedTelegramConnector,
    abort: tokio::task::AbortHandle,
}

pub(crate) struct IngressTasks {
    pub(crate) handles: Vec<JoinHandle<()>>,
    pub(crate) shutdown: watch::Sender<bool>,
}

/// Spawns all active ingress runners configured for the daemon.
pub(crate) fn spawn_ingress_tasks<M>(state: Arc<DaemonState<M>>) -> IngressTasks
where
    M: ModelDriver + Send + Sync + 'static,
{
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let telegram_state = state.clone();
    let telegram_shutdown_rx = shutdown_rx.clone();
    let telegram_task = tokio::spawn(async move {
        let mut shutdown_rx = telegram_shutdown_rx;
        let mut revision_rx = state.connectors().subscribe();
        let polling_state = Arc::new(telegram::TelegramPollingState::default());
        let mut running: BTreeMap<String, RunningTelegramPoller> = BTreeMap::new();
        let mut join_set = JoinSet::new();
        reconcile_telegram_pollers(&state, polling_state.clone(), &mut running, &mut join_set);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        for (_, poller) in std::mem::take(&mut running) {
                            poller.abort.abort();
                        }
                        while join_set.join_next().await.is_some() {}
                    }
                    return;
                }
                changed = revision_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    reconcile_telegram_pollers(&state, polling_state.clone(), &mut running, &mut join_set);
                }
                Some(result) = join_set.join_next() => {
                    match result {
                        Ok((name, Ok(()))) => {
                            running.remove(&name);
                            reconcile_telegram_pollers(&state, polling_state.clone(), &mut running, &mut join_set);
                        }
                        Ok((name, Err(error))) => {
                            running.remove(&name);
                            warn!(connector = %name, error = ?error, "telegram polling task exited");
                            reconcile_telegram_pollers(&state, polling_state.clone(), &mut running, &mut join_set);
                        }
                        Err(error) => {
                            warn!(error = ?error, "telegram polling task panicked");
                            reconcile_telegram_pollers(&state, polling_state.clone(), &mut running, &mut join_set);
                        }
                    }
                }
            }
        }
    });
    let external_task = external::spawn_external_process_supervisor(telegram_state, shutdown_rx);
    IngressTasks {
        handles: vec![telegram_task, external_task],
        shutdown: shutdown_tx,
    }
}

fn reconcile_telegram_pollers<M>(
    state: &Arc<DaemonState<M>>,
    polling_state: Arc<telegram::TelegramPollingState>,
    running: &mut BTreeMap<String, RunningTelegramPoller>,
    join_set: &mut JoinSet<(String, anyhow::Result<()>)>,
) where
    M: ModelDriver + Send + Sync + 'static,
{
    let desired = state
        .connectors()
        .telegram_connectors()
        .into_iter()
        .filter(|connector| connector.ingress_mode == TelegramIngressMode::Polling)
        .map(|connector| (connector.name.clone(), connector))
        .collect::<BTreeMap<String, ResolvedTelegramConnector>>();

    let stale = running
        .iter()
        .filter(|(name, poller)| {
            desired
                .get(*name)
                .map(|connector| connector != &poller.connector)
                .unwrap_or(true)
        })
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    for name in stale {
        if let Some(poller) = running.remove(&name) {
            poller.abort.abort();
        }
    }

    for (name, connector) in desired {
        if running.contains_key(&name) {
            continue;
        }
        let state = state.clone();
        let polling_state = polling_state.clone();
        let task_name = name.clone();
        let task_connector = connector.clone();
        let abort = join_set.spawn(async move {
            let result = telegram::poll_connector_loop(state, polling_state, task_connector).await;
            (task_name, result)
        });
        running.insert(name, RunningTelegramPoller { connector, abort });
    }
}
