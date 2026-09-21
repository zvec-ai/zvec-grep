//! Resident read handles, leased per query and retired before workspace writes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{Duration, Instant},
};

use crate::{
    EngineError, EngineResult,
    workspace::lock::{FileLock, LockMode, acquire_read_write_lock, home_allows_cached_reads},
};

use super::{IndexStore, types::WorkspaceIndexStorageOptions};

const REAP_INTERVAL: Duration = Duration::from_millis(50);
static CACHES: OnceLock<Mutex<Vec<Weak<CacheInner>>>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct ReadSessionCache {
    inner: Arc<CacheInner>,
}

struct CacheInner {
    state: Mutex<CacheState>,
    idle_timeout: Duration,
}

#[derive(Default)]
struct CacheState {
    closed: bool,
    entries: HashMap<PathBuf, CachedSession>,
}

struct CachedSession {
    session: Arc<ReadSession>,
    last_used: Instant,
}

struct ReadSession {
    // Native handles must close before releasing the residency lock.
    storage: IndexStore,
    storage_home: PathBuf,
    _residency: Option<FileLock>,
}

pub(crate) struct ReadSessionLease {
    session: Arc<ReadSession>,
    home: PathBuf,
    cache: Weak<CacheInner>,
    cached: bool,
}

impl ReadSessionCache {
    pub(crate) fn new(idle_timeout: Duration) -> std::io::Result<Self> {
        let inner = Arc::new(CacheInner {
            state: Mutex::new(CacheState::default()),
            idle_timeout,
        });
        let weak = Arc::downgrade(&inner);
        // A native thread can release idle handles even when a synchronous writer
        // is waiting for another process's cache on a single-threaded executor.
        std::thread::Builder::new()
            .name("index-read-cache".into())
            .spawn(move || {
                while let Some(inner) = weak.upgrade() {
                    if !inner.reap(Instant::now()) {
                        break;
                    }
                    drop(inner);
                    std::thread::sleep(REAP_INTERVAL);
                }
            })?;
        let mut caches = lock(CACHES.get_or_init(Mutex::default));
        caches.retain(|cache| cache.strong_count() > 0);
        caches.push(Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    /// The caller holds the workspace home read lock throughout the lease.
    pub(crate) fn acquire(
        &self,
        home: &Path,
        storage_home: &Path,
    ) -> EngineResult<ReadSessionLease> {
        let mut state = lock(&self.inner.state);
        if state.closed {
            return Err(EngineError::resource_closed(
                "index read cache has been closed",
            ));
        }
        if state
            .entries
            .get(home)
            .is_some_and(|entry| entry.session.storage_home != storage_home)
        {
            state.entries.remove(home);
        }
        // Opening under the cache lock also single-flights concurrent misses.
        let entry = match state.entries.entry(home.to_path_buf()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let residency = acquire_read_write_lock(
                    &home.join("locks/read-cache"),
                    LockMode::Read,
                    "context.cache",
                )?;
                let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
                    storage_path: storage_home.to_path_buf(),
                })?;
                entry.insert(CachedSession {
                    session: Arc::new(ReadSession {
                        storage,
                        storage_home: storage_home.to_path_buf(),
                        _residency: Some(residency),
                    }),
                    last_used: Instant::now(),
                })
            }
        };
        Ok(ReadSessionLease {
            session: Arc::clone(&entry.session),
            home: home.to_path_buf(),
            cache: Arc::downgrade(&self.inner),
            cached: true,
        })
    }

    pub(crate) fn close(&self) {
        let mut state = lock(&self.inner.state);
        state.closed = true;
        // In-flight queries retain their own Arc until they finish.
        state.entries.clear();
    }
}

impl std::fmt::Debug for ReadSessionCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadSessionCache")
            .field("entries", &lock(&self.inner.state).entries.len())
            .finish_non_exhaustive()
    }
}

impl CacheInner {
    fn reap(&self, now: Instant) -> bool {
        let mut state = lock(&self.state);
        if state.closed {
            return false;
        }
        state.entries.retain(|home, entry| {
            if Arc::strong_count(&entry.session) > 1 {
                return true;
            }
            if now.saturating_duration_since(entry.last_used) >= self.idle_timeout {
                return false;
            }
            // A writer first owns the home lock, then waits for residency locks.
            // Releasing on any probe failure is conservative and never serves old data.
            home_allows_cached_reads(home)
        });
        true
    }
}

impl ReadSessionLease {
    pub(crate) fn open(storage_home: &Path) -> EngineResult<Self> {
        Ok(Self {
            session: Arc::new(ReadSession {
                storage: IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
                    storage_path: storage_home.to_path_buf(),
                })?,
                storage_home: storage_home.to_path_buf(),
                _residency: None,
            }),
            home: PathBuf::new(),
            cache: Weak::new(),
            cached: false,
        })
    }

    pub(crate) fn storage(&self) -> &IndexStore {
        &self.session.storage
    }

    pub(crate) fn close(self) -> EngineResult<()> {
        if !self.cached {
            self.session.storage.close()?;
        }
        Ok(())
    }
}

impl Drop for ReadSessionLease {
    fn drop(&mut self) {
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        let mut state = lock(&cache.state);
        if let Some(entry) = state.entries.get_mut(&self.home)
            && Arc::ptr_eq(&entry.session, &self.session)
        {
            entry.last_used = Instant::now();
            if cache.idle_timeout.is_zero() {
                state.entries.remove(&self.home);
            }
        }
    }
}

/// Called after acquiring the exclusive home lock, before waiting for remote caches.
pub(crate) fn release_for_write(home: &Path) {
    let Some(caches) = CACHES.get() else {
        return;
    };
    let caches = lock(caches)
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for cache in caches {
        lock(&cache.state).entries.remove(home);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
