pub(crate) mod base64_bytes;
mod encoding;
mod filesystem;
mod hash;
mod text;

// Text decoding and normalization.
pub(crate) use encoding::{decode_index_text, decode_text};

// Filesystem persistence.
pub(crate) use filesystem::{atomic_write, sync_directory};

// Hashes.
pub(crate) use hash::{sha256_hex, sha256_hex_parts, sha256_parts};

// Text normalization, positions, and slicing.
pub(crate) use text::{
    byte_offset_at_utf16_ceil, byte_offset_at_utf16_floor, collapse_whitespace, line_byte_offsets,
    map_text_range, slice_text, take_utf16, text_range_from_offsets, utf16_len,
};
