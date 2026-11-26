use std::collections::{BTreeMap, BTreeSet};

use tokio_util::sync::CancellationToken;

use crate::{EngineResult, api::context::result::GraphRankingTrace, domain::EntityId};

use super::{SearchHit, check_cancelled};

#[derive(Default)]
pub(in super::super) struct Relevance {
    neighbors: BTreeMap<EntityId, BTreeSet<EntityId>>,
    seeds: BTreeMap<EntityId, f64>,
    candidates: BTreeSet<EntityId>,
}

impl Relevance {
    pub(super) fn observe(&mut self, source: &EntityId, target: &EntityId) {
        if source == target {
            return;
        }
        self.neighbors
            .entry(source.clone())
            .or_default()
            .insert(target.clone());
        self.neighbors
            .entry(target.clone())
            .or_default()
            .insert(source.clone());
    }

    pub(super) fn initialize(
        &mut self,
        hits: &[SearchHit],
        seeds: &BTreeSet<EntityId>,
        added: &[EntityId],
    ) {
        self.seeds = hits
            .iter()
            .filter(|hit| seeds.contains(hit.entity.id()))
            .map(|hit| (hit.entity.id().clone(), hit.score))
            .collect();
        self.candidates = hits
            .iter()
            .map(|hit| hit.entity.id().clone())
            .chain(added.iter().cloned())
            .collect();
    }

    pub(in super::super) fn rerank(
        self,
        hits: &mut Vec<SearchHit>,
        limit: usize,
        signal: Option<&CancellationToken>,
    ) -> EngineResult<()> {
        let mut mass = self.seeds;
        let mut scores = BTreeMap::<EntityId, f64>::new();
        // Propagation is linear, so all seeds can share a single three-step walk.
        for decay in [1.0, 0.5, 0.25] {
            check_cancelled(signal)?;
            let mut next = BTreeMap::<EntityId, f64>::new();
            for (node, value) in mass {
                check_cancelled(signal)?;
                if let Some(neighbors) = self.neighbors.get(&node) {
                    let degree =
                        f64::from(u32::try_from(neighbors.len()).expect("bounded adjacency"));
                    for target in neighbors {
                        *next.entry(target.clone()).or_default() += value / degree;
                    }
                }
            }
            for (node, value) in &next {
                if self.candidates.contains(node) {
                    *scores.entry(node.clone()).or_default() += decay * value;
                }
            }
            mass = next;
        }
        let graph_normalizer: f64 = scores.values().sum();
        let ordinary_normalizer: f64 = (1..=hits.len()).map(reciprocal_rank).sum();
        for hit in hits.iter_mut() {
            let ordinary_rank = hit.rank;
            let ordinary_score = reciprocal_rank(ordinary_rank) / ordinary_normalizer;
            let graph_raw_score = scores.get(hit.entity.id()).copied().unwrap_or(0.0);
            let graph_score = if graph_normalizer > 0.0 {
                graph_raw_score / graph_normalizer
            } else {
                0.0
            };
            hit.score = ordinary_score.midpoint(graph_score);
            if let Some(trace) = &mut hit.trace {
                trace.graph_ranking = Some(GraphRankingTrace {
                    ordinary_rank,
                    ordinary_score,
                    graph_raw_score,
                    graph_normalizer,
                    graph_score,
                    score: hit.score,
                    rank: 0,
                });
            }
        }
        check_cancelled(signal)?;
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.rank.cmp(&right.rank))
                .then_with(|| left.entity.id().as_str().cmp(right.entity.id().as_str()))
        });
        hits.truncate(limit);
        for (index, hit) in hits.iter_mut().enumerate() {
            hit.rank = index + 1;
            if let Some(trace) = &mut hit.trace {
                trace
                    .graph_ranking
                    .as_mut()
                    .expect("graph score recorded")
                    .rank = hit.rank;
                trace.final_selection.returned_by_limit = true;
                trace.final_selection.cutoff_rank = limit;
            }
        }
        Ok(())
    }
}

fn reciprocal_rank(rank: usize) -> f64 {
    1.0 / (60.0 + f64::from(u32::try_from(rank).expect("bounded candidate rank")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EngineError,
        api::context::result::{MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace},
        domain::{
            Content, Entity, EntityFragment, FileId, FileIndexStatus, FileRecord, FileSnapshot,
            FragmentId, Range, SourcePath,
        },
    };

    fn hit(id: &str, rank: usize, score: f64) -> SearchHit {
        let id = EntityId::from_string(id.to_owned());
        SearchHit {
            entity: Entity::new(
                id.clone(),
                FileId::new(1),
                Range::Full,
                Content::Text("source".into()),
                None,
                vec![EntityFragment {
                    id: FragmentId::new(&id, 0),
                    range: Range::Full,
                }],
            )
            .expect("entity"),
            file: FileRecord::new(
                FileId::new(1),
                SourcePath::new("src/lib.rs").expect("path"),
                FileSnapshot {
                    size_bytes: 6,
                    modified_epoch_ms: None,
                    content_hash: Some(crate::utils::sha256_hex(b"source")),
                },
                FileIndexStatus::Indexed {
                    indexed_epoch_ms: 0,
                    entity_count: 3,
                },
            )
            .expect("file"),
            evidence: Vec::new(),
            rank,
            score,
            matched_by: MatchedBy::Fts,
            trace: Some(SearchHitTrace {
                recall: Vec::new(),
                fusion: SearchFusionTrace {
                    rank,
                    score,
                    forced: false,
                },
                ranking: None,
                graph_ranking: None,
                final_selection: SearchFinalTrace {
                    returned_by_limit: true,
                    cutoff_rank: 3,
                },
            }),
        }
    }

    #[test]
    fn multi_seed_scores_use_distinct_neighbors_and_the_retained_pool() {
        let mut hits = vec![hit("a", 1, 0.02), hit("b", 2, 0.01), hit("c", 3, 0.005)];
        let a = hits[0].entity.id().clone();
        let b = hits[1].entity.id().clone();
        let bridge = EntityId::from_string("f1".into());
        let added = EntityId::from_string("added".into());
        let mut relevance = Relevance::default();
        relevance.observe(&a, &bridge);
        relevance.observe(&a, &bridge);
        relevance.observe(&a, &a);
        relevance.observe(&b, &bridge);
        relevance.observe(&bridge, &added);
        relevance.initialize(&hits, &BTreeSet::from([a.clone(), b.clone()]), &[added]);
        relevance.rerank(&mut hits, 2, None).expect("graph rank");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entity.id(), &a);
        assert_eq!(hits[1].entity.id(), &b);
        let normalizer: f64 = (1..=3).map(reciprocal_rank).sum();
        for (i, hit) in hits.iter().enumerate() {
            let trace = hit.trace.as_ref().expect("trace");
            let graph = trace.graph_ranking.as_ref().expect("graph trace");
            assert!((graph.graph_raw_score - 0.005).abs() < 1e-15);
            assert!((graph.graph_normalizer - 0.015).abs() < 1e-15);
            assert!((graph.graph_score - 1.0 / 3.0).abs() < 1e-15);
            assert!((graph.ordinary_score - reciprocal_rank(i + 1) / normalizer).abs() < 1e-15);
            assert_eq!(graph.rank, i + 1);
            assert_eq!(trace.final_selection.cutoff_rank, 2);
        }
    }

    #[test]
    fn empty_graph_keeps_ordinary_order_and_propagation_can_be_cancelled() {
        let mut hits = vec![hit("b", 1, 0.01), hit("a", 2, 0.02)];
        let seeds = hits.iter().map(|hit| hit.entity.id().clone()).collect();
        let mut relevance = Relevance::default();
        relevance.initialize(&hits, &seeds, &[]);
        relevance.rerank(&mut hits, 2, None).expect("empty graph");
        assert_eq!(hits[0].entity.id().as_str(), "b");
        assert_eq!(hits[1].entity.id().as_str(), "a");
        assert!(hits.iter().all(|hit| {
            hit.trace
                .as_ref()
                .expect("trace")
                .graph_ranking
                .as_ref()
                .expect("graph rank")
                .graph_score
                == 0.0
        }));
        let signal = CancellationToken::new();
        signal.cancel();
        let error = Relevance::default()
            .rerank(&mut hits, 2, Some(&signal))
            .expect_err("cancelled rank");
        assert_eq!(error.code(), EngineError::CANCELLED);
    }
}
