//! Minimal protobuf wire-format reader shared by the .apkg parsers
//! (media manifests, notetype configs, deck configs). Handles exactly what
//! untrusted Anki blobs need: varints, length-delimited fields, fixed
//! sizes, packed floats — degrading to "field absent" on malformed input,
//! never panicking.

pub(crate) fn read_varint(bytes: &[u8], i: &mut usize) -> Option<u64> {
    let mut out: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *bytes.get(*i)?;
        *i += 1;
        out |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some(out);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

pub(crate) fn skip_field(bytes: &[u8], i: &mut usize, wire: u64) -> bool {
    match wire {
        0 => read_varint(bytes, i).is_some(),
        1 => advance(bytes, i, 8),
        2 => match read_varint(bytes, i).and_then(|len| usize::try_from(len).ok()) {
            Some(len) => advance(bytes, i, len),
            None => false,
        },
        5 => advance(bytes, i, 4),
        _ => false,
    }
}

/// Moves `i` past `n` bytes when they exist; a length the file chose is
/// added checked, so it can neither wrap nor run past the end.
fn advance(bytes: &[u8], i: &mut usize, n: usize) -> bool {
    match i.checked_add(n) {
        Some(end) if end <= bytes.len() => {
            *i = end;
            true
        }
        _ => false,
    }
}

/// Length-delimited payload (wire type 2) starting at `i`.
pub(crate) fn read_bytes<'a>(bytes: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    let len = read_varint(bytes, i)? as usize;
    let end = i.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    let out = &bytes[*i..end];
    *i = end;
    Some(out)
}

/// Fixed 32-bit little-endian float (wire type 5) starting at `i`.
pub(crate) fn read_f32(bytes: &[u8], i: &mut usize) -> Option<f32> {
    let end = i.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[*i..end]);
    *i = end;
    Some(f32::from_le_bytes(buf))
}

/// A packed `repeated float` payload decoded to a Vec.
pub(crate) fn packed_floats(payload: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(payload.len() / 4);
    let mut i = 0;
    while let Some(v) = read_f32(payload, &mut i) {
        out.push(v);
    }
    out
}
