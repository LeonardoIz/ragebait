use super::*;
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};

struct Memory {
    physical: Physical,
    numbers: [usize; 3],
    base: usize,
}

impl Memory {
    fn new() -> Self {
        assert!(install());
        let mut numbers = [0; 3];
        let mut n = numbers.len();
        // Two sections, so the last page also exercises a backing change
        // across the section cap.
        assert_eq!(
            allocate_physical_pages(0, &mut n, numbers.as_mut_ptr(), 2),
            1
        );
        let p = physical();
        let mut physical = Physical::default();
        physical
            .pages
            .extend(numbers.map(|number| (number, p.pages[&number].clone())));
        drop(p);
        // SAFETY: asks for a new reservation in this process.
        let base = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                8 * PAGE,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        } as usize;
        assert_ne!(base, 0);
        assert!(map_pages(&mut physical, base, n, numbers.as_ptr()));
        for i in 0..n {
            // SAFETY: the test owns the committed source views above.
            unsafe { ((base + i * PAGE) as *mut u8).write_volatile(1 + i as u8) };
        }
        Self {
            physical,
            numbers,
            base,
        }
    }

    fn alias(&mut self, target: usize, source: usize) {
        assert!(alias(
            &mut self.physical,
            self.base + target * PAGE,
            self.base + source * PAGE,
            PAGE
        ));
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        assert!(alias(&mut self.physical, self.base, 0, 8 * PAGE));
        let mut at = self.base;
        while at < self.base + 8 * PAGE {
            let info = query_native(at);
            assert_eq!(virtual_memory::VirtualFree(at as _, 0, MEM_RELEASE), 1);
            at += info.RegionSize;
        }
        assert_eq!(
            XMemFreePhysicalPages(self.numbers.len(), self.numbers.as_ptr()),
            1
        );
    }
}

fn query_native(at: usize) -> MEMORY_BASIC_INFORMATION {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: local output of the correct size.
    assert_ne!(
        unsafe { VirtualQuery(at as _, &mut info, size_of_val(&info)) },
        0
    );
    info
}

#[test]
fn neighbouring_mappings_stay_independent_and_keep_protection() {
    use windows_sys::Win32::System::Memory::PAGE_READONLY;

    let mut memory = Memory::new();
    let at = memory.base + 4 * PAGE;
    // Two distinct mapping calls, with consecutive source slices.
    memory.alias(4, 0);
    memory.alias(5, 1);
    assert_eq!(query_native(at).State, MEM_RESERVE);
    assert_eq!(query_native(at + PAGE).State, MEM_RESERVE);

    // SAFETY: the test owns these logically mapped locations. A CPU read
    // must complete with the bytes of its requested backing.
    unsafe {
        assert_eq!((at as *const u8).read_volatile(), 1);
        assert_eq!(((at + PAGE) as *const u8).read_volatile(), 2);
    }
    let mut old = 0;
    // SAFETY: the adjacent page has a live native view.
    assert_eq!(
        unsafe { VirtualProtect((at + PAGE) as _, PAGE, PAGE_READONLY, &mut old) },
        1
    );
    memory.alias(4, 2);
    assert_eq!(query_native(at).State, MEM_RESERVE);
    assert_eq!(query_native(at + PAGE).Protect, PAGE_READONLY);
    assert_eq!(query_native(at + PAGE).AllocationBase as usize, at + PAGE);
    // SAFETY: both logical mappings are live. Replacing the first did
    // not clear the second or leave the first showing its old bytes.
    unsafe {
        assert_eq!((at as *const u8).read_volatile(), 3);
        assert_eq!(((at + PAGE) as *const u8).read_volatile(), 2);
    }
}

#[test]
fn kernel_io_uses_the_latest_alias_and_storage_writes_follow_backing() {
    let mut memory = Memory::new();
    let at = memory.base + 4 * PAGE;
    memory.alias(4, 0);
    memory.alias(4, 2);
    assert_eq!(query_native(at).State, MEM_RESERVE);

    // The page-list worker writes through an independent view of the
    // backing before a CPU or GPU kernel access has materialized the alias.
    assert!(write_physical(&memory.numbers[2..], 0, &[17]));
    assert_eq!(query_native(at).State, MEM_RESERVE);
    let mut byte = 0u8;
    with_views((at, 1), (0, 0), false, &mut || {
        // SAFETY: kernel checks the source and the stack byte is writable.
        assert_ne!(
            unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    at as _,
                    (&raw mut byte).cast(),
                    1,
                    std::ptr::null_mut(),
                )
            },
            0
        );
    });
    assert_eq!(byte, 17);

    memory.alias(4, 1);
    with_views((0, 0), (at, 1), false, &mut || {
        // SAFETY: kernel checks the destination; the source is one readable byte.
        assert_ne!(
            unsafe {
                WriteProcessMemory(
                    GetCurrentProcess(),
                    at as _,
                    (&raw const byte).cast(),
                    1,
                    std::ptr::null_mut(),
                )
            },
            0
        );
    });
    // SAFETY: both source pages remain mapped and the kernel write returned.
    unsafe {
        assert_eq!(((memory.base + PAGE) as *const u8).read_volatile(), 17);
        assert_eq!((memory.base as *const u8).read_volatile(), 1);
    }
}

#[test]
fn a_vm_release_cannot_free_a_live_lazy_alias() {
    let mut memory = Memory::new();
    let at = memory.base + 4 * PAGE;
    memory.alias(4, 0);
    assert_eq!(query_native(at).State, MEM_RESERVE);
    assert_eq!(virtual_memory::VirtualFree(at as _, 0, MEM_RELEASE), 0);
    assert_eq!(query_native(at).State, MEM_COMMIT);
    // SAFETY: the failed release left the page mapped.
    assert_eq!(unsafe { (at as *const u8).read_volatile() }, 1);
}

#[test]
fn racing_reads_resolve_either_live_backing() {
    let mut memory = Memory::new();
    let at = memory.base + 4 * PAGE;
    memory.alias(4, 0);
    let start = std::sync::Barrier::new(3);
    std::thread::scope(|threads| {
        for _ in 0..2 {
            threads.spawn(|| {
                start.wait();
                for _ in 0..2048 {
                    // SAFETY: the mapper only replaces the logical backing;
                    // the fault handler serializes with each replacement.
                    assert!(matches!(
                        unsafe { (at as *const u8).read_volatile() },
                        1 | 3
                    ));
                }
            });
        }
        start.wait();
        for i in 0..256 {
            memory.alias(4, i % 2 * 2);
        }
    });
}

#[test]
fn overlapping_aliases_read_sources_before_changing_targets() {
    let mut memory = Memory::new();
    let base = memory.base;
    // A to A: the only view of page 0 is its own source.
    memory.alias(0, 0);
    // SAFETY: page 0 stays logically mapped.
    assert_eq!(unsafe { (base as *const u8).read_volatile() }, 1);

    // Partial overlap: pages 1..3 take what pages 0..2 showed.
    assert!(alias(&mut memory.physical, base + PAGE, base, 2 * PAGE));
    // SAFETY: pages 0..3 are logically mapped.
    unsafe {
        assert_eq!((base as *const u8).read_volatile(), 1);
        assert_eq!(((base + PAGE) as *const u8).read_volatile(), 1);
        assert_eq!(((base + 2 * PAGE) as *const u8).read_volatile(), 2);
    }
}
