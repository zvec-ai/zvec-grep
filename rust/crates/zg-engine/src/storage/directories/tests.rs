use super::super::zvec::initialize;
use super::*;

#[test]
fn persisted_ancestors_keep_their_identities_after_reopen() {
    initialize().expect("initialize zvec");
    let root = tempfile::tempdir().expect("storage");
    let first = SourcePath::new("src/nested/main.rs").expect("source path");
    let ids = {
        let directories = Directories::open(root.path(), false).expect("directories");
        directories.load().expect("load identities");
        let ids = directories.ensure(&first).expect("first ancestors");
        assert_eq!(ids, [DirectoryId::new(0), DirectoryId::new(1)]);
        assert_eq!(directories.ensure(&first).expect("same ancestors"), ids);
        assert!(
            directories
                .ensure(&SourcePath::new("main.rs").expect("root file"))
                .expect("no ancestors")
                .is_empty()
        );
        directories.flush().expect("flush");
        ids
    };
    let directories = Directories::open(root.path(), false).expect("reopen");
    directories.load().expect("load identities");
    assert_eq!(
        directories.ensure(&first).expect("retained identities"),
        ids
    );
    assert_eq!(
        directories
            .get(&SourcePath::new("src/nested").expect("directory"))
            .expect("identity"),
        Some(ids[1])
    );
    assert_eq!(
        directories
            .ensure(&SourcePath::new("src/other/main.rs").expect("source path"))
            .expect("new ancestor"),
        [ids[0], DirectoryId::new(2)]
    );
}

#[test]
fn load_rejects_missing_or_inconsistent_parent_records() {
    initialize().expect("initialize zvec");
    for parent in [None, Some(5)] {
        let root = tempfile::tempdir().expect("storage");
        let directories = Directories::open(root.path(), false).expect("directories");
        let mut ids = DirectoryIds::default();
        ids.resolve(&SourcePath::new("src/nested/main.rs").expect("source path"))
            .expect("ancestors");
        let mut doc =
            encode_directory_doc(&SourcePath::new("src/nested").expect("directory"), &ids)
                .expect("document");
        if let Some(parent) = parent {
            doc.add_u32("parent_directory_id", parent)
                .expect("wrong parent");
            write_docs(
                &directories.collection,
                &[
                    encode_directory_doc(&SourcePath::new("src").expect("parent"), &ids)
                        .expect("parent document"),
                ],
                "write parent",
            )
            .expect("parent");
        }
        write_docs(&directories.collection, &[doc], "write directory").expect("child");
        let error = directories.load().expect_err("invalid parent relationship");
        assert!(error.message().contains("missing or inconsistent parent"));
    }
}
