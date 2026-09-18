use super::*;

fn memory_type(minimum: u32, maximum: impl Into<Option<u32>>) -> MemoryType {
    let mut b = MemoryType::builder();
    b.min(u64::from(minimum));
    b.max(maximum.into().map(u64::from));
    b.build().unwrap()
}

#[test]
fn subtyping_works() {
    assert!(memory_type(0, 1).is_subtype_of(&memory_type(0, 1)));
    assert!(memory_type(0, 1).is_subtype_of(&memory_type(0, 2)));
    assert!(!memory_type(0, 2).is_subtype_of(&memory_type(0, 1)));
    assert!(memory_type(2, None).is_subtype_of(&memory_type(1, None)));
    assert!(memory_type(0, None).is_subtype_of(&memory_type(0, None)));
    assert!(memory_type(0, 1).is_subtype_of(&memory_type(0, None)));
    assert!(!memory_type(0, None).is_subtype_of(&memory_type(0, 1)));
}

#[test]
fn dirty_ranges_preserve_writes_across_coalescing_and_synchronization() {
    const PAGE: usize = 65_536;
    let mut memory = Memory::new(memory_type(6, None), &mut Default::default()).unwrap();
    memory.mark_dirty_range(4 * PAGE + 7, 4);
    memory.mark_dirty_range(7, 4);
    memory.mark_dirty_range(9, 8);
    assert_eq!(memory.dirty_ranges, [0..PAGE, 4 * PAGE..5 * PAGE]);
    // Bridge disjoint intervals, including a write spanning a page boundary.
    memory.mark_dirty_range(PAGE - 1, 3 * PAGE + 2);
    assert_eq!(memory.dirty_ranges, [0..5 * PAGE]);
    let generation = memory.data_version();
    for offset in [0, 10, 2 * PAGE, 5 * PAGE - 1] {
        memory.mark_dirty_range(offset, 1);
    }
    assert_eq!(memory.data_version(), generation + 4);
    assert_eq!(memory.take_dirty_ranges(), [0..5 * PAGE]);
    assert!(memory.take_dirty_ranges().is_empty());
    // A synchronized page becomes dirty again on the next write, and a range
    // extending beyond the old end must not take the contained-write shortcut.
    memory.mark_dirty_range(4 * PAGE, 1);
    memory.mark_dirty_range(5 * PAGE - 1, 2);
    assert_eq!(memory.take_dirty_ranges(), [4 * PAGE..6 * PAGE]);
    let generation = memory.data_version();
    for (start, len) in [(0, 0), (6 * PAGE, 1), (6 * PAGE - 1, 2), (usize::MAX, 2)] {
        memory.mark_dirty_range(start, len);
    }
    assert_eq!(memory.data_version(), generation);
    assert!(memory.take_dirty_ranges().is_empty());
}

#[test]
fn dirty_ranges_support_single_byte_pages() {
    let mut ty = MemoryType::builder();
    ty.min(16).page_size_log2(0);
    let mut memory = Memory::new(ty.build().unwrap(), &mut Default::default()).unwrap();
    memory.mark_dirty_range(8, 2);
    memory.mark_dirty_range(8, 1);
    memory.mark_dirty_range(6, 2);
    memory.mark_dirty_range(9, 3);
    assert_eq!(memory.take_dirty_ranges(), [6..12]);
    assert_eq!(memory.data_version(), 4);
}
