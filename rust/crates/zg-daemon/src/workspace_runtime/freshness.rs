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
}

#[derive(Clone, Copy)]
struct Proof {
    revision: u64,
    full_epoch: Option<u64>,
}

impl Default for Freshness {
    fn default() -> Self {
        Self {
            required_epoch: 1,
            reconciled_epoch: 0,
            jobs: HashMap::new(),
        }
    }
}

impl WorkspaceRuntime {
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
                return Err(error);
            }
        };
        if !submitted.reused || submitted.job.state == JobState::Queued {
            let proof = state.jobs.entry(submitted.job.id).or_insert(Proof {
                revision,
                full_epoch,
            });
            proof.revision = proof.revision.max(revision);
            proof.full_epoch = proof.full_epoch.max(full_epoch);
        }
        Ok(submitted)
    }

    pub(super) fn complete_job(&self, completed: &IndexJobCompletion) {
        let job = &completed.job;
        let mut state = lock(&self.freshness);
        let Some(proof) = state.jobs.remove(&job.id) else {
            return;
        };
        if job.state == JobState::Succeeded
            && completed
                .result
                .as_ref()
                .is_some_and(|result| result.files_failed == 0)
        {
            self.indexed_revision
                .fetch_max(proof.revision, Ordering::AcqRel);
            if let Some(epoch) = proof.full_epoch {
                state.reconciled_epoch = state.reconciled_epoch.max(epoch);
            }
        } else {
            state.required_epoch += 1;
        }
        self.invalidate_status();
    }

    pub(super) fn forget_job_proof(&self, id: Uuid) {
        let mut state = lock(&self.freshness);
        if state.jobs.remove(&id).is_some() {
            state.required_epoch += 1;
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
