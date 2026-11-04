//! Microsoft's `DiskANN` graph algorithm over a durable SQLite data provider.
//!
//! Vectors and adjacency lists are read on demand. Mutations are serialized and
//! committed together; deleted nodes remain traversable but cannot be returned.

#[cfg(feature = "experimental-pq")]
pub mod experimental_pq;
#[cfg(feature = "node")]
mod node;
mod provider;
#[cfg(test)]
mod tests;

use std::{collections::HashSet, path::Path, sync::Mutex};

use diskann::{
    graph::{
        self, DiskANNIndex,
        ext::labeled,
        glue,
        search::{InlineFilterSearch, Knn},
    },
    neighbor::Neighbor,
};
use diskann_vector::distance::Metric;
use futures::executor::block_on;
use rusqlite::{Connection, OpenFlags, params};

use provider::{Provider, Strategy};

/// Persisted vector precision. Existing indexes and `open` remain FP32.
/// FP16 stores IEEE binary16 values and widens them for distance evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorPrecision {
    Fp32,
    Fp16,
}

impl VectorPrecision {
    fn version(self) -> u32 {
        match self {
            Self::Fp32 => 1,
            Self::Fp16 => 2,
        }
    }

    fn from_version(version: u32) -> Result<Self> {
        match version {
            1 => Ok(Self::Fp32),
            2 => Ok(Self::Fp16),
            _ => Err(Error::Invalid("unknown DiskANN storage version".into())),
        }
    }

    fn encode(self, vector: &[f32]) -> Vec<u8> {
        match self {
            Self::Fp32 => encode_vector(vector),
            Self::Fp16 => vector
                .iter()
                .flat_map(|&value| half::f16::from_f32(value).to_le_bytes())
                .collect(),
        }
    }
}

/// Supported embedding distance functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distance {
    Cosine,
    DotProduct,
    Euclidean,
}

impl Distance {
    fn metric(self) -> Metric {
        match self {
            Self::Cosine => Metric::Cosine,
            Self::DotProduct => Metric::InnerProduct,
            Self::Euclidean => Metric::L2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::DotProduct => "dot_product",
            Self::Euclidean => "euclidean",
        }
    }

    /// Convert a distance to the engine's larger-is-better ranking score.
    fn score(self, distance: f32) -> f64 {
        match self {
            Self::Cosine => 1.0 - f64::from(distance),
            Self::DotProduct | Self::Euclidean => -f64::from(distance),
        }
    }
}

/// A storage, validation, or graph operation failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
}

diskann::always_escalate!(Error);
diskann::convert_error!(Error);

type Result<T> = std::result::Result<T, Error>;

/// One embedding to insert. Document IDs are unique within an embedding model.
pub struct Record<'a> {
    pub document_id: &'a str,
    pub file_id: u32,
    pub vector: &'a [f32],
}

/// A vector result, ordered by decreasing score.
#[derive(Debug)]
pub struct Hit {
    pub document_id: String,
    pub score: f64,
}

/// A single embedding space. Callers must also hold their workspace storage lock.
pub struct VectorIndex {
    index: Mutex<DiskANNIndex<Provider>>,
    dimension: usize,
    distance: Distance,
    read_only: bool,
    precision: VectorPrecision,
}

impl VectorIndex {
    /// Open an existing index using its persisted embedding schema.
    ///
    /// # Errors
    /// Rejects missing databases and unknown stored metrics.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let (version, dimension, metric): (u32, usize, String) = connection.query_row(
            "SELECT version, dimension, metric FROM settings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let distance = match metric.as_str() {
            "cosine" => Distance::Cosine,
            "dot_product" => Distance::DotProduct,
            "euclidean" => Distance::Euclidean,
            _ => {
                return Err(Error::Invalid(
                    "unknown stored vector metric; rebuild the index".into(),
                ));
            }
        };
        drop(connection);
        Self::open_with_precision(
            path,
            dimension,
            distance,
            true,
            VectorPrecision::from_version(version)?,
        )
    }

    /// Open a persistent index, creating it only when writable and absent.
    ///
    /// # Errors
    /// Rejects missing read-only indexes, incompatible schemas, and invalid dimensions.
    pub fn open(
        path: &Path,
        dimension: usize,
        distance: Distance,
        read_only: bool,
    ) -> Result<Self> {
        Self::open_with_precision(path, dimension, distance, read_only, VectorPrecision::Fp32)
    }

    /// Open or create an index with explicitly selected storage precision.
    /// Never converts an existing index; precision must match its stored format.
    ///
    /// # Errors
    /// Rejects missing read-only indexes, incompatible schemas, and invalid dimensions.
    pub fn open_with_precision(
        path: &Path,
        dimension: usize,
        distance: Distance,
        read_only: bool,
        precision: VectorPrecision,
    ) -> Result<Self> {
        if !(1..=20_000).contains(&dimension) {
            return Err(Error::Invalid(
                "vector dimension must be in 1..=20,000".into(),
            ));
        }
        let exists = path
            .try_exists()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        };
        let connection = Connection::open_with_flags(path, flags)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        // Keep private caches small. The bounded mapping shares clean pages with
        // the OS instead of retaining another copy of every hot database page.
        // SQLite falls back to ordinary reads when mapping is unavailable.
        connection.execute_batch(if read_only {
            "PRAGMA cache_size = -256; PRAGMA mmap_size = 16777216;"
        } else {
            // Writers already retain dirty pages. Avoid additionally keeping
            // mapped read pages resident throughout graph construction.
            "PRAGMA cache_size = -2048; PRAGMA mmap_size = 0;"
        })?;
        if !read_only {
            connection.execute_batch("PRAGMA journal_mode = DELETE; PRAGMA synchronous = FULL;")?;
        }
        if !exists {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE settings (version INTEGER NOT NULL, dimension INTEGER NOT NULL, metric TEXT NOT NULL);
                CREATE TABLE nodes (id INTEGER PRIMARY KEY, document_id TEXT, file_id INTEGER,
                    vector BLOB NOT NULL, neighbors BLOB NOT NULL, live INTEGER NOT NULL);
                CREATE UNIQUE INDEX live_document ON nodes(document_id) WHERE live = 1;
                CREATE INDEX file_nodes ON nodes(file_id) WHERE live = 1;")?;
            connection.execute(
                "INSERT INTO settings VALUES (?1, ?2, ?3)",
                params![precision.version(), dimension, distance.name()],
            )?;
            let mut start = vec![0.0_f32; dimension];
            start[0] = 1.0;
            connection.execute(
                "INSERT INTO nodes VALUES (0, NULL, NULL, ?1, x'', 0)",
                [precision.encode(&start)],
            )?;
            connection.execute_batch("COMMIT")?;
        }
        let schema: (u32, usize, String) = connection.query_row(
            "SELECT version, dimension, metric FROM settings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if schema != (precision.version(), dimension, distance.name().to_owned()) {
            return Err(Error::Invalid(
                "incompatible DiskANN storage; rebuild the index".into(),
            ));
        }
        let provider = Provider::new(connection, dimension, distance, precision);
        provider.vector(0)?;
        let config = graph::config::Builder::new(
            48,
            graph::config::MaxDegree::default_slack(),
            100,
            distance.metric().into(),
        )
        .build()
        .map_err(graph_error)?;
        Ok(Self {
            index: Mutex::new(DiskANNIndex::new(config, provider, None)),
            dimension,
            distance,
            read_only,
            precision,
        })
    }

    /// Atomically insert or replace embeddings and their graph edges.
    ///
    /// # Errors
    /// Returns an error for read-only storage, invalid vectors, or failed writes.
    pub fn upsert(&self, records: &[Record<'_>]) -> Result<()> {
        self.writable()?;
        for record in records {
            self.validate(record.vector)?;
            if self.precision == VectorPrecision::Fp16
                && record
                    .vector
                    .iter()
                    .any(|&v| !half::f16::from_f32(v).is_finite())
            {
                return Err(Error::Invalid("vector overflows FP16 storage".into()));
            }
        }
        let index = self.index.lock().map_err(poisoned)?;
        let provider = &index.data_provider;
        transaction(provider, || {
            for record in records {
                // Graph construction must see the same rounded vector that is stored.
                let rounded;
                let vector = if self.precision == VectorPrecision::Fp16 {
                    rounded = record
                        .vector
                        .iter()
                        .map(|&v| half::f16::from_f32(v).to_f32())
                        .collect::<Vec<_>>();
                    rounded.as_slice()
                } else {
                    record.vector
                };
                let id: u32 = provider.connection()?.query_row(
                    "SELECT COALESCE(MAX(id), 0) + 1 FROM nodes",
                    [],
                    |row| row.get(0),
                )?;
                block_on(index.insert(&Strategy, &diskann::provider::DefaultContext, &id, vector))
                    .map_err(graph_error)?;
                let connection = provider.connection()?;
                // Match the partial live_document index instead of scanning
                // every stored vector, including historical replacements.
                connection.execute(
                    "UPDATE nodes SET live = 0 WHERE document_id = ?1 AND live = 1",
                    [record.document_id],
                )?;
                connection.execute(
                    "UPDATE nodes SET document_id = ?1, file_id = ?2, live = 1 WHERE id = ?3",
                    params![record.document_id, record.file_id, id],
                )?;
            }
            Ok(())
        })
    }

    /// Hide a file's embeddings without breaking paths through the graph.
    ///
    /// # Errors
    /// Returns an error for read-only storage or failed writes.
    pub fn delete_file(&self, file_id: u32) -> Result<()> {
        self.writable()?;
        let index = self.index.lock().map_err(poisoned)?;
        index.data_provider.connection()?.execute(
            "UPDATE nodes SET live = 0 WHERE file_id = ?1 AND live = 1",
            [file_id],
        )?;
        Ok(())
    }

    /// Delete selected document IDs, used by clients with non-numeric file IDs.
    ///
    /// # Errors
    /// Returns an error for read-only storage or failed writes.
    pub fn delete_documents(&self, ids: &[String]) -> Result<()> {
        self.writable()?;
        let index = self.index.lock().map_err(poisoned)?;
        transaction(&index.data_provider, || {
            let connection = index.data_provider.connection()?;
            let mut statement = connection
                .prepare_cached("UPDATE nodes SET live = 0 WHERE document_id = ?1 AND live = 1")?;
            for id in ids {
                statement.execute([id])?;
            }
            Ok(())
        })
    }

    /// Search with optional document membership, applied inside `DiskANN` traversal.
    ///
    /// # Errors
    /// Returns an error for invalid vectors or unreadable storage.
    pub fn search(
        &self,
        vector: &[f32],
        limit: usize,
        allowed: Option<&HashSet<String>>,
    ) -> Result<Vec<Hit>> {
        self.search_with_list_size(vector, limit, allowed, 512)
    }

    /// Search with an explicit candidate-list size for recall/latency tuning.
    /// The traversal uses at least twice `limit`, and filtered searches may expand it.
    ///
    /// # Errors
    /// Rejects zero list sizes, invalid vectors, and unreadable storage.
    pub fn search_with_list_size(
        &self,
        vector: &[f32],
        limit: usize,
        allowed: Option<&HashSet<String>>,
        list_size: usize,
    ) -> Result<Vec<Hit>> {
        if list_size == 0 {
            return Err(Error::Invalid("search list size must be positive".into()));
        }
        self.validate(vector)?;
        if limit == 0 || allowed.is_some_and(HashSet::is_empty) {
            return Ok(Vec::new());
        }
        let index = self.index.lock().map_err(poisoned)?;
        index.data_provider.clear_query_cache()?;
        let live: usize = index.data_provider.connection()?.query_row(
            // Only determine whether there are enough live nodes to fill top-k.
            // Counting the entire corpus on every query needlessly scans its index.
            "SELECT COUNT(*) FROM (SELECT 1 FROM nodes WHERE live = 1 LIMIT ?1)",
            [i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )?;
        if live == 0 {
            return Ok(Vec::new());
        }
        let filter = LiveFilter {
            provider: &index.data_provider,
            allowed,
            error: Mutex::new(None),
        };
        let strategy = labeled::Filtered::new(Strategy, &filter);
        let count = limit
            .min(live)
            .min(allowed.map_or(usize::MAX, HashSet::len));
        let search = InlineFilterSearch::new(
            // Keep query effort independent of graph-build effort. A 100-entry
            // beam under-searched the high-dimensional regression corpus.
            Knn::new_default(count.saturating_mul(2).max(list_size)).map_err(graph_error)?,
            Some(graph::search::AdaptiveL::new(32, 8.0).map_err(graph_error)?),
        );
        let mut neighbors = Vec::<Neighbor<u32>>::new();
        block_on(index.search_with(
            search,
            &strategy,
            glue::CopyIds,
            &diskann::provider::DefaultContext,
            vector,
            &mut neighbors,
        ))
        .map_err(graph_error)?;
        if let Some(error) = filter.error.into_inner().map_err(poisoned)? {
            return Err(error);
        }
        let mut hits = Vec::new();
        // DiskANN returns candidates sorted by distance. Resolve only the best
        // results, retaining boundary ties for deterministic document-ID ordering.
        let cutoff = neighbors
            .get(count - 1)
            .map(|neighbor| *neighbor.distance());
        for neighbor in neighbors
            .into_iter()
            .take_while(|neighbor| cutoff.is_none_or(|cutoff| *neighbor.distance() <= cutoff))
        {
            if let Some(document_id) = index.data_provider.document(*neighbor.id())? {
                hits.push(Hit {
                    document_id,
                    score: self.distance.score(*neighbor.distance()),
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.document_id.cmp(&b.document_id))
        });
        hits.truncate(count);
        Ok(hits)
    }

    fn validate(&self, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dimension || vector.iter().any(|v| !v.is_finite()) {
            return Err(Error::Invalid(format!(
                "expected a finite {}-dimensional vector",
                self.dimension
            )));
        }
        Ok(())
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            Err(Error::Invalid("DiskANN index is read-only".into()))
        } else {
            Ok(())
        }
    }
}

struct LiveFilter<'a> {
    provider: &'a Provider,
    allowed: Option<&'a HashSet<String>>,
    error: Mutex<Option<Error>>,
}

impl std::fmt::Debug for LiveFilter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveFilter").finish_non_exhaustive()
    }
}

impl labeled::QueryLabelProvider<u32> for LiveFilter<'_> {
    fn is_match(&self, id: u32) -> bool {
        match self.provider.matches(id, self.allowed) {
            Ok(matches) => matches,
            Err(error) => {
                // The predicate cannot return errors. Preserve one and fail the
                // whole query immediately after traversal instead of hiding it.
                if let Ok(mut failure) = self.error.lock() {
                    *failure = Some(error);
                }
                false
            }
        }
    }
}

fn transaction<T>(provider: &Provider, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    provider.connection()?.execute_batch("BEGIN IMMEDIATE")?;
    let result = operation().and_then(|value| {
        provider.connection()?.execute_batch("COMMIT")?;
        Ok(value)
    });
    if result.is_err() {
        provider.connection()?.execute_batch("ROLLBACK")?;
    }
    result
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn graph_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("DiskANN: {error}"))
}
fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::Invalid("DiskANN storage lock was poisoned".into())
}
