//! Rerank recalled code within the positions assigned to code by fusion.
mod paths;
mod query;
mod support;

use std::{
    cmp::{Ordering, Reverse},
    collections::HashMap,
};

use super::{
    pipeline::{ResolvedSearchRoute, SearchHit},
    stage_trace::StageTraceCollector,
};
use crate::{
    api::context::{
        options::QueryFilter,
        result::{ContentRange, SearchRankingTrace},
    },
    domain::{ContentKind, EntityMetadata, FileId, Range},
};
use paths::Roles;
use query::QueryFeatures;

const FILE_DECAY: f64 = 0.50;

/// Internal policy values also provide a fixed-pool ablation seam for tests.
#[derive(Clone, Copy)]
struct Policy {
    name: &'static str,
    path_weight: f64,
    weak_names: bool,
    definition_intent: bool,
    support_weight: f64,
    bonus_cap: f64,
}

impl Policy {
    const CURRENT: Self = Self {
        name: "code-rules",
        path_weight: 0.35,
        weak_names: true,
        definition_intent: true,
        support_weight: 0.15,
        bonus_cap: 1.25,
    };

    #[cfg(test)]
    const BASELINE: Self = Self {
        name: "code-rules-baseline",
        path_weight: 0.0,
        weak_names: false,
        definition_intent: false,
        support_weight: 0.0,
        bonus_cap: 0.50,
    };
}

struct Scored {
    hit: SearchHit,
    base: f64,
    static_score: f64,
    symbol_match: &'static str,
    symbol_bonus: f64,
    definition_bonus: f64,
    path_bonus: f64,
    support_bonus: f64,
    support_count: u32,
    roles: Roles,
    overrides: Roles,
}

/// Rank without widening recall, inspecting source files, or loading a model.
pub(super) fn rank(
    hits: Vec<SearchHit>,
    routes: &[ResolvedSearchRoute],
    filter: &QueryFilter,
    maximum_rrf: f64,
    limit: usize,
    observation: Option<&mut StageTraceCollector>,
) -> Vec<SearchHit> {
    rank_with_policy(
        hits,
        routes,
        filter,
        maximum_rrf,
        limit,
        Policy::CURRENT,
        observation,
    )
}

fn rank_with_policy(
    mut hits: Vec<SearchHit>,
    routes: &[ResolvedSearchRoute],
    filter: &QueryFilter,
    maximum_rrf: f64,
    limit: usize,
    policy: Policy,
    observation: Option<&mut StageTraceCollector>,
) -> Vec<SearchHit> {
    let result_count = limit.min(hits.len());
    let is_code = |hit: &SearchHit| hit.entity.content().kind() == ContentKind::Code;
    // Stage diagnostics also report static ranks below the result cutoff.
    let slot_count = if observation.is_some() {
        hits.len()
    } else {
        result_count
    };
    let code_ranks: Vec<_> = hits
        .iter()
        .take(slot_count)
        .enumerate()
        .filter_map(|(index, hit)| is_code(hit).then_some(index + 1))
        .collect();

    // Only the visible positions are fixed. Any recalled code hit can fill them.
    let mut ranked_code = Vec::new();
    let visible = code_ranks.partition_point(|rank| *rank <= result_count);
    if visible > 0 {
        let code = hits.extract_if(.., |hit| is_code(hit)).collect();
        let (query, scored) = score_code(code, routes, filter, maximum_rrf, policy);
        if let Some(observation) = observation {
            let mut order = scored.iter().collect::<Vec<_>>();
            order.sort_by(|a, b| compare(a, b, a.static_score, b.static_score));
            for (item, &rank) in order.into_iter().zip(&code_ranks) {
                observation.static_rank(&item.hit, rank);
            }
        }
        ranked_code = select(scored, &query, &code_ranks[..visible], policy);
    }
    let mut ranked_code = ranked_code.into_iter().peekable();
    let mut documents = hits.into_iter();
    (1..=result_count)
        .map_while(|rank| {
            let mut hit = ranked_code
                .next_if(|hit| hit.rank == rank)
                .or_else(|| documents.next())?;
            hit.rank = rank;
            if let Some(trace) = &mut hit.trace {
                trace.final_selection.returned_by_limit = true;
                trace.final_selection.cutoff_rank = limit;
            }
            Some(hit)
        })
        .collect()
}

fn score_code(
    hits: Vec<SearchHit>,
    routes: &[ResolvedSearchRoute],
    filter: &QueryFilter,
    maximum_rrf: f64,
    policy: Policy,
) -> (QueryFeatures, Vec<Scored>) {
    let mut queries = routes.iter().map(|r| r.query.as_str()).collect::<Vec<_>>();
    queries.sort_unstable();
    queries.dedup();
    let query = QueryFeatures::from_queries(&queries);
    let mut overrides = Roles::overrides(&query, filter);
    let roles = hits
        .iter()
        .map(|hit| Roles::from_path(&hit.file.relative_path().display().to_string()))
        .collect::<Vec<_>>();
    // A common dependency prior carries no information within this search scope.
    overrides.dependency |= roles.iter().all(|r| r.dependency);
    let mut scored = hits
        .into_iter()
        .zip(roles)
        .map(|(hit, roles)| {
            let (symbol_match, symbol_bonus, definition_bonus) = match hit.entity.metadata() {
                Some(EntityMetadata::Code(code)) => query.symbol_bonus_with(code, policy),
                _ => ("none", 0.0, 0.0),
            };
            let base = hit.score / maximum_rrf;
            let mut path_bonus = policy.path_weight
                * query.path_affinity(&hit.file.relative_path().display().to_string());
            // Weak lexical coverage is one signal even when repeated in the path.
            if symbol_match == "tokens" {
                path_bonus = (path_bonus - symbol_bonus).max(0.0);
            }
            let static_score = (base
                + (symbol_bonus + definition_bonus + path_bonus).min(policy.bonus_cap))
                * roles.factor(overrides);
            Scored {
                hit,
                base,
                static_score,
                symbol_match,
                symbol_bonus,
                definition_bonus,
                path_bonus,
                support_bonus: 0.0,
                support_count: 0,
                roles,
                overrides,
            }
        })
        .collect::<Vec<_>>();
    if policy.support_weight > 0.0 {
        support::apply(&mut scored, policy.support_weight);
    }
    (query, scored)
}

fn compare(a: &Scored, b: &Scored, a_score: f64, b_score: f64) -> Ordering {
    b_score
        .total_cmp(&a_score)
        .then_with(|| a.hit.rank.cmp(&b.hit.rank))
        .then_with(|| a.hit.entity.id().as_str().cmp(b.hit.entity.id().as_str()))
}

fn take_best(
    pool: &mut Vec<Scored>,
    counts: &mut HashMap<FileId, u32>,
    single_file: bool,
) -> Option<Scored> {
    let score = |item: &Scored| {
        let count = counts.get(&item.hit.file.id()).copied().unwrap_or(0);
        item.static_score / diversity_divisor(single_file, count)
    };
    let (best, _) = pool
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| compare(a, b, score(a), score(b)))?;
    let best = pool.swap_remove(best);
    *counts.entry(best.hit.file.id()).or_default() += 1;
    Some(best)
}

fn diversity_divisor(single_file: bool, count: u32) -> f64 {
    if single_file {
        1.0
    } else {
        1.0 + FILE_DECAY * f64::from(count)
    }
}

fn nested_symbols(left: &Scored, right: &Scored) -> bool {
    left.hit.file.id() == right.hit.file.id()
        && matches!(left.hit.entity.metadata(), Some(EntityMetadata::Code(_)))
        && matches!(right.hit.entity.metadata(), Some(EntityMetadata::Code(_)))
        && ContentRange::from(left.hit.entity.source_range())
            .is_nested_with(&right.hit.entity.source_range().into())
}

/// Resolve conflicts only between candidates that made the original cutoff.
fn retain_representatives(shortlist: &mut Vec<Scored>, query: &QueryFeatures) {
    let mut priority: Vec<_> = shortlist
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let Some(EntityMetadata::Code(code)) = item.hit.entity.metadata() else {
                return None;
            };
            let range = ContentRange::from(item.hit.entity.source_range());
            let (start, end) = range.byte_interval()?;
            let direct = query.directly_names_symbol(code);
            Some((index, direct, end - start))
        })
        .collect();
    // Stable ties keep the original selection order for identical ranges.
    priority.sort_by_key(|&(_, direct, width)| (Reverse(direct), Reverse(width)));
    let mut winners = Vec::<(usize, bool)>::new();
    let mut covered_by = vec![None; shortlist.len()];
    for (index, direct, _) in priority {
        if let Some(&winner) = winners
            .iter()
            .find(|&&(other, _)| nested_symbols(&shortlist[index], &shortlist[other]))
        {
            covered_by[index] = Some(winner);
        } else {
            winners.push((index, direct));
        }
    }

    // An exact symbol keeps its own preview. Otherwise use the earliest covered
    // hit unless the representative already has a higher-ranked fragment.
    for (index, &winner) in covered_by.iter().enumerate() {
        let Some((parent, false)) = winner else {
            continue;
        };
        let hit = &shortlist[parent].hit;
        let has_fragment = hit
            .evidence
            .first()
            .is_some_and(|e| matches!(e.fragment.range, Range::Byte(_)));
        if hit.nested_excerpt.is_none()
            && (index < parent || !has_fragment)
            && let Ok((source @ ContentRange::Text { .. }, excerpt)) =
                super::context::hit_source_ranges(&shortlist[index].hit)
        {
            shortlist[parent].hit.nested_excerpt = Some(excerpt.unwrap_or(source));
        }
    }
    let mut covered_by = covered_by.into_iter();
    shortlist.retain(|_| {
        covered_by
            .next()
            .expect("one coverage entry per hit")
            .is_none()
    });
}

fn select(
    mut scored: Vec<Scored>,
    query: &QueryFeatures,
    code_ranks: &[usize],
    policy: Policy,
) -> Vec<SearchHit> {
    let single_file = scored
        .iter()
        .all(|s| s.hit.file.id() == scored[0].hit.file.id());
    let mut selected_per_file = HashMap::<FileId, u32>::new();
    let mut selected: Vec<_> = code_ranks
        .iter()
        .map_while(|_| take_best(&mut scored, &mut selected_per_file, single_file))
        .collect();
    let original_len = selected.len();
    retain_representatives(&mut selected, query);
    if selected.len() < original_len {
        selected_per_file.clear();
        for item in &selected {
            *selected_per_file.entry(item.hit.file.id()).or_default() += 1;
        }
        scored.retain(|other| !selected.iter().any(|item| nested_symbols(item, other)));
        while selected.len() < code_ranks.len() {
            let Some(item) = take_best(&mut scored, &mut selected_per_file, single_file) else {
                break;
            };
            scored.retain(|other| !nested_symbols(&item, other));
            selected.push(item);
        }

        // The removed symbols influenced the original per-file penalties.
        // Rank the surviving and backfilled hits with only their own counts.
        let mut finalists = selected;
        selected_per_file.clear();
        selected =
            std::iter::from_fn(|| take_best(&mut finalists, &mut selected_per_file, single_file))
                .collect();
    }

    selected_per_file.clear();
    let mut output = Vec::with_capacity(selected.len());
    for (mut item, &rank) in selected.into_iter().zip(code_ranks) {
        let count = selected_per_file.entry(item.hit.file.id()).or_default();
        let factor = 1.0 / diversity_divisor(single_file, *count);
        item.hit.score = item.static_score * factor;
        item.hit.rank = rank;
        if let Some(trace) = item.hit.trace.as_mut() {
            trace.ranking = Some(SearchRankingTrace {
                policy: policy.name.to_owned(),
                query_features: query.describe(),
                normalized_base: item.base,
                symbol_match: item.symbol_match.to_owned(),
                symbol_bonus: item.symbol_bonus,
                definition_bonus: item.definition_bonus,
                path_bonus: item.path_bonus,
                file_support_bonus: item.support_bonus,
                file_support_count: item.support_count,
                path_roles: item.roles.names(),
                path_overrides: item.overrides.names(),
                path_factor: item.roles.factor(item.overrides),
                static_score: item.static_score,
                file_count_at_selection: *count,
                diversity_factor: factor,
                score: item.hit.score,
                rank: item.hit.rank,
            });
        }
        *count = count.saturating_add(1);
        output.push(item.hit);
    }
    output
}

#[cfg(test)]
mod replay;
#[cfg(test)]
mod tests;
