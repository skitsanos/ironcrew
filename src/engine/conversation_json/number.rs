//! JSON-number scanning used by the allocation-free structural preflight.

pub(super) fn scan(bytes: &[u8], mut index: usize) -> Option<usize> {
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => index = consume_digits(bytes, index),
        _ => return None,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        if !bytes.get(index).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        index = consume_digits(bytes, index);
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if !bytes.get(index).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        index = consume_digits(bytes, index);
    }
    Some(index)
}

fn consume_digits(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    index
}
