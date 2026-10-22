//! Shared query admission: lease a compatible writer or acquire a workspace read lock.

use std::{
    path::Path,
    sync::{Arc, Weak},
};

use crate::{
    EngineResult,
    workspace::{
        lock::{FileLock, HomeReadProbe, LockWait},
        manifest::read_workspace_manifest,
    },
};

use super::{
    indexed_search::writer::{WriterContexts, WriterLease, WriterSession},
    indexing::service::is_indexed,
};

pub(super) enum ReadSource<T> {
    Writer { lease: WriterLease, context: T },
    Read(FileLock),
}

pub(super) async fn acquire_read_source<T>(
    home: &Path,
    wait: &LockWait<'_>,
    operation: &str,
    writers: Option<&WriterContexts>,
    mut prepare: impl FnMut(&WriterSession) -> EngineResult<Option<T>>,
) -> EngineResult<ReadSource<T>> {
    let mut probe = HomeReadProbe::new(home, operation)?;
    let mut rejected = Weak::new();
    loop {
        wait.check_cancelled()?;
        if let Some(lease) = writers.and_then(|writers| writers.borrow_candidate(home)) {
            let candidate = Arc::downgrade(&lease.session);
            if !Weak::ptr_eq(&rejected, &candidate) {
                // Check once per writer, not once per retry. The lease prevents
                // retirement/publication while this generation is being queried.
                let active = read_workspace_manifest(home)?;
                if active.is_some_and(|manifest| {
                    is_indexed(&manifest)
                        && manifest.storage_home() == lease.session.manifest.storage_home()
                }) && let Some(context) = prepare(&lease.session)?
                {
                    return Ok(ReadSource::Writer { lease, context });
                }
                // A weak reference remembers rejection without delaying retirement.
                rejected = candidate;
            }
        }
        if let Some(lock) = probe.try_acquire()? {
            return Ok(ReadSource::Read(lock));
        }
        // A writer can become available after indexing has opened its storage.
        wait.retry(home, operation).await?;
    }
}
