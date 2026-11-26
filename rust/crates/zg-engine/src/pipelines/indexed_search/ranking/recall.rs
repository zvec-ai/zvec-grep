use std::collections::{BTreeMap, HashMap};

use super::{SourceLookup, matches_source_lookup};
use crate::{
    domain::EntityMetadata,
    pipelines::indexed_search::pipeline::{SearchHit, reciprocal_rank},
};

#[derive(Default)]
pub(super) struct SupplementalRanks(HashMap<String, Vec<usize>>);

fn supplemental(hit: &SearchHit) -> bool {
    matches!(hit.entity.metadata(), Some(EntityMetadata::Code(code)) if code.is_supplemental && code.symbol_name.is_none())
}

impl SupplementalRanks {
    pub(super) fn for_query(hits: &[SearchHit], lookup: &SourceLookup, query: &str) -> Self {
        let mut ranks = Self::default();
        if !matches!(lookup, SourceLookup::General) {
            return ranks;
        }
        for hit in hits.iter().filter(|hit| supplemental(hit)) {
            // Recall already retains just the best fragment rank per entity per route.
            for route in hit.recall() {
                if let Some(rank) = route.rank {
                    ranks
                        .0
                        .entry(route.route_id.clone())
                        .or_default()
                        .push(rank);
                }
            }
        }
        // A pasted snippet in either a symbol or a supplemental statement is
        // also a direct lookup. Check only the leading bounded evidence.
        if !ranks.0.is_empty()
            && hits
                .iter()
                .any(|hit| matches_source_lookup(hit, None, lookup, query))
        {
            return Self::default();
        }
        for route in ranks.0.values_mut() {
            route.sort_unstable();
        }
        ranks
    }

    pub(super) fn adjust(&self, hit: &SearchHit) -> (f64, BTreeMap<String, usize>) {
        let mut explained = BTreeMap::new();
        if self.0.is_empty() || supplemental(hit) {
            return (hit.score, explained);
        }
        let mut changed = false;
        let mut score = 0.0;
        for route in hit.recall() {
            if let Some(rank) = route.rank {
                let preceding = self
                    .0
                    .get(&route.route_id)
                    .map_or(0, |ranks| ranks.partition_point(|&prior| prior < rank));
                // Adaptive recall can record several entities' best rank at the
                // same position on different attempts, so bound the effective rank.
                let effective = rank.saturating_sub(preceding).max(1);
                if effective != rank {
                    changed = true;
                    if hit.trace.is_some() {
                        explained.insert(route.route_id.clone(), effective);
                    }
                }
                score += reciprocal_rank(effective);
            }
        }
        (if changed { score } else { hit.score }, explained)
    }
}
