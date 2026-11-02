//! `DiskANN`'s provider and strategy contracts. SQLite owns all durable state.

#![allow(
    clippy::unused_async_trait_impl,
    reason = "DiskANN's async provider contract wraps synchronous SQLite operations"
)]

use std::sync::{Mutex, MutexGuard};

use diskann::{
    ANNResult,
    graph::{AdjacencyList, glue, workingset},
    provider::{self, DefaultContext, NeighborAccessor, NeighborAccessorMut},
};
use diskann_vector::DistanceFunction;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Distance, Error, Result, encode_vector, poisoned};

pub(super) struct Provider {
    connection: Mutex<Connection>,
    dimension: usize,
    distance: Distance,
}

impl Provider {
    pub(super) fn new(connection: Connection, dimension: usize, distance: Distance) -> Self {
        Self {
            connection: Mutex::new(connection),
            dimension,
            distance,
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
        let bytes: Vec<u8> = self
            .connection()?
            .prepare_cached("SELECT vector FROM nodes WHERE id = ?1")?
            .query_row([id], |row| row.get(0))?;
        if bytes.len() != self.dimension * 4 {
            return Err(Error::Invalid(format!(
                "invalid vector length for node {id}"
            )));
        }
        let vector: Box<[f32]> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(Error::Invalid(format!("non-finite vector for node {id}")));
        }
        Ok(vector)
    }

    fn neighbors(&self, id: u32) -> Result<Vec<u32>> {
        let bytes: Vec<u8> = self
            .connection()?
            .prepare_cached("SELECT neighbors FROM nodes WHERE id = ?1")?
            .query_row([id], |row| row.get(0))?;
        if !bytes.len().is_multiple_of(4) {
            return Err(Error::Invalid(format!(
                "invalid adjacency list for node {id}"
            )));
        }
        Ok(bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect())
    }
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
            params![id, encode_vector(element)],
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
        let mut dot = 0.0_f64;
        let mut left_norm = 0.0_f64;
        let mut right_norm = 0.0_f64;
        let mut squared = 0.0_f64;
        for (&x, &y) in left.iter().zip(right) {
            let (x, y) = (f64::from(x), f64::from(y));
            dot += x * y;
            left_norm += x * x;
            right_norm += y * y;
            squared += (x - y) * (x - y);
        }
        let value = match self {
            Self::Cosine => {
                let norm = (left_norm * right_norm).sqrt();
                if norm == 0.0 {
                    1.0
                } else {
                    1.0 - (dot / norm).clamp(-1.0, 1.0)
                }
            }
            Self::DotProduct => -dot,
            Self::Euclidean => squared,
        };
        value.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
    }
}

pub(super) struct Accessor<'a> {
    provider: &'a Provider,
    query: &'a [f32],
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
                .distance
                .evaluate_similarity(self.query, &self.provider.vector(0)?),
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
                            .distance
                            .evaluate_similarity(self.query, &self.provider.vector(neighbor)?),
                    );
                }
            }
        }
        Ok(())
    }
}

type WorkingSet = workingset::Map<u32, Box<[f32]>, workingset::map::Ref<[f32]>>;

pub(super) struct Prune<'a> {
    provider: &'a Provider,
    set: WorkingSet,
}

impl provider::HasId for Prune<'_> {
    type Id = u32;
}

impl glue::PruneAccessor for Prune<'_> {
    type ElementRef<'a> = &'a [f32];
    type View<'a>
        = workingset::map::View<'a, u32, Box<[f32]>, workingset::map::Ref<[f32]>>
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
        let view = self
            .set
            .fill(ids, |id| self.provider.vector(id).map(Some))?;
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
        Ok(Accessor { provider, query })
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
        Ok(Accessor { provider, query })
    }

    fn prune_strategy(&self) -> Self {
        *self
    }
}
