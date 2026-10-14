//! Reconciliation proofs belong to the revision and recovery epoch a job observed.

use std::{collections::HashMap, sync::atomic::Ordering};

use futures::FutureExt;
use uuid::Uuid;

use super::{IndexOptions, ScanRulesUpdate, WorkspaceRuntime, lock};
use crate::job_scheduler::{
    IndexJobCompletion, IndexJobScheduler, JobReason, JobState, SchedulerError,
    SubmitIndexJobResult,
};

pub(super) struct Freshness {
    required_epoch: u64,
    reconciled_epoch: u64,
    jobs: HashMap<Uuid, Proof>,
    // Recovery epochs request checks; only observed file changes belong in this evidence.
    change_version: u64,
    unsubmitted_change: Option<u64>,
    unreconciled_change: Option<u64>,
}

struct Proof {
    revision: u64,
    status_epoch: u64,
    full_epoch: Option<u64>,
    full: bool,
    has_changes: bool,
}

/// A read remembers evidence even if an update finishes before the response is formatted.
#[derive(Clone, Copy, Default)]
pub(super) struct SearchFreshness {
    change_version: u64,
    pending_changes: bool,
}

impl SearchFreshness {
    pub(super) fn changed_since(self, before: Self) -> bool {
        before.pending_changes
            || self.pending_changes
            || self.change_version != before.change_version
    }
}

impl Freshness {
    fn observe_change(&mut self, revision: u64) {
        self.change_version += 1;
        self.unsubmitted_change = self.unsubmitted_change.max(Some(revision));
    }

    fn lose_proof(&mut self, proof: &Proof) {
        self.required_epoch += 1;
        if proof.has_changes {
            // Retain one bounded watermark for failed or lost change batches.
            self.unreconciled_change = self.unreconciled_change.max(Some(proof.revision));
        }
    }
}

impl Default for Freshness {
    fn default() -> Self {
        Self {
            required_epoch: 1,
            reconciled_epoch: 0,
            jobs: HashMap::new(),
            change_version: 0,
            unsubmitted_change: None,
            unreconciled_change: None,
        }
    }
}

impl WorkspaceRuntime {
    pub(super) fn observe_file_changes(&self, revision: u64) {
        lock(&self.freshness).observe_change(revision);
    }

    pub(super) fn search_freshness(&self) -> SearchFreshness {
        let state = lock(&self.freshness);
        SearchFreshness {
            change_version: state.change_version,
            pending_changes: state.unsubmitted_change.is_some()
                || state.unreconciled_change.is_some()
                || state.jobs.values().any(|proof| proof.has_changes),
        }
    }

    pub(super) fn record_status(
        &self,
        epoch: u64,
        info: &super::InfoResult,
        job: Option<(Uuid, JobState)>,
    ) -> bool {
        let mut state = lock(&self.freshness);
        if !lock(&self.index_status).record(epoch, info, job) {
            return false;
        }
        if info.status.as_ref().is_some_and(|stats| {
            stats.files_added > 0 || stats.files_modified > 0 || stats.files_deleted > 0
        }) {
            // A disk inspection can discover changes that the watcher missed.
            let revision = self.dirty_revision.fetch_add(1, Ordering::AcqRel) + 1;
            state.change_version += 1;
            state.unreconciled_change = state.unreconciled_change.max(Some(revision));
        }
        if matches!(
            info.index_status(),
            zg_engine::api::info::result::IndexStatus::Stale
                | zg_engine::api::info::result::IndexStatus::Failed
        ) {
            // Unfinished files still need repair even without evidence that their source changed.
            state.required_epoch += 1;
        }
        true
    }

    pub(super) fn require_reconciliation(&self) {
        lock(&self.freshness).required_epoch += 1;
        self.invalidate_status();
    }

    pub(super) fn is_reconciled(&self) -> bool {
        let state = lock(&self.freshness);
        self.watcher_active.load(Ordering::Acquire)
            && state.jobs.is_empty()
            && state.reconciled_epoch == state.required_epoch
            && self.indexed_revision.load(Ordering::Acquire)
                == self.dirty_revision.load(Ordering::Acquire)
    }

    pub(super) fn submit_tracked(
        &self,
        scheduler: &IndexJobScheduler,
        options: IndexOptions,
        reason: JobReason,
    ) -> Result<SubmitIndexJobResult, SchedulerError> {
        let mut state = lock(&self.freshness);
        let full = options.changes.is_empty()
            || options.changes.contains(&super::IndexChange::Rescan)
            || options.reset_paths
            || options.scan != ScanRulesUpdate::default();
        let revision = self.dirty_revision.load(Ordering::Acquire);
        let full_epoch = (reason == JobReason::Watch && full).then_some(state.required_epoch);
        let submitted = match scheduler.submit(self.canonical_root.clone(), options, reason) {
            Ok(submitted) => submitted,
            Err(error) => {
                // A later narrow batch cannot repair changes rejected by the queue.
                state.required_epoch += 1;
                if state.unsubmitted_change == Some(revision) {
                    state.unsubmitted_change = None;
                    state.unreconciled_change = state.unreconciled_change.max(Some(revision));
                }
                return Err(error);
            }
        };
        if !submitted.reused || submitted.job.state == JobState::Queued {
            let status_epoch = {
                let mut cache = lock(&self.index_status);
                let epoch = cache.invalidate();
                if reason == JobReason::Manual || !full {
                    // These jobs did not inspect the current workspace for a completion baseline.
                    cache.completion_baseline = None;
                }
                epoch
            };
            let known_change =
                reason == JobReason::Watch && state.unsubmitted_change == Some(revision);
            if known_change {
                state.unsubmitted_change = None;
            }
            let proof = state.jobs.entry(submitted.job.id).or_insert(Proof {
                revision,
                status_epoch,
                full_epoch,
                full,
                has_changes: known_change,
            });
            proof.revision = proof.revision.max(revision);
            proof.status_epoch = proof.status_epoch.max(status_epoch);
            proof.full_epoch = proof.full_epoch.max(full_epoch);
            proof.full |= full;
            proof.has_changes |= known_change;
        }
        Ok(submitted)
    }

    pub(super) fn complete_job(&self, completed: &IndexJobCompletion) {
        let job = &completed.job;
        let mut state = lock(&self.freshness);
        let Some(mut proof) = state.jobs.remove(&job.id) else {
            return;
        };
        if completed.result.as_ref().is_some_and(|result| {
            result.files_added > 0 || result.files_modified > 0 || result.files_deleted > 0
        }) {
            // Indexing may find drift without any exact watcher notification.
            state.change_version += 1;
            proof.has_changes = true;
        }
        let succeeded = job.state == JobState::Succeeded
            && completed
                .result
                .as_ref()
                .is_some_and(|result| result.files_failed == 0);
        if succeeded {
            if proof.full {
                state.unreconciled_change = state
                    .unreconciled_change
                    .filter(|revision| *revision > proof.revision);
                state.unsubmitted_change = state
                    .unsubmitted_change
                    .filter(|revision| *revision > proof.revision);
            }
            self.indexed_revision
                .fetch_max(proof.revision, Ordering::AcqRel);
            if let Some(epoch) = proof.full_epoch {
                state.reconciled_epoch = state.reconciled_epoch.max(epoch);
            }
        } else {
            state.lose_proof(&proof);
        }
        let mut cache = lock(&self.index_status);
        if completed
            .result
            .as_ref()
            .is_some_and(|result| result.files_failed > 0)
        {
            cache.needs_repair = true;
        } else if succeeded && proof.full && cache.epoch == proof.status_epoch {
            // A later inspection or mutation must not be cleared by an older job.
            cache.needs_repair = false;
        }
        cache.invalidate();
        cache.completion_baseline = None;
    }

    pub(super) fn forget_job_proof(&self, id: Uuid) {
        let mut state = lock(&self.freshness);
        if let Some(proof) = state.jobs.remove(&id) {
            state.lose_proof(&proof);
        }
    }

    pub(super) fn settle_completed_jobs(&self, scheduler: &IndexJobScheduler) -> bool {
        let jobs = lock(&self.freshness)
            .jobs
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in jobs {
            if let Some(result) = scheduler.wait(id).now_or_never() {
                match result {
                    Ok(completed) => self.complete_job(&completed),
                    Err(_) => {
                        self.forget_job_proof(id);
                    }
                }
            }
        }
        lock(&self.freshness).jobs.is_empty()
    }

    pub(super) async fn settle_jobs(
        &self,
        scheduler: &IndexJobScheduler,
        reporter: Option<zg_engine::api::index::progress::IndexProgressReporter>,
    ) {
        loop {
            let jobs = lock(&self.freshness)
                .jobs
                .keys()
                .copied()
                .collect::<Vec<_>>();
            if jobs.is_empty() {
                return;
            }
            for id in jobs {
                match scheduler.wait_with_progress(id, reporter.clone()).await {
                    Ok(completed) => self.complete_job(&completed),
                    Err(_) => {
                        self.forget_job_proof(id);
                    }
                }
            }
        }
    }
}
