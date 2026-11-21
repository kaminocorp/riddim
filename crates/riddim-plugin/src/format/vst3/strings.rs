//! Conversions between Rust strings and the fixed-size character arrays VST3 uses.

use std::ffi::c_char;

use vst3::Steinberg::{char16, TUID};

/// Reads a NUL-terminated 8-bit string out of a fixed array (lossy for non-UTF-8 bytes).
pub fn from_char8(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Reads a NUL-terminated UTF-16 string out of a fixed array.
pub fn from_char16(chars: &[char16]) -> String {
    let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    let units: Vec<u16> = chars[..len].iter().map(|&c| c as u16).collect();
    String::from_utf16_lossy(&units)
}

/// Writes `src` into a fixed UTF-16 array, truncating and always NUL-terminating.
pub fn to_char16(src: &str, dst: &mut [char16]) {
    if dst.is_empty() {
        return;
    }
    let mut n = 0;
    for unit in src.encode_utf16() {
        if n + 1 >= dst.len() {
            break;
        }
        dst[n] = unit as char16;
        n += 1;
    }
    dst[n] = 0;
}

/// The canonical text form of a class id: 32 upper-case hex digits, the same on every platform.
pub fn tuid_to_string(tuid: &TUID) -> String {
    let b = canonical_bytes(tuid);
    let mut s = String::with_capacity(32);
    for byte in b {
        s.push_str(&format!("{byte:02X}"));
    }
    s
}

/// Parses the text form produced by [`tuid_to_string`].
#[cfg(test)]
pub fn tuid_from_string(s: &str) -> Option<TUID> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    let bytes = canonical_bytes_inverse(bytes);
    let mut tuid: TUID = [0; 16];
    for (dst, src) in tuid.iter_mut().zip(bytes) {
        *dst = src as c_char;
    }
    Some(tuid)
}

// On Windows a TUID is laid out like a COM GUID (the first three fields little-endian); elsewhere
// it is plain big-endian. Steinberg's text form is the big-endian one, so undo the swap there.
#[cfg(target_os = "windows")]
fn swap_guid(mut b: [u8; 16]) -> [u8; 16] {
    b.swap(0, 3);
    b.swap(1, 2);
    b.swap(4, 5);
    b.swap(6, 7);
    b
}

fn canonical_bytes(tuid: &TUID) -> [u8; 16] {
    let mut b = [0u8; 16];
    for (dst, src) in b.iter_mut().zip(tuid) {
        *dst = *src as u8;
    }
    #[cfg(target_os = "windows")]
    let b = swap_guid(b);
    b
}

#[cfg(test)]
fn canonical_bytes_inverse(b: [u8; 16]) -> [u8; 16] {
    #[cfg(target_os = "windows")]
    let b = swap_guid(b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuid_text_round_trip() {
        let tuid = vst3::uid(0x6E332252, 0x54224A00, 0xAA69301A, 0xF318797D);
        let text = tuid_to_string(&tuid);
        assert_eq!(text, "6E33225254224A00AA69301AF318797D");
        assert_eq!(tuid_from_string(&text).unwrap(), tuid);
        assert!(tuid_from_string("xyz").is_none());
    }

    #[test]
    fn char16_round_trip_and_truncation() {
        let mut buf = [0 as char16; 8];
        to_char16("Riddim engine", &mut buf);
        assert_eq!(from_char16(&buf), "Riddim ");
        to_char16("ok", &mut buf);
        assert_eq!(from_char16(&buf), "ok");
    }
}
