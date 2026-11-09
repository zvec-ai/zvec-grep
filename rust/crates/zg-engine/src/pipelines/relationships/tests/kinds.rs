use super::*;
use crate::api::relationships::GraphEdgeKind;

fn fixture_with_kinds() -> Fixture {
    use GraphEdgeKind::{Calls, Contains, Declares, Extends, Implements, Imports};
    let fixture = Fixture::new();
    let nodes = fixture.add_symbols(
        "relations.rs",
        &[
            ("subject", None, 1, "fn subject() {}"),
            ("other", None, 3, "fn other() {}"),
        ],
    );
    let subject = nodes[0].id().as_str();
    let other = nodes[1].id().as_str();
    let file_id = nodes[0].file_id().get();
    let file_node = format!("f{file_id}");
    let graph = rusqlite::Connection::open(&fixture.graph_path).expect("graph");
    for (kind, source, target, line) in [
        (Calls, subject, other, Some(7)),
        (Imports, subject, other, Some(8)),
        (Extends, subject, other, Some(9)),
        (Implements, other, subject, Some(10)),
        (Declares, subject, subject, Some(11)),
        (Contains, file_node.as_str(), subject, None),
        (Calls, other, subject, Some(12)),
    ] {
        graph
            .execute(
                "INSERT INTO edges (file_id, source, target, kind, status, line, provenance)
             VALUES (?, ?, ?, ?, 'resolved', ?, 'file_local')",
                rusqlite::params![file_id, source, target, kind.as_str(), line],
            )
            .expect("persist relationship");
    }
    drop(graph);
    fixture
}

#[tokio::test]
async fn relationships_select_multiple_kinds_and_preserve_edge_orientation() {
    use GraphEdgeKind::{Calls, Contains, Declares, Extends, Implements, Imports};
    let fixture = fixture_with_kinds();
    for (direction, kinds, expected) in [
        (
            Direction::In,
            None,
            vec![None, Some(10), Some(11), Some(12)],
        ),
        (
            Direction::Out,
            None,
            vec![Some(7), Some(8), Some(9), Some(11)],
        ),
        (
            Direction::Both,
            None,
            vec![
                None,
                Some(7),
                Some(8),
                Some(9),
                Some(10),
                Some(11),
                Some(12),
            ],
        ),
        (
            Direction::Both,
            Some(vec![Imports, Extends, Imports]),
            vec![Some(8), Some(9)],
        ),
        (Direction::Both, Some(vec![]), vec![]),
    ] {
        let result = fixture
            .engine
            .relationships(
                direction,
                RelationshipOptions {
                    kinds,
                    ..fixture.options("subject")
                },
            )
            .await
            .expect("relationships");
        assert_eq!(result.matches.len(), 1);
        let matched = &result.matches[0];
        assert!(!matched.truncated);
        let mut lines: Vec<_> = matched.edges.iter().map(|edge| edge.line).collect();
        lines.sort_unstable();
        assert_eq!(lines, expected);
        for edge in &matched.edges {
            let (kind, orientation) = match edge.line {
                Some(7) => (Calls, Direction::Out),
                Some(8) => (Imports, Direction::Out),
                Some(9) => (Extends, Direction::Out),
                Some(10) => (Implements, Direction::In),
                Some(11) => (Declares, Direction::Both),
                Some(12) => (Calls, Direction::In),
                None => (Contains, Direction::In),
                _ => panic!("unexpected edge"),
            };
            assert_eq!((edge.kind, edge.direction), (kind, orientation));
            assert_eq!(edge.file_path, PathBuf::from("relations.rs"));
            assert_eq!(edge.symbol.is_none(), kind == Contains);
        }
    }
}

#[tokio::test]
async fn kind_filters_precede_limits_and_call_queries_always_select_calls() {
    use GraphEdgeKind::{Calls, Extends, Imports};
    let fixture = fixture_with_kinds();
    let options = RelationshipOptions {
        kinds: Some(vec![Imports, Extends]),
        limit: Some(1),
        ..fixture.options("subject")
    };
    let result = fixture
        .engine
        .relationships(Direction::Out, options.clone())
        .await
        .expect("bounded kinds");
    assert_eq!(result.matches[0].edges.len(), 1);
    assert!(result.matches[0].truncated);
    assert!([Imports, Extends].contains(&result.matches[0].edges[0].kind));
    for (result, line) in [
        (fixture.engine.callers(options.clone()).await, 12),
        (fixture.engine.callees(options).await, 7),
    ] {
        let result = result.expect("call-only convenience query");
        assert_eq!(result.matches[0].edges.len(), 1);
        assert_eq!(result.matches[0].edges[0].kind, Calls);
        assert_eq!(result.matches[0].edges[0].line, Some(line));
        assert!(!result.matches[0].truncated);
    }
}
