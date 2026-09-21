//! Foreground activity and idle retirement for resident workspace resources.

use std::{ops::Deref, sync::Arc, time::Duration};

use tokio::time::Instant;

use super::{WorkspaceRuntime, WorkspaceRuntimeManager, lock};

const DEFAULT_IDLE_TTL: Duration = Duration::from_hours(4);
const MAINTENANCE_INTERVAL: Duration = Duration::from_mins(1);

pub(super) struct RuntimeLifecycle {
    last_used: Instant,
    active: usize,
    retired: bool,
}

impl Default for RuntimeLifecycle {
    fn default() -> Self {
        Self {
            last_used: Instant::now(),
            active: 0,
            retired: false,
        }
    }
}

/// An admitted operation protects its runtime until completion or cancellation.
pub(super) struct RuntimeActivity {
    runtime: Arc<WorkspaceRuntime>,
    foreground: bool,
}

impl RuntimeActivity {
    pub(super) fn continuation(&self) -> Self {
        lock(&self.runtime.lifecycle).active += 1;
        Self {
            runtime: Arc::clone(&self.runtime),
            foreground: false,
        }
    }

    pub(super) fn begin(runtime: &Arc<WorkspaceRuntime>, foreground: bool) -> Option<Self> {
        let mut state = lock(&runtime.lifecycle);
        if state.retired {
            return None;
        }
        state.active += 1;
        if foreground {
            state.last_used = Instant::now();
        }
        Some(Self {
            runtime: Arc::clone(runtime),
            foreground,
        })
    }
}

impl Deref for RuntimeActivity {
    type Target = Arc<WorkspaceRuntime>;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl Drop for RuntimeActivity {
    fn drop(&mut self) {
        let mut state = lock(&self.runtime.lifecycle);
        state.active -= 1;
        if self.foreground {
            state.last_used = Instant::now();
        }
    }
}

impl WorkspaceRuntimeManager {
    pub(super) const DEFAULT_IDLE_TTL: Duration = DEFAULT_IDLE_TTL;

    pub(super) fn start_maintenance(&self) {
        let mut task = lock(&self.inner.maintenance);
        if task.is_some() || self.inner.closed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let shutdown = self.inner.shutdown.clone();
        let interval = self.inner.idle_ttl.min(MAINTENANCE_INTERVAL);
        *task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {}
                }
                let Some(inner) = weak.upgrade() else { break };
                let manager = WorkspaceRuntimeManager { inner };
                manager.retire_idle(Instant::now()).await;
            }
        }));
    }

    pub(super) async fn retire_idle(&self, now: Instant) {
        let retired = {
            let mut runtimes = lock(&self.inner.runtimes);
            let mut retired = Vec::new();
            runtimes.retain(|root, runtime| {
                let mut state = lock(&runtime.lifecycle);
                if state.active != 0
                    || now.saturating_duration_since(state.last_used) < self.inner.idle_ttl
                    || self.inner.scheduler.has_active_root(root)
                {
                    return true;
                }
                // Admission and history removal share the map lock. A replacement
                // runtime cannot submit a job that this retirement would cancel.
                state.retired = true;
                self.inner.scheduler.forget_root(root);
                retired.push(Arc::clone(runtime));
                false
            });
            retired
        };
        for runtime in retired {
            if let Err(error) = Self::close_watcher(&runtime).await {
                tracing::warn!(%error, root = %runtime.canonical_root.display(), "idle watcher close failed");
            }
        }
    }
}

impl WorkspaceRuntime {
    pub(super) fn retire(&self) {
        lock(&self.lifecycle).retired = true;
    }

    pub(super) fn is_retired(&self) -> bool {
        lock(&self.lifecycle).retired
    }
}
