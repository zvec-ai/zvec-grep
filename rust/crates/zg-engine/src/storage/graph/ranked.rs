use rusqlite::{StatementStatus, functions::FunctionFlags, params};
use tokio_util::sync::CancellationToken;

use crate::domain::FileEdge;

use super::{Result, SqliteGraphStorage, reader::edge_from_row};

#[derive(Debug)]
pub(crate) struct RankedHop {
    pub rows: Vec<(usize, i64, FileEdge)>,
    pub rows_considered: usize,
    pub vm_steps: u64,
}

impl SqliteGraphStorage {
    /// The caller serializes this callback under the store snapshot lock.
    pub(crate) fn with_edge_priority<T>(
        &self,
        priority: impl Fn(bool, &str, &str) -> Option<i64> + Send + 'static,
        read: impl FnOnce() -> T,
    ) -> Result<T> {
        const NAME: &str = "zg_explore_priority";
        self.connection.create_scalar_function(
            NAME,
            3,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                let inheritance: bool = ctx.get(0)?;
                let parent: String = ctx.get(1)?;
                let neighbor: String = ctx.get(2)?;
                Ok(priority(inheritance, &parent, &neighbor))
            },
        )?;
        let result = read();
        self.connection.remove_function(NAME, 3)?;
        Ok(result)
    }

    /// Return at most the remaining hop budget of rows. SQLite computes local
    /// priorities before the limit, so count that work separately.
    pub(crate) fn ranked_hop(
        &self,
        frontier: &[&str],
        limit: usize,
        signal: Option<&CancellationToken>,
    ) -> Result<RankedHop> {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(super::Error::Cancelled);
        }
        if let Some(signal) = signal {
            let signal = signal.clone();
            // Scheduling may inspect far more rows than the returned edge budget.
            self.connection
                .progress_handler(1_000, Some(move || signal.is_cancelled()));
        }
        let result = self.select_hop(frontier, limit);
        if signal.is_some() {
            self.connection.progress_handler(0, None::<fn() -> bool>);
        }
        match result {
            Err(super::Error::Sqlite(error))
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
                    && signal.is_some_and(CancellationToken::is_cancelled) =>
            {
                Err(super::Error::Cancelled)
            }
            result => result,
        }
    }

    fn select_hop(&self, frontier: &[&str], limit: usize) -> Result<RankedHop> {
        let mut stmt = self.connection.prepare(RANKED_HOP)?;
        let frontier = serde_json::to_string(frontier)?;
        let mut rows = stmt.query(params![frontier, limit])?;
        let mut selected = Vec::new();
        let mut rows_considered = 0;
        while let Some(row) = rows.next()? {
            rows_considered = row.get(8)?;
            selected.push((row.get(7)?, row.get(9)?, edge_from_row(row)?));
        }
        drop(rows);
        Ok(RankedHop {
            rows: selected,
            rows_considered,
            vm_steps: u64::try_from(stmt.get_status(StatementStatus::VmStep))
                .map_err(|_| super::Error::InvalidInput("SQLite VM step count is negative"))?,
        })
    }
}

// The frontier arrives in scheduler order. Take one row per node per round,
// using local probes first and then alternating incoming and outgoing edges.
const RANKED_HOP: &str = r"
WITH frontier AS (
    SELECT CAST(key AS INTEGER) AS pos, value AS node FROM json_each(?1)
),
links AS MATERIALIZED (
    SELECT f.pos, f.node, 0 AS direction, e.id, e.kind, e.source AS neighbor
    FROM frontier AS f CROSS JOIN edges AS e
    WHERE e.target = f.node AND e.source <> f.node AND e.status = 'resolved'
    UNION ALL
    SELECT f.pos, f.node, 1, e.id, e.kind, e.target
    FROM frontier AS f CROSS JOIN edges AS e
    WHERE e.source = f.node AND e.status = 'resolved'
),
ordered AS MATERIALIZED (
    SELECT *, ROW_NUMBER() OVER (
        PARTITION BY pos, direction ORDER BY CASE kind
            WHEN 'contains' THEN 0 WHEN 'calls' THEN 1
            WHEN 'extends' THEN 2 WHEN 'implements' THEN 3
            WHEN 'imports' THEN 4 ELSE 5 END, id
    ) AS direction_order
    FROM links
),
first_sites AS (
    SELECT *, kind <> 'calls' AS inheritance, ROW_NUMBER() OVER (
        PARTITION BY pos, kind = 'calls', neighbor ORDER BY direction_order
    ) AS site
    FROM ordered WHERE direction = 0 AND kind IN ('calls', 'extends', 'implements')
),
distinct_endpoints AS (
    SELECT *, COUNT(*) OVER (PARTITION BY pos, inheritance) AS fanout
    FROM first_sites WHERE site = 1
),
scored AS MATERIALIZED (
    SELECT pos, id, inheritance, zg_explore_priority(inheritance, node, neighbor) AS score
    FROM distinct_endpoints WHERE inheritance OR fanout >= 64
),
probes AS (
    SELECT *, ROW_NUMBER() OVER (PARTITION BY pos, inheritance ORDER BY score, id) AS probe
    FROM scored WHERE score IS NOT NULL
),
scheduled AS (
    SELECT o.pos, o.id, ROW_NUMBER() OVER (
        PARTITION BY o.pos
        ORDER BY CASE WHEN p.id IS NULL THEN 2 WHEN p.inheritance THEN 0 ELSE 1 END,
                 p.probe, o.direction_order, o.direction
    ) AS turn
    FROM ordered AS o
    LEFT JOIN probes AS p ON p.pos = o.pos AND p.id = o.id
        AND o.direction = 0 AND p.probe <= CASE WHEN p.inheritance THEN 3 ELSE 2 END
),
selected AS MATERIALIZED (
    SELECT pos, id, turn, COUNT(*) OVER () AS rows_considered
    FROM scheduled ORDER BY turn, pos LIMIT ?2
)
SELECT e.kind, e.source, e.target, e.line, e.col, e.provenance, e.metadata,
       s.pos, s.rows_considered, e.id
FROM selected AS s JOIN edges AS e ON e.id = s.id
ORDER BY s.turn, s.pos
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::GraphEdgeKind;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn high_fanout_graph() -> SqliteGraphStorage {
        let graph = SqliteGraphStorage::in_memory().expect("graph");
        graph
            .connection
            .execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 1000)
             INSERT INTO edges (file_id, source, target, kind, status, provenance)
             SELECT 1, 'caller-' || x, 'seed', 'calls', 'resolved', 'file_local' FROM n",
                [],
            )
            .expect("callers");
        graph
    }

    #[test]
    fn ranked_hop_cancels_during_sql_and_removes_its_progress_handler() {
        let graph = high_fanout_graph();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        graph
            .with_edge_priority(
                move |_, _, _| {
                    if observed.fetch_add(1, Ordering::Relaxed) == 0 {
                        cancel.cancel();
                    }
                    None
                },
                || {
                    let error = graph
                        .ranked_hop(&["seed"], 600, Some(&signal))
                        .expect_err("cancel while SQLite schedules callers");
                    assert!(matches!(error, super::super::Error::Cancelled));
                    assert!((1..1000).contains(&calls.load(Ordering::Relaxed)));
                    assert_eq!(
                        graph
                            .ranked_hop(&["seed"], 1, None)
                            .expect("reuse connection")
                            .rows
                            .len(),
                        1
                    );
                    let next = CancellationToken::new();
                    assert_eq!(
                        graph
                            .ranked_hop(&["seed"], 1, Some(&next))
                            .expect("new request")
                            .rows
                            .len(),
                        1
                    );
                },
            )
            .expect("priority callback");
        assert!(matches!(
            graph.ranked_hop(&[], 1, Some(&signal)),
            Err(super::super::Error::Cancelled)
        ));
    }

    #[test]
    fn sqlite_interrupt_without_request_cancellation_remains_a_storage_error() {
        let graph = high_fanout_graph();
        let signal = CancellationToken::new();
        let interrupt = graph.connection.get_interrupt_handle();
        graph.with_edge_priority(
            move |_, _, _| {
                interrupt.interrupt();
                None
            },
            || {
                let error = graph.ranked_hop(&["seed"], 600, Some(&signal))
                    .expect_err("unrelated interruption");
                assert!(matches!(error, super::super::Error::Sqlite(error)
                    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)));
                assert!(!signal.is_cancelled());
            },
        ).expect("priority callback");
    }

    #[test]
    fn ranked_hop_prioritizes_incoming_probes_and_preserves_selected_payloads() {
        let graph = SqliteGraphStorage::in_memory().expect("graph");
        let add = |source: &str, target: &str, kind: &str, line: usize| {
            graph.connection.execute(
                "INSERT INTO edges (file_id, source, target, kind, status, line, col, provenance, metadata)
                 VALUES (1, ?1, ?2, ?3, 'resolved', ?4, 4, 'file_local', ?5)",
                params![source, target, kind, line, json!({"payload": [line, "中文"]}).to_string()],
            ).expect("edge");
        };
        for index in 0..70 {
            add(&format!("caller-{index}"), "seed", "calls", index + 1);
        }
        add("caller-69", "seed", "calls", 171);
        add("derived", "seed", "extends", 172);
        add("seed", "outgoing", "calls", 173);
        add("seed", "seed", "calls", 174);
        graph
            .with_edge_priority(
                |inheritance, _, neighbor| (inheritance || neighbor == "caller-69").then_some(0),
                || {
                    let selected = graph
                        .ranked_hop(&["seed", "isolated"], 2, None)
                        .expect("ranked hop");
                    assert_eq!(selected.rows_considered, 74);
                    assert_eq!(selected.rows.len(), 2);
                    assert!(selected.rows.iter().all(|(pos, _, _)| *pos == 0));
                    assert_eq!(selected.rows[0].2.kind, GraphEdgeKind::Extends);
                    let call = &selected.rows[1].2;
                    assert_eq!(call.source, "caller-69");
                    assert_eq!(call.target, "seed");
                    assert_eq!(call.line, Some(70));
                    assert_eq!(call.column, Some(4));
                    assert_eq!(call.metadata["payload"], json!([70, "中文"]));
                    let full = graph.ranked_hop(&["seed"], 100, None).expect("all edges");
                    assert_eq!(full.rows.len(), 74);
                    assert_eq!(
                        full.rows
                            .iter()
                            .filter(|(_, _, edge)| edge.source == edge.target)
                            .count(),
                        1
                    );
                    assert!(
                        graph
                            .ranked_hop(&["seed"], 0, None)
                            .expect("zero budget")
                            .rows
                            .is_empty()
                    );
                    assert!(
                        graph
                            .ranked_hop(&["isolated"], 10, None)
                            .expect("isolated")
                            .rows
                            .is_empty()
                    );
                },
            )
            .expect("priority callback");
    }
}
