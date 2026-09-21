//! Queries may lease an active incremental writer until it retires for publication.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;

use crate::{
    models::ModelRuntimeLease,
    storage::IndexStore,
    workspace::{lock::FileLock, manifest::WorkspaceManifest},
};

#[derive(Clone, Default)]
pub(crate) struct WriterContexts {
    entries: Arc<Mutex<HashMap<PathBuf, Weak<WriterSession>>>>,
}

pub(crate) struct WriterSession {
    // Borrowers retain all resources, including the home lock, if indexing is aborted.
    pub(crate) storage: IndexStore,
    pub(crate) models: Vec<ModelRuntimeLease>,
    pub(crate) manifest: WorkspaceManifest,
    active: AtomicUsize,
    drained: Notify,
    _home_lock: Arc<FileLock>,
}

pub(crate) struct WriterRegistration {
    contexts: WriterContexts,
    pub(crate) session: Arc<WriterSession>,
}

pub(crate) struct WriterLease {
    pub(crate) session: Arc<WriterSession>,
}

impl WriterSession {
    pub(crate) fn new(
        storage: IndexStore,
        models: Vec<ModelRuntimeLease>,
        manifest: WorkspaceManifest,
        home_lock: Arc<FileLock>,
    ) -> Self {
        Self {
            storage,
            models,
            manifest,
            active: AtomicUsize::new(0),
            drained: Notify::new(),
            _home_lock: home_lock,
        }
    }
}

impl WriterContexts {
    pub(crate) fn register(&self, session: WriterSession, borrowable: bool) -> WriterRegistration {
        let session = Arc::new(session);
        if borrowable {
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session.manifest.path.clone(), Arc::downgrade(&session));
        }
        WriterRegistration {
            contexts: self.clone(),
            session,
        }
    }

    pub(crate) fn borrow(&self, home: &Path, storage_home: &Path) -> Option<WriterLease> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = entries.get(home)?.upgrade()?;
        // Never expose a rebuild's unpublished generation or a different index.
        if session.manifest.storage_home() != storage_home {
            return None;
        }
        session.active.fetch_add(1, Ordering::AcqRel);
        Some(WriterLease { session })
    }
}

impl WriterRegistration {
    fn unpublish(&self) {
        let mut entries = self
            .contexts
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries
            .get(&self.session.manifest.path)
            .is_some_and(|entry| Weak::ptr_eq(entry, &Arc::downgrade(&self.session)))
        {
            entries.remove(&self.session.manifest.path);
        }
    }

    pub(crate) async fn retire(&self) {
        self.unpublish();
        while self.session.active.load(Ordering::Acquire) != 0 {
            self.session.drained.notified().await;
        }
    }
}

impl Drop for WriterRegistration {
    fn drop(&mut self) {
        self.unpublish();
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        if self.session.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.session.drained.notify_one();
        }
    }
}
