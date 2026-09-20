mod encoding;
mod filesystem;
mod hash;
mod text;

// Text decoding and normalization.
pub(crate) use encoding::decode_text;
pub(crate) use text::collapse_whitespace;

// Text positions and slicing.
pub(crate) use text::{
    byte_offset_at_utf16_ceil, byte_offset_at_utf16_floor, line_byte_offsets, map_text_range,
    slice_text, take_utf16, text_range_from_offsets, utf16_len,
};

// Filesystem persistence.
pub(crate) use filesystem::{atomic_write, sync_directory};

// Hashes.
pub(crate) use hash::{sha256_hex, sha256_hex_parts};
