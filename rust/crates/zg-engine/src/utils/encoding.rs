use std::borrow::Cow;

/// Decodes UTF-8, or UTF-16/32 with a BOM, without replacing invalid input.
pub(crate) fn decode_text(bytes: &[u8], complete: bool) -> Option<Cow<'_, str>> {
    // UTF-32 LE must precede UTF-16 LE because their BOMs share the first two bytes.
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe\x00\x00") {
        return decode_utf32(body, true, complete).map(Cow::Owned);
    }
    if let Some(body) = bytes.strip_prefix(b"\x00\x00\xfe\xff") {
        return decode_utf32(body, false, complete).map(Cow::Owned);
    }
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe") {
        return decode_utf16(body, true, complete).map(Cow::Owned);
    }
    if let Some(body) = bytes.strip_prefix(b"\xfe\xff") {
        return decode_utf16(body, false, complete).map(Cow::Owned);
    }
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = match std::str::from_utf8(body) {
        Ok(text) => text,
        Err(error) if !complete && error.error_len().is_none() => {
            std::str::from_utf8(&body[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    Some(Cow::Borrowed(text))
}

fn decode_utf16(bytes: &[u8], little_endian: bool, complete: bool) -> Option<String> {
    let (mut units, remainder) = bytes.as_chunks::<2>();
    if complete && !remainder.is_empty() {
        return None;
    }
    let unit = |pair: &[u8; 2]| {
        if little_endian {
            u16::from_le_bytes(*pair)
        } else {
            u16::from_be_bytes(*pair)
        }
    };
    // A sample may end halfway through a surrogate pair as well as a code unit.
    if !complete
        && units
            .last()
            .is_some_and(|pair| (0xd800..=0xdbff).contains(&unit(pair)))
    {
        units = &units[..units.len() - 1];
    }
    char::decode_utf16(units.iter().map(unit))
        .collect::<Result<String, _>>()
        .ok()
}

fn decode_utf32(bytes: &[u8], little_endian: bool, complete: bool) -> Option<String> {
    let (units, remainder) = bytes.as_chunks::<4>();
    if complete && !remainder.is_empty() {
        return None;
    }
    units
        .iter()
        .map(|unit| {
            char::from_u32(if little_endian {
                u32::from_le_bytes(*unit)
            } else {
                u32::from_be_bytes(*unit)
            })
        })
        .collect()
}
