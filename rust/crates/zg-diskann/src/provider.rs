//! `DiskANN`'s provider and strategy contracts. SQLite owns all durable state.

#![allow(
    clippy::unused_async_trait_impl,
    reason = "DiskANN's async provider contract wraps synchronous SQLite operations"
)]

use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard},
};

use diskann::{
    ANNResult,
    graph::{AdjacencyList, glue, workingset},
    provider::{self, DefaultContext, NeighborAccessor, NeighborAccessorMut},
};
use diskann_vector::DistanceFunction;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Distance, Error, Result, VectorPrecision, poisoned};

pub(super) struct Provider {
    connection: Mutex<Connection>,
    dimension: usize,
    distance: Distance,
    precision: VectorPrecision,
    last_live: Mutex<Option<(u32, bool)>>,
}

impl Provider {
    pub(super) fn new(
        connection: Connection,
        dimension: usize,
        distance: Distance,
        precision: VectorPrecision,
    ) -> Self {
        Self {
            connection: Mutex::new(connection),
            dimension,
            distance,
            precision,
            last_live: Mutex::new(None),
        }
    }

    pub(super) fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(poisoned)
    }

    pub(super) fn document(&self, id: u32) -> Result<Option<String>> {
        self.connection()?
            .prepare_cached("SELECT document_id FROM nodes WHERE id = ?1 AND live = 1")?
            .query_row([id], |row| row.get(0))
            .optional()
            .map_err(Into::into)
    }

    pub(super) fn vector(&self, id: u32) -> Result<Box<[f32]>> {
        self.with_vector(id, |vector| vector.into())
    }

    pub(super) fn matches(&self, id: u32, allowed: Option<&HashSet<String>>) -> Result<bool> {
        // The labeled traversal asks about a node immediately after measuring
        // its distance. Reuse liveness fetched with the vector, without issuing
        // another SQL lookup. Other nodes and metadata filters use the database.
        if allowed.is_none() {
            let last_live = self.last_live.lock().map_err(poisoned)?;
            if let Some((cached, live)) = *last_live
                && cached == id
            {
                return Ok(live);
            }
        }
        self.connection()?
            .prepare_cached("SELECT live, document_id FROM nodes WHERE id = ?1")?
            .query_row([id], |row| {
                if !row.get::<_, bool>(0)? {
                    return Ok(false);
                }
                match allowed {
                    None => Ok(true),
                    Some(allowed) => Ok(allowed.contains(row.get_ref(1)?.as_str()?)),
                }
            })
            .optional()
            .map(|value| value.unwrap_or(false))
            .map_err(Into::into)
    }

    pub(super) fn clear_query_cache(&self) -> Result<()> {
        *self.last_live.lock().map_err(poisoned)? = None;
        Ok(())
    }

    fn with_vector<T>(&self, id: u32, visit: impl FnOnce(&[f32]) -> T) -> Result<T> {
        self.connection()?
            .prepare_cached("SELECT vector, live FROM nodes WHERE id = ?1")?
            .query_row([id], |row| {
                let bytes = row.get_ref(0)?.as_blob()?;
                let live = row.get(1)?;
                let mut last_live = match self.last_live.lock() {
                    Ok(last_live) => last_live,
                    Err(error) => return Ok(Err(poisoned(error))),
                };
                *last_live = Some((id, live));
                drop(last_live);
                Ok(match self.precision {
                    VectorPrecision::Fp32 => with_vector_bytes(bytes, self.dimension, id, visit),
                    VectorPrecision::Fp16 => {
                        with_fp16_vector_bytes(bytes, self.dimension, id, visit)
                    }
                })
            })?
    }

    pub(super) fn query_distance(&self, id: u32, query: &[f32], query_norm: f64) -> Result<f32> {
        self.with_vector(id, |vector| {
            if self.distance == Distance::Cosine {
                cosine_distance(
                    sum_pairs(query, vector, |x, y| x * y),
                    query_norm,
                    self.norm(vector),
                )
            } else {
                self.distance.evaluate_similarity(query, vector)
            }
        })
    }

    fn norm(&self, vector: &[f32]) -> f64 {
        if self.distance == Distance::Cosine {
            sum_pairs(vector, vector, |x, y| x * y)
        } else {
            0.0
        }
    }

    pub(super) fn neighbors(&self, id: u32) -> Result<Vec<u32>> {
        self.connection()?
            .prepare_cached("SELECT neighbors FROM nodes WHERE id = ?1")?
            .query_row([id], |row| {
                let bytes = row.get_ref(0)?.as_blob()?;
                if !bytes.len().is_multiple_of(4) {
                    return Ok(Err(Error::Invalid(format!(
                        "invalid adjacency list for node {id}"
                    ))));
                }
                Ok(Ok(bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect()))
            })?
    }
}

// The SQLite row owns the bytes for the duration of the callback. Borrow aligned
// little-endian blobs directly; copy only when alignment or byte order requires it.
fn with_vector_bytes<T>(
    bytes: &[u8],
    dimension: usize,
    id: u32,
    visit: impl FnOnce(&[f32]) -> T,
) -> Result<T> {
    if bytes.len() != dimension * 4 {
        return Err(Error::Invalid(format!(
            "invalid vector length for node {id}"
        )));
    }
    let decoded;
    let vector = if cfg!(target_endian = "little") {
        if let Ok(vector) = bytemuck::try_cast_slice::<u8, f32>(bytes) {
            vector
        } else {
            decoded = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect::<Vec<_>>();
            &decoded
        }
    } else {
        decoded = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect::<Vec<_>>();
        &decoded
    };
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(Error::Invalid(format!("non-finite vector for node {id}")));
    }
    Ok(visit(vector))
}

// FP16 occupies two bytes per element on disk. Decode only the currently visited
// vector, not the corpus; search/pruning arithmetic retains its existing precision.
fn with_fp16_vector_bytes<T>(
    bytes: &[u8],
    dimension: usize,
    id: u32,
    visit: impl FnOnce(&[f32]) -> T,
) -> Result<T> {
    if bytes.len() != dimension * 2 {
        return Err(Error::Invalid(format!(
            "invalid FP16 vector length for node {id}"
        )));
    }
    let vector: Vec<f32> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| half::f16::from_le_bytes(b).to_f32())
        .collect();
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(Error::Invalid(format!(
            "non-finite FP16 vector for node {id}"
        )));
    }
    Ok(visit(&vector))
}

impl provider::DataProvider for Provider {
    type Context = DefaultContext;
    type InternalId = u32;
    type ExternalId = u32;
    type Error = Error;
    type Guard = provider::NoopGuard<u32>;

    fn to_internal_id(&self, _: &DefaultContext, id: &u32) -> Result<u32> {
        self.connection()?
            .query_row("SELECT id FROM nodes WHERE id = ?1", [id], |row| row.get(0))
            .map_err(Into::into)
    }

    fn to_external_id(&self, context: &DefaultContext, id: u32) -> Result<u32> {
        self.to_internal_id(context, &id)
    }
}

impl provider::SetElement<&[f32]> for Provider {
    type SetError = Error;

    async fn set_element(
        &self,
        _: &DefaultContext,
        id: &u32,
        element: &[f32],
    ) -> Result<Self::Guard> {
        self.connection()?.execute(
            "INSERT INTO nodes VALUES (?1, NULL, NULL, ?2, x'', 0)",
            params![id, self.precision.encode(element)],
        )?;
        Ok(provider::NoopGuard::new(*id))
    }
}

pub(super) struct Neighbors<'a>(&'a Provider);

impl provider::HasId for Neighbors<'_> {
    type Id = u32;
}

impl NeighborAccessor for Neighbors<'_> {
    async fn get_neighbors(&mut self, id: u32, output: &mut AdjacencyList<u32>) -> ANNResult<()> {
        output.clear();
        output.extend_from_slice(&self.0.neighbors(id)?);
        Ok(())
    }
}

impl NeighborAccessorMut for Neighbors<'_> {
    async fn set_neighbors(&mut self, id: u32, neighbors: &[u32]) -> ANNResult<()> {
        let bytes: Vec<u8> = neighbors.iter().flat_map(|id| id.to_le_bytes()).collect();
        let changed = self
            .0
            .connection()?
            .execute(
                "UPDATE nodes SET neighbors = ?1 WHERE id = ?2",
                params![bytes, id],
            )
            .map_err(Error::from)?;
        if changed != 1 {
            return Err(Error::Invalid(format!("missing graph node {id}")).into());
        }
        Ok(())
    }

    async fn append_vector(&mut self, id: u32, neighbors: &[u32]) -> ANNResult<()> {
        let mut merged = AdjacencyList::from_iter_untrusted(self.0.neighbors(id)?);
        merged.extend_from_slice(neighbors);
        self.set_neighbors(id, &merged).await
    }
}

impl provider::DefaultAccessor for Provider {
    type Accessor<'a> = Neighbors<'a>;
    fn default_accessor(&self) -> Self::Accessor<'_> {
        Neighbors(self)
    }
}

// Accumulate in f64 so finite f32 inputs do not overflow during distance
// evaluation. Zero vectors have cosine similarity zero, including to each other.
impl DistanceFunction<&[f32], &[f32]> for Distance {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "DiskANN distances are f32; clamp finite f64 accumulation"
    )]
    fn evaluate_similarity(&self, left: &[f32], right: &[f32]) -> f32 {
        let value = match self {
            Self::Cosine => {
                let dot = sum_pairs(left, right, |x, y| x * y);
                return cosine_distance(
                    dot,
                    sum_pairs(left, left, |x, y| x * y),
                    sum_pairs(right, right, |x, y| x * y),
                );
            }
            Self::DotProduct => -sum_pairs(left, right, |x, y| x * y),
            Self::Euclidean => sum_pairs(left, right, |x, y| (x - y) * (x - y)),
        };
        value.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "normalized cosine distance is finite and within 0..=2"
)]
fn cosine_distance(dot: f64, left_norm: f64, right_norm: f64) -> f32 {
    let norm = (left_norm * right_norm).sqrt();
    if norm == 0.0 {
        1.0
    } else {
        (1.0 - (dot / norm).clamp(-1.0, 1.0)) as f32
    }
}

// Independent f64 accumulators let the compiler vectorize the reduction while
// retaining finite-input overflow protection, including on non-SIMD platforms.
fn sum_pairs(left: &[f32], right: &[f32], operation: impl Fn(f64, f64) -> f64) -> f64 {
    let (left, left_tail) = left.as_chunks::<4>();
    let (right, right_tail) = right.as_chunks::<4>();
    let mut sums = [0.0; 4];
    for (left, right) in left.iter().zip(right) {
        for lane in 0..4 {
            sums[lane] += operation(f64::from(left[lane]), f64::from(right[lane]));
        }
    }
    let mut sum = sums.into_iter().sum::<f64>();
    for (&left, &right) in left_tail.iter().zip(right_tail) {
        sum += operation(f64::from(left), f64::from(right));
    }
    sum
}

pub(super) struct Accessor<'a> {
    provider: &'a Provider,
    query: &'a [f32],
    query_norm: f64,
}

impl provider::HasId for Accessor<'_> {
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
                .query_distance(0, self.query, self.query_norm)?,
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
                    visit(
                        neighbor,
                        self.provider
                            .query_distance(neighbor, self.query, self.query_norm)?,
                    );
                }
            }
        }
        Ok(())
    }
}

pub(super) struct PreparedVector {
    values: Box<[f32]>,
    norm: f64,
}

impl DistanceFunction<&PreparedVector, &PreparedVector> for Distance {
    fn evaluate_similarity(&self, left: &PreparedVector, right: &PreparedVector) -> f32 {
        if *self == Self::Cosine {
            cosine_distance(
                sum_pairs(&left.values, &right.values, |x, y| x * y),
                left.norm,
                right.norm,
            )
        } else {
            self.evaluate_similarity(&*left.values, &*right.values)
        }
    }
}

type WorkingSet = workingset::Map<u32, Box<PreparedVector>, workingset::map::Ref<PreparedVector>>;

pub(super) struct Prune<'a> {
    provider: &'a Provider,
    set: WorkingSet,
}

impl provider::HasId for Prune<'_> {
    type Id = u32;
}

impl glue::PruneAccessor for Prune<'_> {
    type ElementRef<'a> = &'a PreparedVector;
    type View<'a>
        = workingset::map::View<'a, u32, Box<PreparedVector>, workingset::map::Ref<PreparedVector>>
    where
        Self: 'a;
    type Distance<'a>
        = Distance
    where
        Self: 'a;
    type Neighbors<'a>
        = Neighbors<'a>
    where
        Self: 'a;

    async fn fill<I>(&mut self, ids: I) -> ANNResult<(Self::View<'_>, Distance)>
    where
        I: ExactSizeIterator<Item = u32> + Clone + Send + Sync,
    {
        let view = self.set.fill(ids, |id| {
            self.provider.vector(id).map(|values| {
                // Pruning compares each candidate repeatedly. Keep only this
                // bounded working set's norms, never a corpus-wide vector cache.
                Some(Box::new(PreparedVector {
                    norm: self.provider.norm(&values),
                    values,
                }))
            })
        })?;
        Ok((view, self.provider.distance))
    }

    fn neighbors(&mut self) -> Neighbors<'_> {
        Neighbors(self.provider)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Strategy;

impl<'a> glue::SearchStrategy<'a, Provider, &'a [f32]> for Strategy {
    type SearchAccessorError = Error;
    type SearchAccessor = Accessor<'a>;

    fn search_accessor(
        &'a self,
        provider: &'a Provider,
        _: &'a DefaultContext,
        query: &'a [f32],
    ) -> Result<Accessor<'a>> {
        Ok(Accessor {
            provider,
            query,
            query_norm: provider.norm(query),
        })
    }
}

impl glue::PruneStrategy<Provider> for Strategy {
    type PruneAccessor<'a> = Prune<'a>;
    type PruneAccessorError = Error;

    fn prune_accessor<'a>(
        &'a self,
        provider: &'a Provider,
        _: &'a DefaultContext,
        capacity: usize,
    ) -> Result<Prune<'a>> {
        Ok(Prune {
            provider,
            set: workingset::map::Builder::new(workingset::map::Capacity::Default).build(capacity),
        })
    }
}

impl<'a> glue::InsertStrategy<'a, Provider, &'a [f32]> for Strategy {
    type SearchAccessor = Accessor<'a>;
    type SearchAccessorError = Error;
    type PruneStrategy = Self;

    fn insert_search_accessor(
        &'a self,
        provider: &'a Provider,
        _: &'a DefaultContext,
        query: &'a [f32],
    ) -> Result<Accessor<'a>> {
        Ok(Accessor {
            provider,
            query,
            query_norm: provider.norm(query),
        })
    }

    fn prune_strategy(&self) -> Self {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fp16_decoder_handles_unaligned_bytes_and_rejects_corruption() {
        let bytes = [0, 0x66, 0x2e, 0, 0xbc];
        let decoded =
            with_fp16_vector_bytes(&bytes[1..], 2, 1, <[f32]>::to_vec).expect("unaligned binary16");
        assert_eq!(decoded[0].to_bits(), 0.099_975_586_f32.to_bits());
        assert_eq!(decoded[1].to_bits(), (-1.0_f32).to_bits());
        assert!(with_fp16_vector_bytes(&bytes[1..4], 2, 1, |_| ()).is_err());
        for bits in [0x7c00_u16, 0xfc00, 0x7e00] {
            assert!(with_fp16_vector_bytes(&bits.to_le_bytes(), 1, 1, |_| ()).is_err());
        }
    }

    #[test]
    fn stored_vectors_decode_independently_of_alignment_and_reject_corruption() {
        let expected = [0.25_f32, -3.5, f32::MAX];
        let encoded = super::super::encode_vector(&expected);
        let mut unaligned = vec![0_u8; encoded.len() + 1];
        unaligned[1..].copy_from_slice(&encoded);
        for bytes in [&encoded[..], &unaligned[1..]] {
            let decoded = with_vector_bytes(bytes, 3, 1, <[f32]>::to_vec).expect("valid vector");
            assert_eq!(decoded, expected);
        }
        assert!(with_vector_bytes(&encoded[..11], 3, 1, |_| ()).is_err());
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(with_vector_bytes(&value.to_le_bytes(), 1, 1, |_| ()).is_err());
        }
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "these reductions have exact representable results or clamp to exact endpoints"
    )]
    fn distance_reductions_handle_tail_elements_zero_and_extreme_vectors() {
        for length in [1, 3, 4, 7, 768] {
            let a = vec![f32::MAX; length];
            let b = vec![-f32::MAX; length];
            assert_eq!(
                Distance::Cosine.evaluate_similarity(a.as_slice(), a.as_slice()),
                0.0
            );
            assert_eq!(
                Distance::Cosine.evaluate_similarity(a.as_slice(), b.as_slice()),
                2.0
            );
            assert_eq!(
                Distance::DotProduct.evaluate_similarity(a.as_slice(), b.as_slice()),
                f32::MAX
            );
            assert_eq!(
                Distance::Euclidean.evaluate_similarity(a.as_slice(), b.as_slice()),
                f32::MAX
            );
            assert_eq!(
                Distance::Cosine.evaluate_similarity(vec![0.0; length].as_slice(), a.as_slice()),
                1.0
            );
        }
        let left = [1.0, -2.0, 3.0, -4.0, 5.0, -6.0, 7.0];
        let right = [7.0, -6.0, 5.0, -4.0, 3.0, -2.0, 1.0];
        assert_eq!(
            Distance::DotProduct.evaluate_similarity(left.as_slice(), right.as_slice()),
            -84.0
        );
        assert_eq!(
            Distance::Euclidean.evaluate_similarity(left.as_slice(), right.as_slice()),
            112.0
        );
        assert!(
            (Distance::Cosine.evaluate_similarity(left.as_slice(), right.as_slice()) - 0.4).abs()
                < 1e-6
        );
    }
}
