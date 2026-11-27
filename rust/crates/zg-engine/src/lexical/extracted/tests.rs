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
        let mut offset = 0;
        for text in [
            "prefix\n",
            "hit one\n",
            "",
            "hit two\n",
            "gap\nhit hidden\n",
            "hit later\n",
        ] {
            found.extend(
                matcher
                    .search(text, offset, &mut state)
                    .expect("entity search"),
            );
            offset += text.len() as u64;
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
        .search("prefix\nhit\ngap\n", 0, &mut state)
        .expect("first");
    assert_eq!(first.len(), 1);
    assert!(state.stopped);
    assert!(
        matcher
            .search("hit\n", 15, &mut state)
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
        let mut offset = 0;
        let found = entities
            .into_iter()
            .map(|text| {
                let found = matcher
                    .search(text, offset, &mut state)
                    .expect("search")
                    .len();
                offset += text.len() as u64;
                found
            })
            .sum::<usize>();
        assert_eq!(found, expected, "{pattern}, invert={invert}");
        assert_eq!(state.count, expected);
    }
}

#[test]
fn overlapping_entities_deduplicate_before_max_count_but_keep_distinct_positions() {
    for (pattern, invert, multiline) in [
        ("hit", false, false),
        ("skip", true, false),
        ("hit", false, true),
        ("^", false, false),
    ] {
        let mut request = LexicalSearchRequest {
            patterns: vec![pattern.into()],
            ..LexicalSearchRequest::default()
        };
        request.options.matching.max_count = Some(2);
        request.options.matching.invert_match = invert;
        request.options.matching.multiline = multiline;
        let matcher = ExtractedMatcher::new(Path::new("."), &request).expect("valid test fixture");
        let mut state = ExtractedFileState::default();
        assert_eq!(
            matcher
                .search("hit\n", 10, &mut state)
                .expect("valid test fixture")
                .len(),
            1
        );
        let found = matcher
            .search("hit\nhit\nhit\n", 10, &mut state)
            .expect("valid test fixture");
        assert_eq!(found.len(), 1, "{pattern}");
        assert_eq!(found[0].content_range.start_byte_offset(), 4);
        assert_eq!(state.count, 2);
        assert!(matcher.truncated.load(Ordering::Relaxed));
    }
}

#[test]
fn overlapping_prefix_does_not_trigger_stop_on_nonmatch() {
    let matcher = matcher("hit", false, false);
    let mut state = ExtractedFileState::default();
    assert_eq!(
        matcher
            .search("prefix\nhit\n", 0, &mut state)
            .expect("valid test fixture")
            .len(),
        1
    );
    assert!(
        matcher
            .search("prefix\nhit\n", 0, &mut state)
            .expect("valid test fixture")
            .is_empty()
    );
    assert!(!state.stopped);
    assert_eq!(
        matcher
            .search("hit\n", 11, &mut state)
            .expect("valid test fixture")
            .len(),
        1
    );
}
