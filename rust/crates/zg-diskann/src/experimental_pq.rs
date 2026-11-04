//! Experimental PQ traversal on an immutable, contiguous-ID FP16 benchmark snapshot.
//! Graph edges and full vectors remain in SQLite. Production defaults are unchanged.
//! The caller must verify the sidecar's source-vector hash before opening it.

#![allow(
    clippy::unused_async_trait_impl,
    reason = "DiskANN's provider contract is async"
)]

use super::provider::Provider;
use super::{Distance, Error, Hit, Result, VectorIndex, VectorPrecision, graph_error, poisoned};
use diskann::{
    ANNResult,
    graph::{
        ext::labeled,
        glue,
        search::{InlineFilterSearch, Knn},
    },
    provider::DefaultContext,
};
use diskann_quantization::{
    distances,
    product::tables::{
        TransposedTable,
        lookup::{self, DotAndNorm},
    },
    views::ChunkOffsets,
};
use diskann_utils::views::rowmajor::{self, Matrix, MatrixMut};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

pub const MAGIC: &[u8; 8] = b"ZGPQ0001";

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("experimental PQ: {error}"))
}

/// All PQ codes of the selected prefix are owned resident memory, not an mmap.
pub struct PqIndex {
    table: TransposedTable,
    codes: Vec<u8>,
    count: usize,
}

impl PqIndex {
    /// Load a prefix of a verified PQ sidecar for a static, all-live FP16 snapshot.
    ///
    /// # Errors
    /// Rejects malformed tables, truncated codes, unsupported indexes and sparse/deleted IDs.
    pub fn load(index: &VectorIndex, path: &Path, count: usize) -> Result<Self> {
        if !index.read_only
            || index.distance != Distance::Cosine
            || index.precision != VectorPrecision::Fp16
        {
            return Err(invalid("requires a read-only FP16 cosine index"));
        }
        let mut reader = BufReader::new(File::open(path).map_err(invalid)?);
        let mut header = [0_u8; 24];
        reader.read_exact(&mut header).map_err(invalid)?;
        if &header[..8] != MAGIC {
            return Err(invalid("bad magic"));
        }
        let dimension = u32::from_le_bytes(header[8..12].try_into().map_err(invalid)?) as usize;
        let chunks = u32::from_le_bytes(header[12..16].try_into().map_err(invalid)?) as usize;
        let total = usize::try_from(u64::from_le_bytes(
            header[16..24].try_into().map_err(invalid)?,
        ))
        .map_err(invalid)?;
        if dimension != index.dimension
            || chunks == 0
            || chunks > dimension
            || !dimension.is_multiple_of(chunks)
            || count == 0
            || count > total
            || total > u32::MAX as usize
        {
            return Err(invalid("invalid shape"));
        }
        let expected = 24_u64 + 256 * dimension as u64 * 4 + total as u64 * chunks as u64;
        if reader.get_ref().metadata().map_err(invalid)?.len() != expected {
            return Err(invalid("invalid file size"));
        }
        let mut values = Vec::with_capacity(256 * dimension);
        for _ in 0..256 * dimension {
            let mut bytes = [0_u8; 4];
            reader.read_exact(&mut bytes).map_err(invalid)?;
            let value = f32::from_le_bytes(bytes);
            if !value.is_finite() {
                return Err(invalid("non-finite pivot"));
            }
            values.push(value);
        }
        let pivots = rowmajor::Owned::try_from_data(values.into_boxed_slice(), 256, dimension)
            .map_err(invalid)?;
        let offsets = ChunkOffsets::new(
            (0..=chunks)
                .map(|i| i * (dimension / chunks))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
        .map_err(invalid)?;
        let table = TransposedTable::from_parts(pivots.as_view(), offsets).map_err(invalid)?;
        let mut codes = vec![
            0_u8;
            count
                .checked_mul(chunks)
                .ok_or_else(|| invalid("size overflow"))?
        ];
        reader.read_exact(&mut codes).map_err(invalid)?;
        let locked = index.index.lock().map_err(poisoned)?;
        let db = locked.data_provider.connection()?;
        let shape: (usize, usize, usize) = db.query_row(
            "SELECT COUNT(*),MIN(id),MAX(id) FROM nodes WHERE live=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let nodes: usize = db.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        if shape != (count, 1, count) || nodes != count + 1 {
            return Err(invalid(
                "requires contiguous live IDs 1..count and entry node 0",
            ));
        }
        Ok(Self {
            table,
            codes,
            count,
        })
    }

    #[must_use]
    pub fn code_bytes(&self) -> usize {
        self.codes.len()
    }

    fn distance(&self, id: u32, lut: &rowmajor::Owned<DotAndNorm>, norm: f32) -> Result<f32> {
        let id = id as usize;
        if id == 0 || id > self.count {
            return Err(invalid("node outside code range"));
        }
        let m = self.table.nchunks();
        let partial = lookup::lookup_single(
            lookup::Sum,
            lut.as_view(),
            &self.codes[(id - 1) * m..id * m],
        )
        .map_err(invalid)?;
        Ok(partial.finish_cosine(norm).into_inner())
    }
}

#[derive(Debug)]
struct StaticFilter;
impl labeled::QueryLabelProvider<u32> for StaticFilter {
    fn is_match(&self, id: u32) -> bool {
        id != 0
    }
}

struct Strategy<'a>(&'a PqIndex);
struct Accessor<'a> {
    provider: &'a Provider,
    pq: &'a PqIndex,
    query: &'a [f32],
    norm_squared: f64,
    norm: f32,
    lut: rowmajor::Owned<DotAndNorm>,
}
impl diskann::provider::HasId for Accessor<'_> {
    type Id = u32;
}
impl glue::SearchAccessor for Accessor<'_> {
    async fn starting_points(&self) -> ANNResult<Vec<u32>> {
        Ok(vec![0])
    }
    async fn start_point_distances<F>(&mut self, mut visit: F) -> ANNResult<()>
    where
        F: FnMut(u32, f32) + Send,
    {
        visit(
            0,
            self.provider
                .query_distance(0, self.query, self.norm_squared)?,
        );
        Ok(())
    }
    async fn expand_beam<I, P, F>(
        &mut self,
        ids: I,
        mut predicate: P,
        mut visit: F,
    ) -> ANNResult<()>
    where
        I: Iterator<Item = u32> + Send,
        P: glue::HybridPredicate<u32> + Send + Sync,
        F: FnMut(u32, f32) + Send,
    {
        for id in ids {
            for neighbor in self.provider.neighbors(id)? {
                if predicate.eval_mut(&neighbor) {
                    let distance = if neighbor == 0 {
                        self.provider
                            .query_distance(0, self.query, self.norm_squared)?
                    } else {
                        self.pq.distance(neighbor, &self.lut, self.norm)?
                    };
                    visit(neighbor, distance);
                }
            }
        }
        Ok(())
    }
}
impl<'a> glue::SearchStrategy<'a, Provider, &'a [f32]> for Strategy<'a> {
    type SearchAccessorError = Error;
    type SearchAccessor = Accessor<'a>;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "FP16 finite vectors have finite FP32 norms"
    )]
    fn search_accessor(
        &'a self,
        provider: &'a Provider,
        _: &'a DefaultContext,
        query: &'a [f32],
    ) -> Result<Accessor<'a>> {
        let norm_squared = query.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
        let mut lut =
            rowmajor::Owned::from_element(self.0.table.nchunks(), 256, DotAndNorm::default());
        self.0
            .table
            .process_into::<distances::Cosine, DotAndNorm>(query, lut.as_view_mut());
        Ok(Accessor {
            provider,
            pq: self.0,
            query,
            norm_squared,
            norm: norm_squared.sqrt() as f32,
            lut,
        })
    }
}

impl VectorIndex {
    /// PQ cosine graph traversal followed by full FP16-vector reranking.
    /// Only for immutable benchmark snapshots verified by `PqIndex::load`.
    ///
    /// # Errors
    /// Rejects invalid query/effort, non-read-only indexes, and mismatched dimensions.
    pub fn search_with_pq(
        &self,
        pq: &PqIndex,
        query: &[f32],
        limit: usize,
        list_size: usize,
        rerank: usize,
    ) -> Result<Vec<Hit>> {
        self.validate(query)?;
        if !self.read_only
            || self.distance != Distance::Cosine
            || self.precision != VectorPrecision::Fp16
            || self.dimension != pq.table.dim()
            || rerank < limit
            || list_size < rerank
            || list_size == 0
        {
            return Err(invalid("invalid index or search parameters"));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let index = self.index.lock().map_err(poisoned)?;
        let strategy = labeled::Filtered::new(Strategy(pq), &StaticFilter);
        let search = InlineFilterSearch::new(
            Knn::new_default(list_size).map_err(graph_error)?,
            Some(diskann::graph::search::AdaptiveL::new(32, 8.0).map_err(graph_error)?),
        );
        let mut neighbors = Vec::<diskann::neighbor::Neighbor<u32>>::new();
        futures::executor::block_on(index.search_with(
            search,
            &strategy,
            glue::CopyIds,
            &DefaultContext,
            query,
            &mut neighbors,
        ))
        .map_err(graph_error)?;
        let norm = query.iter().map(|&x| f64::from(x).powi(2)).sum();
        let mut candidates = neighbors
            .iter()
            .take(rerank)
            .map(|n| {
                Ok((
                    *n.id(),
                    index.data_provider.query_distance(*n.id(), query, norm)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        candidates.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let cutoff = candidates.get(limit.saturating_sub(1)).map(|r| r.1);
        let mut hits = Vec::new();
        for (id, distance) in candidates
            .into_iter()
            .take_while(|r| cutoff.is_none_or(|c| r.1 <= c))
        {
            if let Some(document_id) = index.data_provider.document(id)? {
                hits.push(Hit {
                    document_id,
                    score: self.distance.score(distance),
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.document_id.cmp(&b.document_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests;
