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
