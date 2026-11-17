use super::*;

fn matcher(pattern: &str, invert: bool, crlf: bool) -> ExtractedMatcher {
    let mut request = LexicalSearchRequest {
        patterns: vec![pattern.into()],
        ..LexicalSearchRequest::default()
    };
    request.options.matching.stop_on_nonmatch = true;
    request.options.matching.invert_match = invert;
    request.options.matching.crlf = crlf;
    ExtractedMatcher::new(Path::new("."), &request).expect("matcher")
}

#[test]
fn stop_on_nonmatch_carries_contiguous_matches_across_entities_and_resets_per_file() {
    let matcher = matcher("^hit", false, false);
    for _ in 0..2 {
        let mut state = ExtractedFileState::default();
        let mut found = Vec::new();
        for text in [
            "prefix\n",
            "hit one\n",
            "",
            "hit two\n",
            "gap\nhit hidden\n",
            "hit later\n",
        ] {
            found.extend(matcher.search(text, &mut state).expect("entity search"));
        }
        assert_eq!(
            found.iter().map(LexicalMatch::content).collect::<Vec<_>>(),
            ["hit one", "hit two"],
        );
        assert_eq!(state.count, 2);
        assert!(state.stopped);
    }
}

#[test]
fn stop_on_nonmatch_keeps_the_stop_after_a_gap_inside_an_entity() {
    let matcher = matcher("hit", false, false);
    let mut state = ExtractedFileState::default();
    let first = matcher
        .search("prefix\nhit\ngap\n", &mut state)
        .expect("first");
    assert_eq!(first.len(), 1);
    assert!(state.stopped);
    assert!(
        matcher
            .search("hit\n", &mut state)
            .expect("next")
            .is_empty()
    );
}

#[test]
fn stop_on_nonmatch_handles_crlf_inverted_and_zero_width_matches() {
    for (pattern, invert, entities, expected) in [
        ("hit", false, ["prefix\r\nhit\r\n", "gap\r\nhit\r\n"], 1),
        ("skip", true, ["skip\r\nkeep\r\n", "skip\r\nkeep\r\n"], 1),
        ("^", false, ["one\r\n", "two\r\n"], 2),
        ("^$", false, ["prefix\r\n\r\n", "gap\r\n\r\n"], 1),
    ] {
        let matcher = matcher(pattern, invert, true);
        let mut state = ExtractedFileState::default();
        let found = entities
            .into_iter()
            .map(|text| matcher.search(text, &mut state).expect("search").len())
            .sum::<usize>();
        assert_eq!(found, expected, "{pattern}, invert={invert}");
        assert_eq!(state.count, expected);
    }
}
