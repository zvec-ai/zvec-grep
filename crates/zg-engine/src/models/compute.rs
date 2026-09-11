//! Process-level execution pool for CPU-bound model work.

use std::{
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
};

use rayon::{ThreadPool, ThreadPoolBuilder};
use tokio::sync::oneshot;

use super::spi::ModelError;

#[derive(Clone)]
pub(crate) struct ModelComputeRuntime {
    inner: Arc<ComputeInner>,
}

struct ComputeInner {
    pool: OnceLock<Result<ThreadPool, String>>,
    capacity: usize,
}

impl ModelComputeRuntime {
    pub(crate) fn shared() -> Self {
        static SHARED: OnceLock<ModelComputeRuntime> = OnceLock::new();
        SHARED
            .get_or_init(|| Self::with_capacity(default_capacity()))
            .clone()
    }

    fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(ComputeInner {
                pool: OnceLock::new(),
                capacity,
            }),
        }
    }

    pub(crate) async fn run<F, T>(&self, task: F) -> Result<T, ModelError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.inner.pool()?.spawn(move || {
            let _result = sender.send(task());
        });
        receiver
            .await
            .map_err(|error| ModelError::internal("Model compute worker failed").with_cause(error))
    }
}

impl ComputeInner {
    fn pool(&self) -> Result<&ThreadPool, ModelError> {
        match self.pool.get_or_init(|| {
            ThreadPoolBuilder::new()
                .num_threads(self.capacity)
                .thread_name(|index| format!("zg-model-compute-{index}"))
                .build()
                .map_err(|error| error.to_string())
        }) {
            Ok(pool) => Ok(pool),
            Err(error) => {
                Err(ModelError::internal("Unable to create model compute pool").with_cause(error))
            }
        }
    }
}

impl Default for ModelComputeRuntime {
    fn default() -> Self {
        Self::shared()
    }
}

fn default_capacity() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}
