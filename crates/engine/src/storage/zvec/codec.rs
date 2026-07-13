use zvec_rust::{CollectionSchema, DataType, Doc, FieldSchema, IndexParams, MetricType};

use crate::{CodeSymbolKind, EngineError, FileLocation, FileSegment, Result, SegmentKind};

pub(super) fn schema(dimension: usize) -> Result<CollectionSchema> {
    let fts = IndexParams::fts(None, None, None).map_err(zvec_error)?;
    let vector = IndexParams::hnsw(MetricType::Cosine, 16, 200).map_err(zvec_error)?;
    CollectionSchema::builder("zvec_grep_segments")
        .add_field(FieldSchema::new("id", DataType::String, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("file_id", DataType::String, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("path", DataType::String, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("kind", DataType::Int32, false, 0).map_err(zvec_error)?)
        .add_field(
            FieldSchema::new("location_kind", DataType::Int32, false, 0).map_err(zvec_error)?,
        )
        .add_field(FieldSchema::new("start", DataType::Int64, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("end", DataType::Int64, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("content", DataType::String, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("symbol", DataType::String, false, 0).map_err(zvec_error)?)
        .add_field(FieldSchema::new("symbol_kind", DataType::Int32, false, 0).map_err(zvec_error)?)
        .add_indexed_field("search_text", DataType::String, fts)
        .add_vector_field("embedding", DataType::VectorFp32, dimension as u32, vector)
        .build()
        .map_err(zvec_error)
}

pub(super) fn segment_doc(segment: &FileSegment, vector: &[f32]) -> Result<Doc> {
    let mut doc = Doc::new().map_err(zvec_error)?;
    doc.set_pk(&segment.id);
    doc.add_string("id", &segment.id).map_err(zvec_error)?;
    doc.add_string("file_id", segment.file_id.as_str())
        .map_err(zvec_error)?;
    doc.add_string("path", &segment.relative_path.to_string_lossy())
        .map_err(zvec_error)?;
    doc.add_i32("kind", encode_kind(segment.kind))
        .map_err(zvec_error)?;
    let (location_kind, start, end) = encode_location(&segment.location);
    doc.add_i32("location_kind", location_kind)
        .map_err(zvec_error)?;
    doc.add_i64("start", start as i64).map_err(zvec_error)?;
    doc.add_i64("end", end as i64).map_err(zvec_error)?;
    doc.add_string("content", &segment.text)
        .map_err(zvec_error)?;

    let symbol = segment.symbol.as_ref();
    doc.add_string("symbol", symbol.map_or("", |symbol| symbol.name.as_str()))
        .map_err(zvec_error)?;
    doc.add_i32(
        "symbol_kind",
        symbol.map_or(8, |symbol| encode_symbol_kind(symbol.kind)),
    )
    .map_err(zvec_error)?;
    doc.add_string(
        "search_text",
        &format!(
            "{} {} {}",
            segment.relative_path.display(),
            symbol.map_or("", |symbol| symbol.name.as_str()),
            segment.text
        ),
    )
    .map_err(zvec_error)?;
    doc.add_vector_f32("embedding", vector)
        .map_err(zvec_error)?;
    Ok(doc)
}

fn encode_kind(kind: SegmentKind) -> i32 {
    match kind {
        SegmentKind::Code => 0,
        SegmentKind::Section => 1,
        SegmentKind::Text => 2,
    }
}

fn encode_symbol_kind(kind: CodeSymbolKind) -> i32 {
    match kind {
        CodeSymbolKind::Function => 0,
        CodeSymbolKind::Method => 1,
        CodeSymbolKind::Type => 2,
        CodeSymbolKind::Interface => 3,
        CodeSymbolKind::Enum => 4,
        CodeSymbolKind::Trait => 5,
        CodeSymbolKind::Module => 6,
        CodeSymbolKind::Constant => 7,
        CodeSymbolKind::Other => 8,
    }
}

fn encode_location(location: &FileLocation) -> (i32, usize, usize) {
    match *location {
        FileLocation::Lines { start, end } => (0, start, end),
        FileLocation::Page { number } => (1, number, number),
        FileLocation::WholeFile => (2, 0, 0),
    }
}

fn zvec_error(error: zvec_rust::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}
