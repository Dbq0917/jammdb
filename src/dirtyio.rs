use std::ops::Range;

pub(crate) fn dirty_span(old: &[u8], new: &[u8]) -> Option<Range<usize>> {
    if old.len() != new.len() {
        return Some(0..new.len());
    }

    let first = (0..new.len()).find(|index| old[*index] != new[*index])?;
    let last = (first..new.len())
        .rev()
        .find(|index| old[*index] != new[*index])
        .unwrap_or(first);

    Some(first..last + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_buffers_need_no_write() {
        let buf = vec![7u8; 4096];
        assert_eq!(dirty_span(&buf, &buf), None);
    }

    #[test]
    fn single_changed_byte_is_tiny_span() {
        let old = vec![0u8; 4096];
        let mut new = old.clone();
        new[123] = 9;
        assert_eq!(dirty_span(&old, &new), Some(123..124));
    }

    #[test]
    fn distant_changes_use_one_coalesced_span() {
        let old = vec![0u8; 4096];
        let mut new = old.clone();
        new[10] = 1;
        new[80] = 2;
        assert_eq!(dirty_span(&old, &new), Some(10..81));
    }

    #[test]
    fn different_length_falls_back_to_full_buffer() {
        assert_eq!(dirty_span(&[0, 0], &[1, 0, 0]), Some(0..3));
    }
}
