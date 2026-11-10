//! Shared utilities.

pub(crate) mod base64_bytes;
mod encoding;
mod filesystem;
mod hash;
mod text;

// Encoding.
pub(crate) use encoding::decode_index_text;
pub(crate) use encoding::decode_text;

// Filesystem.
pub(crate) use filesystem::atomic_write;
pub(crate) use filesystem::directory_exists;
pub(crate) use filesystem::ensure_directory;
pub(crate) use filesystem::file_exists;
pub(crate) use filesystem::reject_symlink;
pub(crate) use filesystem::resolve_directory;
pub(crate) use filesystem::sync_directory;

// Hashes.
pub(crate) use hash::sha256_hex;
pub(crate) use hash::sha256_hex_parts;
pub(crate) use hash::sha256_parts;

// Text.
pub(crate) use text::byte_offset_at_utf16_ceil;
pub(crate) use text::byte_offset_at_utf16_floor;
pub(crate) use text::collapse_whitespace;
pub(crate) use text::line_byte_offsets;
pub(crate) use text::map_text_range;
pub(crate) use text::slice_text;
pub(crate) use text::take_utf16;
pub(crate) use text::text_range_from_offsets;
pub(crate) use text::utf16_len;

pub(crate) fn current_username() -> Option<String> {
    let names = if cfg!(windows) {
        ["USERNAME", "USER"]
    } else {
        ["USER", "USERNAME"]
    };
    names.into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}
