use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use crate::{
    api::context::result::{GraphBudgetTrace, GraphRetainedPathTrace},
    domain::{FileId, SymbolType},
};

use super::{
    ALL_EDGES, CancellationToken, ENDPOINT_BATCH, Endpoint, EngineResult, EntityId, Expansion,
    GraphCallTrace, GraphEdgeKind, GraphLayerTrace, GraphRejectedTrace, GraphRejection,
    GraphWorkTrace, MAX_HOPS, Parent, ReadView, SearchHit, StorageSearchFilter, check_cancelled,
    edge_trace, hydrate, micros, neighbor, seed_frontier, witness,
};
use crate::domain::FileEdge;

const HOP_LIMITS: [usize; MAX_HOPS] = [600, 1050, 1500];
const CANDIDATE_LIMIT: usize = 200;
const PATH_LIMIT: usize = 20;

struct NodeFacts {
    file: FileId,
    terms: HashSet<String>,
    exact: bool,
    enclosing_type: bool,
}

struct FileFacts {
    path: PathBuf,
    seed: bool,
    test: bool,
    matches: usize,
}

struct Priority {
    nodes: HashMap<String, NodeFacts>,
    files: HashMap<FileId, FileFacts>,
    calls: HashMap<String, i64>,
    ordinary: HashSet<String>,
}

struct Observed {
    id: i64,
    target: EntityId,
    edge: GraphCallTrace,
}

struct PathGroup<'a> {
    anchor: &'a EntityId,
    seed: &'a EntityId,
    steps: Vec<&'a Observed>,
}

impl Priority {
    fn new(
        view: &ReadView<'_>,
        query: &str,
        hits: &[SearchHit],
        seeds: &BTreeSet<EntityId>,
        signal: Option<&CancellationToken>,
        work: &mut GraphWorkTrace,
    ) -> EngineResult<Self> {
        let terms = tokens(query);
        let exact: HashSet<_> = query
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .collect();
        let mentions_tests = ["test", "testing", "fixture", "spec"]
            .iter()
            .any(|term| terms.contains(*term));
        let seed_files: HashSet<_> = hits
            .iter()
            .filter(|hit| seeds.contains(hit.entity.id()))
            .map(|hit| hit.file.id())
            .collect();
        let files = Self::file_facts(view, &terms, &seed_files, mentions_tests)?;
        let ordinary: HashSet<_> = hits
            .iter()
            .map(|hit| hit.entity.id().as_str().to_owned())
            .collect();
        let projection = view.explore_symbols(signal)?;
        work.symbol_rows_read = projection.len();
        let mut nodes = HashMap::with_capacity(projection.len());
        let mut call_groups = BTreeMap::<_, Vec<_>>::new();
        for (id, file, code) in projection {
            check_cancelled(signal)?;
            let Some(file_facts) = files.get(&file) else {
                continue;
            };
            let leaf = code
                .as_ref()
                .and_then(|code| code.symbol_name.as_deref())
                .unwrap_or("");
            let mut matches = tokens(leaf);
            if let Some(scope) = code.as_ref().and_then(|code| code.scope.as_deref()) {
                matches.extend(tokens(scope));
            }
            matches.retain(|token| terms.contains(token));
            if !ordinary.contains(&id) && (!matches.is_empty() || file_facts.matches > 0) {
                call_groups
                    .entry((
                        file_facts.test,
                        Reverse(matches.len()),
                        Reverse(file_facts.matches),
                        !file_facts.seed,
                    ))
                    .or_default()
                    .push(id.clone());
            }
            let scoped_leaf = leaf.rsplit('.').next().unwrap_or(leaf);
            nodes.insert(
                id,
                NodeFacts {
                    file,
                    terms: matches,
                    exact: exact.contains(scoped_leaf) && !tokens(scoped_leaf).is_empty(),
                    enclosing_type: code.as_ref().is_some_and(|code| {
                        matches!(
                            code.symbol_type,
                            Some(SymbolType::Class | SymbolType::Interface | SymbolType::Enum)
                        )
                    }),
                },
            );
        }
        for (file, facts) in &files {
            let id = format!("f{}", file.get());
            if facts.matches > 0 {
                call_groups
                    .entry((facts.test, Reverse(0), Reverse(facts.matches), !facts.seed))
                    .or_default()
                    .push(id.clone());
            }
            nodes.insert(
                id,
                NodeFacts {
                    file: *file,
                    terms: HashSet::new(),
                    exact: false,
                    enclosing_type: false,
                },
            );
        }
        let calls = call_groups
            .into_values()
            .zip(0i64..)
            .flat_map(|(ids, rank)| ids.into_iter().map(move |id| (id, rank)))
            .collect();
        Ok(Self {
            nodes,
            files,
            calls,
            ordinary,
        })
    }

    fn file_facts(
        view: &ReadView<'_>,
        terms: &HashSet<String>,
        seed_files: &HashSet<FileId>,
        mentions_tests: bool,
    ) -> EngineResult<HashMap<FileId, FileFacts>> {
        Ok(view
            .explore_file_paths()?
            .into_iter()
            .map(|(id, path)| {
                (
                    id,
                    FileFacts {
                        seed: seed_files.contains(&id),
                        test: !mentions_tests && test_path(&path),
                        matches: tokens(&path.to_string_lossy()).intersection(terms).count(),
                        path,
                    },
                )
            })
            .collect())
    }

    fn probe(&self, inheritance: bool, parent: &str, neighbor: &str) -> Option<i64> {
        if !inheritance {
            return self.calls.get(neighbor).copied();
        }
        if self.ordinary.contains(neighbor) {
            return None;
        }
        let source = &self.files[&self.nodes.get(parent)?.file];
        let target = &self.files[&self.nodes.get(neighbor)?.file];
        Some(2 * i64::from(source.path.parent() != target.path.parent()) + i64::from(!target.seed))
    }

    fn same_file(&self, left: &EntityId, right: &EntityId) -> bool {
        self.nodes
            .get(left.as_str())
            .zip(self.nodes.get(right.as_str()))
            .is_some_and(|(left, right)| left.file == right.file)
    }

    fn score(&self, id: &EntityId, parents: &HashMap<EntityId, Parent>) -> usize {
        let matching = self.nodes.get(id.as_str());
        let mut path_terms = HashSet::new();
        let mut step = id;
        while let Some((parent, _)) = &parents[step] {
            if let Some(facts) = self.nodes.get(parent.as_str()) {
                path_terms.extend(facts.terms.iter());
            }
            step = parent;
        }
        path_terms.len()
            + matching.map_or(0, |facts| {
                3 * facts.terms.len() + 2 * self.files[&facts.file].matches
            })
    }
}

pub(super) fn walk(
    view: &ReadView<'_>,
    hits: &[SearchHit],
    filter: Option<&StorageSearchFilter>,
    signal: Option<&CancellationToken>,
    query: &str,
    trace: bool,
    result: &mut Expansion,
) -> EngineResult<()> {
    let mut work = GraphWorkTrace::default();
    let mut endpoints = HashMap::new();
    let started = Instant::now();
    let seeds = seed_frontier(view, hits, filter, signal, &mut endpoints)?;
    work.endpoint_micros = micros(started);
    if seeds.is_empty() {
        result.relevance = None;
        if trace {
            result.trace.edge_kinds = ALL_EDGES.to_vec();
            result.trace.work = Some(work);
        }
        return Ok(());
    }
    let ranks: HashMap<_, _> = hits
        .iter()
        .filter(|hit| seeds.contains(hit.entity.id()))
        .map(|hit| (hit.entity.id().clone(), hit.rank))
        .collect();
    let started = Instant::now();
    let priority = Arc::new(Priority::new(view, query, hits, &seeds, signal, &mut work)?);
    work.priority_preparation_micros = micros(started);
    let probe = Arc::clone(&priority);
    let mut frontier: Vec<_> = seeds.iter().cloned().collect();
    let mut reached = Traversal::new(endpoints, &seeds, trace);
    view.with_edge_priority(
        move |inheritance, parent, neighbor| probe.probe(inheritance, parent, neighbor),
        || {
            for (index, limit) in HOP_LIMITS.into_iter().enumerate() {
                check_cancelled(signal)?;
                if frontier.is_empty() {
                    break;
                }
                frontier.sort_by_key(|id| {
                    (
                        Reverse(reached.support[id].len()),
                        best_rank(&reached.support[id], &ranks),
                        id.clone(),
                    )
                });
                let ids: Vec<_> = frontier.iter().map(EntityId::as_str).collect();
                let started = Instant::now();
                let hop = view.ranked_hop(&ids, limit - work.edge_rows_read, signal)?;
                work.index_edge_micros += micros(started);
                *work.sqlite_edge_rows_considered.get_or_insert(0) += hop.rows_considered;
                *work.sqlite_vm_steps.get_or_insert(0) += hop.vm_steps;
                work.edge_rows_read += hop.rows.len();
                let started = Instant::now();
                let missing: BTreeSet<_> = hop
                    .rows
                    .iter()
                    .map(|(pos, _, edge)| {
                        EntityId::from_string(neighbor(edge, &frontier[*pos]).to_owned())
                    })
                    .filter(|id| !reached.endpoints.contains_key(id))
                    .collect();
                let missing: Vec<_> = missing.into_iter().collect();
                for batch in missing.chunks(ENDPOINT_BATCH) {
                    check_cancelled(signal)?;
                    hydrate(view, batch.to_vec(), filter, &mut reached.endpoints)?;
                }
                work.endpoint_micros += micros(started);
                frontier =
                    reached.visit(&frontier, hop.rows, index + 1, &priority, signal, result)?;
            }
            Ok(())
        },
    )?;
    check_cancelled(signal)?;
    reached.retain(&priority, &seeds, &ranks, hits, work, result);
    Ok(())
}

struct Traversal {
    endpoints: HashMap<EntityId, Endpoint>,
    parents: HashMap<EntityId, Parent>,
    depth: HashMap<EntityId, usize>,
    support: HashMap<EntityId, BTreeSet<EntityId>>,
    observed: HashMap<EntityId, Vec<Observed>>,
    reached_ordinary: BTreeSet<String>,
    rejected: HashSet<EntityId>,
    trace: bool,
}

impl Traversal {
    fn new(
        endpoints: HashMap<EntityId, Endpoint>,
        seeds: &BTreeSet<EntityId>,
        trace: bool,
    ) -> Self {
        Self {
            endpoints,
            parents: seeds.iter().cloned().map(|id| (id, None)).collect(),
            depth: seeds.iter().cloned().map(|id| (id, 0)).collect(),
            support: seeds
                .iter()
                .map(|id| (id.clone(), BTreeSet::from([id.clone()])))
                .collect(),
            observed: HashMap::new(),
            reached_ordinary: BTreeSet::new(),
            rejected: HashSet::new(),
            trace,
        }
    }

    fn visit(
        &mut self,
        frontier: &[EntityId],
        rows: Vec<(usize, i64, FileEdge)>,
        depth: usize,
        priority: &Priority,
        signal: Option<&CancellationToken>,
        result: &mut Expansion,
    ) -> EngineResult<Vec<EntityId>> {
        let mut layer = GraphLayerTrace {
            depth,
            expanded: Vec::new(),
            edges_read: rows.len(),
            already_visited: 0,
            filtered: 0,
            invisible: 0,
            reached: Vec::new(),
        };
        let mut expanded = BTreeSet::new();
        let mut next = BTreeMap::<EntityId, BTreeSet<EntityId>>::new();
        for (position, id, edge) in rows {
            check_cancelled(signal)?;
            let source = &frontier[position];
            let target = EntityId::from_string(neighbor(&edge, source).to_owned());
            let endpoint = &self.endpoints[&target];
            if self.trace {
                expanded.insert(source.as_str().to_owned());
            }
            if let Some(reason) = endpoint.rejection {
                match reason {
                    GraphRejection::Filtered => layer.filtered += 1,
                    GraphRejection::Invisible => layer.invisible += 1,
                }
                if self.trace
                    && self.rejected.insert(target)
                    && let Some(entity) = &endpoint.entity
                {
                    result.trace.rejected.push(GraphRejectedTrace {
                        entity: entity.clone(),
                        reason,
                    });
                }
                continue;
            }
            if self.trace && priority.ordinary.contains(target.as_str()) {
                self.reached_ordinary.insert(target.as_str().to_owned());
            }
            if let Some(relevance) = &mut result.relevance {
                relevance.observe(source, &target);
            }
            let kind = edge.kind;
            let evidence = edge_trace(edge, &self.endpoints);
            if self.trace {
                result.trace.edges.push(evidence.clone());
            }
            if matches!(kind, GraphEdgeKind::Calls | GraphEdgeKind::Contains) {
                self.observed
                    .entry(source.clone())
                    .or_default()
                    .push(Observed {
                        id,
                        target: target.clone(),
                        edge: evidence.clone(),
                    });
            }
            if self.depth.contains_key(&target) {
                layer.already_visited += 1;
                continue;
            }
            let sources = next.entry(target.clone()).or_insert_with(|| {
                self.parents
                    .insert(target, Some((source.clone(), evidence)));
                BTreeSet::new()
            });
            sources.extend(self.support[source].iter().cloned());
        }
        let frontier: Vec<_> = next.keys().cloned().collect();
        for (id, sources) in next {
            self.depth.insert(id.clone(), depth);
            self.support.insert(id, sources);
        }
        if self.trace {
            layer.expanded = expanded.into_iter().collect();
            layer.reached = frontier.iter().map(|id| id.as_str().to_owned()).collect();
            result.trace.layers.push(layer);
        }
        Ok(frontier)
    }

    fn retain(
        &self,
        priority: &Priority,
        seeds: &BTreeSet<EntityId>,
        ranks: &HashMap<EntityId, usize>,
        hits: &[SearchHit],
        mut work: GraphWorkTrace,
        result: &mut Expansion,
    ) {
        let started = Instant::now();
        let mut candidates: Vec<_> = self
            .depth
            .keys()
            .filter(|id| {
                self.endpoints[*id].entity.is_some() && !priority.ordinary.contains(id.as_str())
            })
            .cloned()
            .collect();
        let scores: HashMap<_, _> = candidates
            .iter()
            .map(|id| (id.clone(), priority.score(id, &self.parents)))
            .collect();
        candidates.sort_by_key(|id| {
            (
                Reverse(scores[id]),
                Reverse(self.support[id].len()),
                self.depth[id],
                best_rank(&self.support[id], ranks),
                id.clone(),
            )
        });
        let long: Vec<_> = candidates
            .iter()
            .filter(|id| self.depth[*id] >= 2 && scores[*id] > 0)
            .take(PATH_LIMIT)
            .cloned()
            .collect();
        let reached_pool = self.trace.then(|| {
            candidates
                .iter()
                .map(|id| {
                    self.endpoints[id]
                        .entity
                        .clone()
                        .expect("reached candidate")
                })
                .collect()
        });
        let previous = reserve(candidates, &long);
        let mut sorted_seeds: Vec<_> = seeds.iter().cloned().collect();
        sorted_seeds.sort_by_key(|id| (ranks[id], id.clone()));
        let (path_ids, paths) = path_candidates(
            priority,
            &sorted_seeds,
            &self.observed,
            ranks,
            &previous,
            self.trace,
        );
        let mut selected = reserve(previous, &path_ids);
        selected.truncate(CANDIDATE_LIMIT);
        work.retention_micros = micros(started);

        result.added = selected.len();
        if let Some(relevance) = &mut result.relevance {
            relevance.initialize(hits, seeds, &selected);
        }
        if self.trace {
            result.trace.edge_kinds = ALL_EDGES.to_vec();
            result.trace.seeds = seeds.iter().map(|id| id.as_str().to_owned()).collect();
            result.trace.reached_ordinary = self.reached_ordinary.iter().cloned().collect();
            result.trace.added = selected
                .iter()
                .map(|id| {
                    witness(
                        id,
                        self.endpoints[id]
                            .entity
                            .as_ref()
                            .expect("selected candidate"),
                        &self.parents,
                    )
                })
                .collect();
            result.trace.work = Some(work);
            result.trace.budget = Some(GraphBudgetTrace {
                edge_limit: HOP_LIMITS[MAX_HOPS - 1],
                candidate_limit: CANDIDATE_LIMIT,
                reached_pool: reached_pool.expect("trace enabled"),
                retained_paths: paths,
            });
        }
    }
}

fn best_rank(origins: &BTreeSet<EntityId>, ranks: &HashMap<EntityId, usize>) -> usize {
    origins
        .iter()
        .map(|id| ranks[id])
        .min()
        .expect("nonempty seed support")
}

fn reserve(mut ordered: Vec<EntityId>, reserved: &[EntityId]) -> Vec<EntityId> {
    let reserved_ids: HashSet<_> = reserved.iter().collect();
    ordered.retain(|id| !reserved_ids.contains(id));
    let position = (CANDIDATE_LIMIT - reserved.len()).min(ordered.len());
    ordered.splice(position..position, reserved.iter().cloned());
    ordered
}

fn path_candidates(
    priority: &Priority,
    seeds: &[EntityId],
    observed: &HashMap<EntityId, Vec<Observed>>,
    ranks: &HashMap<EntityId, usize>,
    previous: &[EntityId],
    trace: bool,
) -> (Vec<EntityId>, Vec<GraphRetainedPathTrace>) {
    let positions: HashMap<_, _> = previous.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let mut anchors = HashMap::new();
    for seed in seeds {
        if let Some(node) = priority.nodes.get(seed.as_str())
            && node.exact
        {
            anchors.entry(node.file).or_insert(seed);
        }
    }
    let mut groups = Vec::new();
    for seed in seeds {
        let Some(facts) = priority.nodes.get(seed.as_str()) else {
            continue;
        };
        let anchor = if facts.exact {
            seed
        } else if let Some(anchor) = anchors.get(&facts.file).copied() {
            anchor
        } else {
            continue;
        };
        for call in observed.get(seed).into_iter().flatten() {
            let member = &call.target;
            if call.edge.kind != Some(GraphEdgeKind::Calls)
                || member == seed
                || (seed != anchor && !priority.same_file(seed, member))
            {
                continue;
            }
            if positions.contains_key(member) {
                groups.push(PathGroup {
                    anchor,
                    seed,
                    steps: vec![call],
                });
            }
            for contains in observed.get(member).into_iter().flatten() {
                let owner = &contains.target;
                if contains.edge.kind == Some(GraphEdgeKind::Contains)
                    && contains.edge.callee_id == member.as_str()
                    && owner != seed
                    && positions.contains_key(owner)
                    && priority.same_file(owner, member)
                    && priority.nodes[owner.as_str()].enclosing_type
                {
                    groups.push(PathGroup {
                        anchor,
                        seed,
                        steps: vec![call, contains],
                    });
                }
            }
        }
    }
    groups.sort_by_key(|group| {
        (
            group.seed != group.anchor,
            Reverse(group.steps.len()),
            ranks[group.anchor],
            ranks[group.seed],
            group
                .steps
                .iter()
                .filter_map(|step| positions.get(&step.target))
                .min()
                .copied(),
            group.steps.iter().map(|step| step.id).collect::<Vec<_>>(),
        )
    });
    let mut reserved = Vec::new();
    let mut selected = HashSet::new();
    let mut paths = Vec::new();
    for group in groups {
        let additions: Vec<_> = group
            .steps
            .iter()
            .map(|step| &step.target)
            .filter(|id| positions.contains_key(id) && !selected.contains(*id))
            .cloned()
            .collect();
        if additions.is_empty() || reserved.len() + additions.len() > PATH_LIMIT {
            continue;
        }
        selected.extend(additions.iter().cloned());
        if trace {
            paths.push(GraphRetainedPathTrace {
                anchor_id: group.anchor.as_str().to_owned(),
                seed_id: group.seed.as_str().to_owned(),
                candidate_ids: additions.iter().map(|id| id.as_str().to_owned()).collect(),
                path: group
                    .steps
                    .into_iter()
                    .map(|step| step.edge.clone())
                    .collect(),
            });
        }
        reserved.extend(additions);
    }
    (reserved, paths)
}

fn test_path(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        parent.iter().any(|part| {
            matches!(
                part.to_string_lossy().to_ascii_lowercase().as_str(),
                "test" | "tests" | "testing" | "fixtures" | "spec"
            )
        })
    }) || path.file_stem().is_some_and(|stem| {
        let name = stem.to_string_lossy().to_ascii_lowercase();
        name.starts_with("test_") || name.ends_with("_test")
    })
}

fn tokens(text: &str) -> HashSet<String> {
    const STOP: &str = "a an and are as at be been both by can could did do does for from how in into is it its may of on or should so than that the their them then there these they this those to was were when where which while who will with would";
    let mut result = HashSet::new();
    for word in text.split(|c: char| !c.is_ascii_alphabetic()) {
        let bytes = word.as_bytes();
        let mut start = 0;
        for end in 1..=bytes.len() {
            if end < bytes.len()
                && !(bytes[end].is_ascii_uppercase()
                    && (bytes[end - 1].is_ascii_lowercase()
                        || bytes.get(end + 1).is_some_and(u8::is_ascii_lowercase)))
            {
                continue;
            }
            let mut part = word[start..end].to_ascii_lowercase();
            start = end;
            if part.len() < 3 || STOP.split_ascii_whitespace().any(|stop| stop == part) {
                continue;
            }
            if part.len() > 4 && part.ends_with("ies") {
                part.truncate(part.len() - 3);
                part.push('y');
            } else if part.len() > 4 && part.ends_with('s') && !part.ends_with("ss") {
                part.pop();
            }
            result.insert(part);
        }
    }
    result
}
