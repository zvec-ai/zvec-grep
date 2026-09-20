use crate::domain::{Content, Range};

use super::{ExtractedEntity, ExtractedEntityFragment, ImageSource};

pub(super) fn extract(source: &ImageSource) -> Vec<ExtractedEntity> {
    vec![ExtractedEntity {
        index: 0,
        source_range: Range::Full,
        content: Content::Image(source.content.clone()),
        metadata: None,
        fragments: vec![ExtractedEntityFragment { range: Range::Full }],
    }]
}

#[cfg(test)]
mod tests {
    use crate::domain::{Content, FileFormat, ImageContent, Range};

    use super::super::{
        ChunkOptions, ImageSource, extract as extract_source, extract_for_indexing, test_content,
    };
    use super::extract;
    use crate::extraction::test_metadata;

    fn image_source(data: Vec<u8>) -> ImageSource {
        ImageSource {
            content: ImageContent::new(data, FileFormat::Png).expect("image content"),
        }
    }

    #[test]
    fn preserves_image_bytes_format_and_file_range() {
        let source = image_source(vec![1, 2, 3]);
        let fragments = extract(&source);
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].index(), 0);
        assert_eq!(fragments[0].source_range(), &Range::Full);
        assert_eq!(test_metadata(&fragments[0]), None);
        assert_eq!(test_content(&fragments[0]), Content::Image(source.content));
    }

    #[test]
    fn source_router_and_indexing_preserve_the_image_fragment() {
        let source = image_source(vec![4, 5, 6]);
        let direct = extract_source(&source, ChunkOptions::default()).expect("direct extraction");
        let indexing =
            extract_for_indexing(&source, ChunkOptions::default()).expect("indexing extraction");

        assert_eq!(indexing.len(), 1);
        assert_eq!(direct, indexing);
    }
}
