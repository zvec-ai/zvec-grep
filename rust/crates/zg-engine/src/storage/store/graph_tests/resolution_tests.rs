use super::*;
use crate::domain::{CodeMetadata, EntityMetadata, Language};

#[test]
fn candidates_are_complete_and_distinct_without_loading_source_bodies() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let names = (0..1030)
        .map(|i| format!("definition {i}"))
        .collect::<Vec<_>>();
    let texts = names.iter().map(String::as_str).collect::<Vec<_>>();
    let mut source = snapshot(&store, "source.rs", &texts);
    source.entities = source
        .entities
        .iter()
        .enumerate()
        .map(|(index, entity)| {
            // A single definition can fill a whole native query batch with fragments.
            let fragments = (0..if index == 0 { 1024 } else { 1 })
                .map(|ordinal| EntityFragment {
                    id: FragmentId::new(entity.id(), ordinal),
                    range: Range::Full,
                })
                .collect();
            Entity::new(
                entity.id().clone(),
                entity.file_id(),
                entity.source_range(),
                entity.content().clone(),
                Some(EntityMetadata::Code(CodeMetadata {
                    symbol_type: Some(SymbolType::Function),
                    symbol_name: Some("target'quoted".into()),
                    scope: Some(format!("scope{index}")),
                    signature: Some("fn target()".into()),
                    documentation: None,
                    visibility: None,
                    language: Some(Language::Rust),
                })),
                fragments,
            )
            .expect("named entity")
        })
        .collect();
    source.fragments = source
        .entities
        .iter()
        .flat_map(|entity| {
            entity.fragments().iter().map(|fragment| IndexedFragment {
                entity_id: entity.id().clone(),
                fragment_id: fragment.id.clone(),
                model: "fixture/graph".into(),
                vector: vec![1.0, 0.0, 0.0],
                fts_text: "target'quoted".into(),
            })
        })
        .collect();
    write(&store, &source);
    // The candidate path only needs metadata, so a source body must not be decoded.
    store
        .write(|state| {
            let mut docs = Entities::prepare(&source.entities[..1])?;
            docs[0]
                .add_string("payload", "invalid canonical body")
                .expect("replace body");
            state.entities.write(&docs)
        })
        .expect("poison unused body");
    let candidates = store
        .find_resolution_candidates("target'quoted")
        .expect("all candidates");
    assert_eq!(candidates.len(), source.entities.len());
    assert_eq!(
        candidates
            .iter()
            .map(|c| &c.entity_id)
            .collect::<HashSet<_>>()
            .len(),
        candidates.len()
    );
    assert!(candidates.iter().all(|c| c.file.id() == source.file.id() && c.metadata.language == Some(Language::Rust)));
    assert!(
        store
            .find_resolution_candidates("Target'quoted")
            .expect("case sensitive")
            .is_empty()
    );
    assert!(store.find_resolution_candidates(" ").is_err());
    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Deleting,
        FileIndexStatus::Failed {
            error: "fixture".into(),
        },
    ] {
        let mut file = source.file.clone();
        file.set_index_status(status).expect("file status");
        store
            .write(|state| state.files.put(&file, &[]))
            .expect("hide file");
        assert!(
            store
                .find_resolution_candidates("target'quoted")
                .expect("hidden candidates")
                .is_empty()
        );
    }
    store.close().expect("close writer");
}

#[test]
fn pending_pages_preserve_cursor_and_writeback_checks_both_endpoints() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let mut hidden = snapshot(&store, "hidden.rs", &["hidden"]);
    hidden
        .graph
        .pending_refs
        .push(reference(&hidden.entities[0]));
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &hidden);
    write(&store, &source);
    let hidden_ref = pending(&store)[0].id;
    store
        .write(|state| state.files.put(&hidden.file, &[]))
        .expect("unfinished file");
    let page = store.list_pending_refs(1, 0).expect("hidden page");
    assert!(page.refs.is_empty());
    // Ordinary store reads between resolver operations do not recursively lock.
    assert_eq!(store.list_files().expect("read files").len(), 2);
    let page = store
        .list_pending_refs(1, page.next_cursor.expect("advance cursor"))
        .expect("visible page");
    assert_eq!(page.refs.len(), 1);
    assert!(page.next_cursor.is_none());
    let proposal = Resolution {
        ref_id: page.refs[0].id,
        target_id: source.entities[0].id().as_str().into(),
        provenance: EdgeProvenance::ImportScoped,
    };
    for invalid in [
        Resolution {
            ref_id: hidden_ref,
            ..proposal.clone()
        },
        Resolution {
            target_id: hidden.entities[0].id().as_str().into(),
            ..proposal.clone()
        },
        Resolution {
            target_id: format!("f{}", hidden.file.id().get()),
            ..proposal.clone()
        },
        Resolution {
            target_id: "missing".into(),
            ..proposal.clone()
        },
        Resolution {
            ref_id: i64::MAX,
            ..proposal.clone()
        },
    ] {
        let stats = store
            .apply_resolutions(&[invalid])
            .expect("skip unavailable endpoints");
        assert_eq!((stats.resolved, stats.stale), (0, 1));
    }
    // An invalid batch must not partially resolve its valid proposal.
    assert!(
        store
            .apply_resolutions(&[
                proposal.clone(),
                Resolution {
                    provenance: EdgeProvenance::FileLocal,
                    ..proposal.clone()
                }
            ])
            .is_err()
    );
    let proposal = Resolution {
        target_id: format!("f{}", source.file.id().get()),
        ..proposal
    };
    let stats = store
        .apply_resolutions(std::slice::from_ref(&proposal))
        .expect("visible file endpoint");
    assert_eq!((stats.resolved, stats.stale), (1, 0));
    let stats = store
        .apply_resolutions(&[proposal])
        .expect("already resolved");
    assert_eq!((stats.resolved, stats.stale), (0, 1));
    store.close().expect("close writer");
    assert!(store.list_pending_refs(1, 0).is_err());
    let reader = open(temporary.path(), true);
    assert!(reader.list_pending_refs(1, 0).is_err());
    assert!(reader.apply_resolutions(&[]).is_err());
    reader.close().expect("close reader");
}
