//! Frontend session-history stack movement. HTML #traverse-the-history-by-a-delta
//! chooses the destination before mutating history and ignores out-of-range deltas.

pub(crate) fn entry<'a, T>(back: &'a [T], forward: &'a [T], delta: i32) -> Option<&'a T> {
    let source = if delta > 0 { forward } else { back };
    let steps = delta.unsigned_abs() as usize;
    if steps == 0 {
        return None;
    }
    source.get(source.len().checked_sub(steps)?)
}

/// Commit a traversal after its destination is available. Intermediate entries
/// move to the opposite stack in order; they are never loaded along the way.
pub(crate) fn traverse<T>(
    back: &mut Vec<T>,
    forward: &mut Vec<T>,
    current: Option<T>,
    delta: i32,
) -> Option<T> {
    entry(back, forward, delta)?;
    let (source, destination) = if delta > 0 {
        (forward, back)
    } else {
        (back, forward)
    };
    destination.extend(current);
    for _ in 1..delta.unsigned_abs() {
        destination.push(source.pop().expect("validated history range"));
    }
    source.pop()
}

#[cfg(test)]
mod tests {
    #[test]
    fn delta_traversal_preserves_skipped_entries_and_rejects_invalid_bounds() {
        let (mut back, mut forward) = (vec![0, 1, 2], vec![5, 4]);
        for delta in [0, -4, 3, i32::MIN, i32::MAX] {
            assert_eq!(
                super::traverse(&mut back, &mut forward, Some(3), delta),
                None
            );
            assert_eq!(back, [0, 1, 2]);
            assert_eq!(forward, [5, 4]);
        }
        assert_eq!(
            super::traverse(&mut back, &mut forward, Some(3), -2),
            Some(1)
        );
        assert_eq!(back, [0]);
        assert_eq!(forward, [5, 4, 3, 2]);
        assert_eq!(
            super::traverse(&mut back, &mut forward, Some(1), 3),
            Some(4)
        );
        assert_eq!(back, [0, 1, 2, 3]);
        assert_eq!(forward, [5]);
    }
}
