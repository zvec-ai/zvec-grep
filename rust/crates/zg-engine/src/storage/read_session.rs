//! Resident read handles, leased per query and retired before workspace writes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
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
    closed: AtomicBool,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<PathBuf, Arc<SessionSlot>>,
}

#[derive(Default)]
struct SessionSlot {
    entry: Mutex<Option<CachedSession>>,
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
    cache: Weak<SessionSlot>,
    cached: bool,
}

impl ReadSessionCache {
    pub(crate) fn new(idle_timeout: Duration) -> std::io::Result<Self> {
        let inner = Arc::new(CacheInner {
            state: Mutex::new(CacheState::default()),
            idle_timeout,
            closed: AtomicBool::new(false),
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
        self.acquire_with(home, storage_home, || {
            IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: storage_home.to_path_buf(),
            })
        })
    }

    fn acquire_with(
        &self,
        home: &Path,
        storage_home: &Path,
        open: impl FnOnce() -> EngineResult<IndexStore>,
    ) -> EngineResult<ReadSessionLease> {
        let slot = {
            let mut state = lock(&self.inner.state);
            if self.inner.closed.load(Ordering::Acquire) {
                return Err(EngineError::resource_closed(
                    "index read cache has been closed",
                ));
            }
            Arc::clone(state.entries.entry(home.to_path_buf()).or_default())
        };
        // Only this workspace waits for native open/close; the map owns entries,
        // never native I/O. Concurrent misses for one workspace share this slot.
        let mut entry = lock(&slot.entry);
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(EngineError::resource_closed(
                "index read cache has been closed",
            ));
        }
        if entry
            .as_ref()
            .is_some_and(|entry| entry.session.storage_home != storage_home)
        {
            entry.take();
        }
        if entry.is_none() {
            let residency = acquire_read_write_lock(
                &home.join("locks/read-cache"),
                LockMode::Read,
                "context.cache",
            )?;
            let storage = open()?;
            *entry = Some(CachedSession {
                session: Arc::new(ReadSession {
                    storage,
                    storage_home: storage_home.to_path_buf(),
                    _residency: Some(residency),
                }),
                last_used: Instant::now(),
            });
        }
        let session = Arc::clone(&entry.as_ref().expect("initialized session").session);
        Ok(ReadSessionLease {
            session,
            cache: Arc::downgrade(&slot),
            cached: true,
        })
    }

    pub(crate) fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let entries = std::mem::take(&mut lock(&self.inner.state).entries);
        // In-flight queries retain their own Arc until they finish. Native close
        // runs after releasing the map lock, under the workspace's own slot lock.
        for slot in entries.into_values() {
            lock(&slot.entry).take();
        }
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
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let entries = lock(&self.state)
            .entries
            .iter()
            .map(|(home, slot)| (home.clone(), Arc::clone(slot)))
            .collect::<Vec<_>>();
        for (home, slot) in entries {
            // A cold open or another retirement must not stall maintenance of
            // other workspaces. It will be considered on the next pass.
            let Ok(mut entry) = slot.entry.try_lock() else {
                continue;
            };
            if let Some(cached) = entry.as_ref() {
                if Arc::strong_count(&cached.session) > 1 {
                    continue;
                }
                // A writer first owns the home lock, then waits for residency locks.
                // Releasing on any probe failure is conservative and never serves old data.
                if now.saturating_duration_since(cached.last_used) < self.idle_timeout
                    && home_allows_cached_reads(&home)
                {
                    continue;
                }
                entry.take();
            }
            drop(entry);
            let mut state = lock(&self.state);
            if Arc::strong_count(&slot) == 2
                && state
                    .entries
                    .get(&home)
                    .is_some_and(|current| Arc::ptr_eq(current, &slot))
            {
                state.entries.remove(&home);
            }
        }
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
        let mut entry = lock(&cache.entry);
        if let Some(entry) = entry.as_mut()
            && Arc::ptr_eq(&entry.session, &self.session)
        {
            entry.last_used = Instant::now();
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
        let slot = lock(&cache.state).entries.remove(home);
        if let Some(slot) = slot {
            lock(&slot.entry).take();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
