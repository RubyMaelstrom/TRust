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
#[allow(clippy::single_range_in_vec_init)]
fn dirty_ranges_preserve_writes_across_coalescing_and_synchronization() {
    const PAGE: usize = 65_536;
    let mut memory = Memory::new(memory_type(6, None), &mut crate::ResourceLimiterRef::default()).unwrap();
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
#[allow(clippy::single_range_in_vec_init)]
fn dirty_ranges_support_single_byte_pages() {
    let mut ty = MemoryType::builder();
    ty.min(16).page_size_log2(0);
    let mut memory = Memory::new(ty.build().unwrap(), &mut crate::ResourceLimiterRef::default()).unwrap();
    memory.mark_dirty_range(8, 2);
    memory.mark_dirty_range(8, 1);
    memory.mark_dirty_range(6, 2);
    memory.mark_dirty_range(9, 3);
    assert_eq!(memory.take_dirty_ranges(), [6..12]);
    assert_eq!(memory.data_version(), 4);
}

#[test]
fn shared_bytes_are_identified_with_the_memory() {
    // TRust: WebAssembly JS API #memories identifies Memory.buffer's Data Block with the
    // memory. Sharing must neither copy nor move the bytes, and both owners see every write.
    const PAGE: usize = 65_536;
    let mut limiter = crate::ResourceLimiterRef::default();
    let mut memory = Memory::new(memory_type(1, 4), &mut limiter).unwrap();
    memory.write(10, &[1, 2, 3]).unwrap();
    let ptr = memory.data_ptr();
    // SAFETY: single-threaded, and no slice of `memory` is alive while `shared` is used.
    let shared = unsafe { memory.share() }.unwrap();
    assert!(memory.is_shared());
    assert_eq!(memory.data_ptr(), ptr);
    assert_eq!(&shared.borrow()[10..13], &[1, 2, 3]);
    shared.borrow_mut()[20] = 7;
    assert_eq!(memory.data()[20], 7);
    memory.write(30, &[9]).unwrap();
    assert_eq!(shared.borrow()[30], 9);
    // A shared memory has no mirror, so it records no dirty ranges.
    let version = memory.data_version();
    memory.data_mut()[40] = 1;
    assert_eq!(memory.data_version(), version);
    assert!(memory.take_dirty_ranges().is_empty());
    // Growth resizes the shared Vec; repeated sharing returns the same owner.
    assert_eq!(memory.grow(1, None, &mut limiter).unwrap(), 1);
    assert_eq!(shared.borrow().len(), 2 * PAGE);
    assert_eq!((memory.size(), shared.borrow()[30]), (2, 9));
    // SAFETY: as above.
    let again = unsafe { memory.share() }.unwrap();
    assert!(alloc::rc::Rc::ptr_eq(&shared, &again));
    {
        // A borrow held by the other owner is never aliased: growth fails and the
        // memory exposes no bytes until it is released.
        let _held = shared.borrow_mut();
        assert!(memory.grow(1, None, &mut limiter).is_err());
        assert!(memory.data_mut().is_empty());
    }
    // Accesses follow the live Vec header, even after a foreign resize.
    shared.borrow_mut().truncate(PAGE);
    shared.borrow_mut().shrink_to_fit();
    assert_eq!((memory.size(), memory.data().len()), (1, PAGE));
    assert_eq!(memory.data_ptr(), shared.borrow_mut().as_mut_ptr());
    // The other owner keeps the bytes after the memory is dropped.
    drop(memory);
    assert_eq!(shared.borrow()[30], 9);
}
