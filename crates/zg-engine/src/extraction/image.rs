use crate::{
    EngineError,
    domain::{Content, Entity, EntityContent, SourceRange},
};

use super::{EntityFragment, ImageSource, make_entity_id, validate_source_file};

pub(super) fn extract(source: &ImageSource) -> Result<Vec<EntityFragment>, EngineError> {
    validate_source_file(&source.file)?;
    Ok(vec![EntityFragment::Standalone(Entity {
        id: make_entity_id(&source.file.id, 0),
        file_id: source.file.id.clone(),
        range: SourceRange::File,
        content: EntityContent::Source(vec![Content::Image(source.content.clone())]),
        metadata: None,
    })])
}

#[cfg(test)]
mod tests {
    use crate::domain::{Content, EntityFragment, FileFormat, ImageContent, SourceRange};

    use super::super::{
        ChunkOptions, ImageSource, extract as extract_source, extract_for_indexing, test_content,
        test_file,
    };
    use super::extract;

    fn image_source(data: Vec<u8>) -> ImageSource {
        ImageSource {
            file: test_file(FileFormat::Png, "fixture.png", data.len() as u64),
            content: ImageContent::new(data, FileFormat::Png).expect("image content"),
        }
    }

    #[test]
    fn preserves_image_bytes_format_and_file_range() {
        let source = image_source(vec![1, 2, 3]);
        let fragments = extract(&source).expect("image extraction");
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].document_id().len(), 64);
        assert_eq!(fragments[0].file_id(), &source.file.id);
        assert_eq!(fragments[0].range(), &SourceRange::File);
        assert!(matches!(fragments[0], EntityFragment::Standalone(_)));
        assert_eq!(fragments[0].metadata(), None);
        assert_eq!(test_content(&fragments[0]), Content::Image(source.content));
    }

    #[test]
    fn rejects_invalid_source_metadata() {
        let mut missing_absolute_path = image_source(vec![1]);
        missing_absolute_path.file.absolute_path.clear();
        assert!(extract(&missing_absolute_path).is_err());

        let mut missing_relative_path = image_source(vec![1]);
        missing_relative_path.file.relative_path.clear();
        assert!(extract(&missing_relative_path).is_err());
    }

    #[test]
    fn source_router_and_indexing_preserve_the_image_fragment() {
        let source = image_source(vec![4, 5, 6]);
        let direct = extract_source(&source, ChunkOptions::default()).expect("direct extraction");
        let indexing =
            extract_for_indexing(&source, ChunkOptions::default()).expect("indexing extraction");

        assert_eq!(indexing.len(), 1);
        assert_eq!(direct, vec![indexing[0].fragment.clone()]);
        assert_eq!(indexing[0].embedding_source, None);
    }
}
