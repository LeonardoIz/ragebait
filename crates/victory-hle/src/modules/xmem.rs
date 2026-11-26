//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    // Memory committed for the title: the GPU side's notes that blocks there
    // are uncommitted go (`VICTORY_ABSENT_ONCE`).
    if !at.is_null() && kind & MEM_COMMIT != 0 {
        victory_gpu::title_memory_committed(at as usize, size);
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section {
    handle: HANDLE,
    /// Identifies this section even after its handle or allocation address is reused.
    first_page: usize,
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

impl Slice {
    /// The key in `Physical::shown` of the page `k` pages after this one.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.section) as usize, self.index + k)
    }
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each page shows: by its section and its index there, the
    /// address of that page in every view that shows it now ([`shown_at`];
    /// as upstream's c75fdba keeps it). The title maps pages where the
    /// card reads them and has them filled afterwards by DirectStorage,
    /// which writes through views of our own.
    shown: HashMap<(usize, usize), Vec<usize>>,
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    victory_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let mut physical = physical();
    let first = physical.next_page;
    let section = Arc::new(Section { handle, first_page: first });
    physical.next_page += n * FRAMES;
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        let number = first + index * FRAMES;
        physical.pages.insert(number, slice);
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(number) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View { at, len };
        commit(at.Value as usize, len).then_some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether every page under `[offset, offset + len)` is known (a write that
/// fails with them known is a host failure).
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    runs(pages, offset, len).is_some()
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: View,
    within: usize,
    len: usize,
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        &mut self.view.bytes()[within..within + len]
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it (the GPU side's guards fault only on a write at the
/// guarded address itself). Empty if no view shows them. Byte for byte,
/// not by whole pages: the title packs several small textures into one
/// page and fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let physical = physical();
        for (i, page) in touched.iter().enumerate() {
            let Some(addresses) = physical.pages.get(page).and_then(|slice| physical.shown.get(&slice.key(0))) else {
                continue;
            };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            pieces.extend(addresses.iter().map(|&at| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// Bytes `[offset, offset + len)` of `pages` have been written through a
/// view of our own: the GPU side is told at every title address that
/// shows them (`victory_gpu::title_memory_written`; what it does with it
/// is `VICTORY_PAGE_WRITES`' to say, and it counts either way). Call when
/// the bytes are there, and without the page table's lock ([`shown_at`]
/// takes and leaves it).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    for (at, bytes) in shown_at(pages, offset, len) {
        victory_gpu::title_memory_written(at, bytes, true);
    }
}

/// `VICTORY_REMAP_FAST=1`: a mapping call that would leave a range showing
/// the pages it shows (or empty) skips the unmapping and mapping again and
/// only resets what those would ([`refresh`]); the frame log counts them.
/// Off by default, for A/B runs.
fn remap_fast() -> bool {
    static FAST: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_REMAP_FAST").is_ok_and(|v| v.trim() == "1"));
    *FAST
}

/// The slice the page at `at` shows, if a view holds it.
fn slice_at(views: &BTreeMap<usize, (usize, Slice)>, at: usize) -> Option<Slice> {
    view_at(views, at).map(|(slice, k)| Slice {
        index: slice.index + k,
        section: slice.section.clone(),
    })
}

fn view_at(views: &BTreeMap<usize, (usize, Slice)>, at: usize) -> Option<(&Slice, usize)> {
    views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| at < *base + size)
        .map(|(base, (_, slice))| (slice, (at - base) / PAGE))
}

/// Compare the page table around one call so skips and partial failures
/// report the backing actually left at each destination.
fn observe_remap(
    physical: &mut Physical,
    base: usize,
    len: usize,
    remap: impl FnOnce(&mut Physical) -> bool,
) -> bool {
    use victory_gpu::remap_stats;
    if !remap_stats::on() {
        return remap(physical);
    }
    let backing = |slice: &Slice, k| slice.section.first_page + (slice.index + k) * FRAMES;
    let targets = || (0..len).step_by(PAGE).map(|offset| base.wrapping_add(offset));
    let before: Vec<_> = targets()
        .map(|at| view_at(&physical.views, at).map(|(slice, k)| backing(slice, k)))
        .collect();
    let timers = remap_stats::timing();
    let start = std::time::Instant::now();
    let ok = remap(physical);
    let elapsed = start.elapsed().as_micros() as u64;
    let end = remap_stats::timing();
    let [unmap, map, commit, wait] = std::array::from_fn(|i| end[i].wrapping_sub(timers[i]));
    let book = elapsed.saturating_sub(unmap + map + commit + wait);
    let pages = targets().zip(before).map(|(at, before)| {
        let view = view_at(&physical.views, at);
        let after = view.map(|(slice, k)| backing(slice, k));
        let aliased = before != after
            && view
                .and_then(|(slice, k)| physical.shown.get(&slice.key(k)))
                .is_some_and(|addresses| addresses.iter().any(|&other| other != at));
        (at, before, after, aliased)
    });
    remap_stats::record(
        base,
        base.wrapping_add(len),
        len.div_ceil(PAGE),
        ok,
        [unmap, map, commit, wait, book],
        pages,
    );
    ok
}

/// Whether the pages from `base` on already show `wanted`, page for page
/// ([`Slice::key`], `None` for unmapped).
fn shows(views: &BTreeMap<usize, (usize, Slice)>, base: usize, wanted: &[Option<(usize, usize)>]) -> bool {
    wanted
        .iter()
        .enumerate()
        .all(|(k, w)| slice_at(views, base + k * PAGE).map(|s| s.key(0)) == *w)
}

/// [`XMemMapPhysicalPages`] with `VICTORY_REMAP_FAST`: if `[base, ..)`
/// already shows `pages` (or is empty, null `pages`), [`refresh`] it and
/// say so.
fn same_pages(physical: &Physical, base: usize, n: usize, pages: *const usize) -> bool {
    let wanted: Option<Vec<_>> = if pages.is_null() {
        Some(vec![None; n])
    } else {
        // SAFETY: the title passes `n` page numbers.
        let pages = unsafe { std::slice::from_raw_parts(pages, n) };
        pages.iter().map(|p| physical.pages.get(p).map(|s| Some(s.key(0)))).collect()
    };
    let same = wanted.is_some_and(|w| shows(&physical.views, base, &w));
    if same {
        refresh(physical, base, n * PAGE);
    }
    same
}

/// [`map_alias`] with `VICTORY_REMAP_FAST`: if `to` already shows what it
/// would (or is empty, `from` 0), [`refresh`] it and say so. A source
/// overlapping `to` is lost to the unmapping: never the same.
fn same_alias(physical: &Physical, to: usize, from: usize, len: usize) -> bool {
    if from != 0 && from < to + len && to < from + len {
        return false;
    }
    let wanted: Option<Vec<_>> = (0..len)
        .step_by(PAGE)
        .map(|k| match from {
            0 => Some(None),
            _ => slice_at(&physical.views, from + k).map(|s| Some(s.key(0))),
        })
        .collect();
    let same = wanted.is_some_and(|w| shows(&physical.views, to, &w));
    if same {
        refresh(physical, to, len);
    }
    same
}

/// A remapping of `[base, base + len)` to what it shows already: no
/// unmapping and no mapping again, but what the title or the GPU side would
/// see those reset is reset, over every view they would take away and map
/// again whole: read-write again, and the GPU side told it is remapped
/// (guards, texture copy-backs, descriptors) and what it shows. Nothing
/// mapped there: nothing to reset, no lock.
fn refresh(physical: &Physical, base: usize, len: usize) {
    victory_gpu::framelog::add(victory_gpu::framelog::T::RemapSkips, 1);
    let hit = views_in(physical, base, len);
    if hit.is_empty() {
        return;
    }
    // As for a remapping: the GPU side's guards change with no read in
    // place under way.
    let _changing = victory_gpu::title_memory_remapping();
    reset(physical, hit);
}

/// [`refresh`]'s work on the views `hit`, under a remapping lock the caller
/// holds.
fn reset(physical: &Physical, hit: Vec<usize>) {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    for at in hit {
        let (size, slice) = &physical.views[&at];
        let mut old = 0;
        // SAFETY: a committed view of ours.
        unsafe { VirtualProtect(at as *const c_void, *size, PAGE_READWRITE, &mut old) };
        victory_gpu::title_memory_remapped(at, *size);
        victory_gpu::title_memory_view(at, *size, slice.section.handle as usize, (slice.index * PAGE) as u64);
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    if observe_remap(&mut physical(), base, n * PAGE, |physical| map_pages(physical, base, n, pages)) {
        address
    } else {
        std::ptr::null_mut()
    }
}

fn map_pages(physical: &mut Physical, base: usize, n: usize, pages: *const usize) -> bool {
    if remap_fast() && same_pages(physical, base, n, pages) {
        return true;
    }
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`victory_gpu::title_memory_remapping`).
    let _changing = victory_gpu::title_memory_remapping();
    unmap(physical, base, n * PAGE);
    if pages.is_null() {
        return true;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return false;
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        let mapped = if map_runs() {
            map_run(physical, base + i * PAGE, run, &first)
        } else {
            map(physical, base + i * PAGE, run * PAGE, first)
        };
        if !mapped {
            return false;
        }
        i += run;
    }
    true
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page, or in runs ([`map_runs`]); false if a source page is
/// not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    observe_remap(&mut physical(), to, len, |physical| remap_alias(physical, to, from, len))
}

fn remap_alias(physical: &mut Physical, to: usize, from: usize, len: usize) -> bool {
    // `VICTORY_MAP_RUNS` leaves every page alone that shows what it is to
    // show already, which covers `VICTORY_REMAP_FAST`'s whole-call skip here.
    if remap_fast() && !map_runs() && same_alias(physical, to, from, len) {
        return true;
    }
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = victory_gpu::title_memory_remapping();
    if map_runs() {
        return alias_in_runs(physical, to, from, len);
    }
    unmap(physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let Some(slice) = slice_at(&physical.views, from + k) else {
            return false;
        };
        if !map(physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// `VICTORY_MAP_RUNS=1`: [`map_alias`] maps adjacent pages that show
/// consecutive slices of one section as one view, and leaves pages alone
/// that show what they are to show already (only [`reset`] as a remapping
/// would leave them); [`XMemMapPhysicalPages`] joins the placeholders under
/// a run first ([`map_run`]). Off by default, for A/B runs (after cgbj
/// 4180818, where it is on): the title's page-mapping copies cost about
/// 0.35 ms a page, one view each.
fn map_runs() -> bool {
    static RUNS: LazyLock<bool> = LazyLock::new(|| std::env::var("VICTORY_MAP_RUNS").is_ok_and(|v| v.trim() == "1"));
    *RUNS
}

/// The pages of a [`map_alias`] in runs `(first, end, same)`: pages that
/// show what they are to show already (`same`), and runs of pages to map as
/// one view, which show consecutive slices of one section (`wanted`, by
/// [`Slice::key`]).
fn alias_plan(wanted: &[(usize, usize)], same: &[bool]) -> Vec<(usize, usize, bool)> {
    let follows = |k: usize| wanted[k].0 == wanted[k - 1].0 && wanted[k].1 == wanted[k - 1].1 + 1;
    let mut plan = Vec::new();
    let mut first = 0;
    while first < wanted.len() {
        let mut end = first + 1;
        while end < wanted.len() && same[end] == same[first] && (same[first] || follows(end)) {
            end += 1;
        }
        plan.push((first, end, same[first]));
        first = end;
    }
    plan
}

/// [`map_alias`] in runs ([`map_runs`]), under the caller's remapping lock.
fn alias_in_runs(physical: &mut Physical, to: usize, from: usize, len: usize) -> bool {
    if from == 0 {
        unmap(physical, to, len);
        return true;
    }
    let pages = len.div_ceil(PAGE);
    // What is to be shown, up to the first page with no source; read before
    // anything changes, so a source inside `to` is not lost.
    let wanted: Vec<Slice> = (0..pages).map_while(|k| slice_at(&physical.views, from + k * PAGE)).collect();
    let keys: Vec<_> = wanted.iter().map(|s| s.key(0)).collect();
    let same: Vec<bool> = keys
        .iter()
        .enumerate()
        .map(|(k, w)| slice_at(&physical.views, to + k * PAGE).is_some_and(|s| s.key(0) == *w))
        .collect();
    for (first, end, same) in alias_plan(&keys, &same) {
        let (at, bytes) = (to + first * PAGE, (end - first) * PAGE);
        if same {
            victory_gpu::framelog::add(victory_gpu::framelog::T::RemapSkips, 1);
            reset_in(physical, at, bytes);
            continue;
        }
        unmap(physical, at, bytes);
        if !map_run(physical, at, end - first, &wanted[first]) {
            // The caller reports success anyway (`CopyPageMappingsBatchX`): what
            // was not done yet shows nothing rather than what it showed before.
            unmap(physical, at, to + len - at);
            return false;
        }
    }
    if wanted.len() < pages {
        unmap(physical, to + wanted.len() * PAGE, len - wanted.len() * PAGE);
        return false;
    }
    true
}

/// [`reset`] of `[base, base + len)` only: the views there stay mapped
/// whole, and what they show outside the range is left as it is (a texture
/// copy-back queued there stays good).
fn reset_in(physical: &Physical, base: usize, len: usize) {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    for at in views_in(physical, base, len) {
        let (size, slice) = &physical.views[&at];
        let (from, to) = (at.max(base), (at + size).min(base + len));
        let mut old = 0;
        // SAFETY: committed pages of a view of ours.
        unsafe { VirtualProtect(from as *const c_void, to - from, PAGE_READWRITE, &mut old) };
        let offset = (slice.index * PAGE) as u64;
        victory_gpu::title_memory_refreshed(at, *size, slice.section.handle as usize, offset, from, to - from);
    }
}

/// Maps `pages` slices from `slice` on at `base`, where only placeholders
/// are (several, after views of a page each went): the placeholders made
/// one, then one view; page by page if they cannot be made one.
fn map_run(physical: &mut Physical, base: usize, pages: usize, slice: &Slice) -> bool {
    const MEM_COALESCE_PLACEHOLDERS: u32 = 1;
    if pages > 1 {
        let end = base + pages * PAGE;
        // SAFETY: placeholders only. Splitting off the first and the last
        // page puts placeholder edges at both ends of the run (failing
        // harmlessly where one is already); the inner edges are joined.
        let one = unsafe {
            VirtualFree(base as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            VirtualFree((end - PAGE) as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            VirtualFree(base as *mut c_void, pages * PAGE, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS) != 0
        };
        if one {
            return map(physical, base, pages * PAGE, slice.clone());
        }
    }
    (0..pages).all(|k| {
        let slice = Slice {
            index: slice.index + k,
            section: slice.section.clone(),
        };
        map(physical, base + k * PAGE, PAGE, slice)
    })
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    use victory_gpu::framelog::{T, add};
    let started = std::time::Instant::now();
    // SAFETY: splits the placeholder around the range (failing harmlessly
    // when the range is the whole placeholder), then maps the section
    // there; both act only on placeholders.
    let view = unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        MapViewOfFile3(
            slice.section.handle,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    let mapped = std::time::Instant::now();
    add(T::RemapMapUs, (mapped - started).as_micros() as u64);
    add(T::RemapViews, 1);
    add(T::RemapPages, len.div_ceil(PAGE) as u64);
    if !commit(base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    let committed = std::time::Instant::now();
    add(T::RemapCommitUs, (committed - mapped).as_micros() as u64);
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(base, len, slice.section.handle as usize, (slice.index * PAGE) as u64);
    add(T::RemapNotesUs, committed.elapsed().as_micros() as u64);
    for k in 0..len.div_ceil(PAGE) {
        physical.shown.entry(slice.key(k)).or_default().push(base + k * PAGE);
    }
    physical.views.insert(base, (len, slice));
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(len, &error, queried.then_some(&info)));
    } else {
        tracing::warn!(len, %error, "committing physical pages failed");
    }
    false
}

fn commit_limit_message(
    len: usize,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of physical pages: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// The views that overlap `[base, base + len)`, by address.
fn views_in(physical: &Physical, base: usize, len: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    first
        .into_iter()
        .chain(physical.views.range(base + 1..base + len).map(|(at, _)| *at))
        .collect()
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    for at in views_in(physical, base, len) {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        for k in 0..size.div_ceil(PAGE) {
            let key = slice.key(k);
            if let Some(addresses) = physical.shown.get_mut(&key) {
                addresses.retain(|&a| a != at + k * PAGE);
                if addresses.is_empty() {
                    physical.shown.remove(&key);
                }
            }
        }
        use victory_gpu::framelog::{T, add};
        let started = std::time::Instant::now();
        victory_gpu::title_memory_remapped(at, size);
        let noted = std::time::Instant::now();
        add(T::RemapNotesUs, (noted - started).as_micros() as u64);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        add(T::RemapUnmapUs, noted.elapsed().as_micros() as u64);
        // What of the view lies outside the range asked for is mapped
        // again below: the whole view went for it.
        let outside = base.saturating_sub(at) + (at + size).saturating_sub(end);
        if outside != 0 {
            add(T::RemapCuts, 1);
            add(T::RemapCutBytes, outside as u64);
        }
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, &error, Some(&info));
        assert!(report.contains("4194304 bytes"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, &error, None);
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// One reservation of `n` 64 KiB pages for mapping.
    fn reserve(n: usize) -> usize {
        let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!at.is_null());
        at as usize
    }

    /// Scatter reads each entry after doing the ones before it: an entry
    /// that maps over the page holding the list changes those after it.
    #[test]
    fn scatter_reads_each_entry_after_the_ones_before() {
        let pages = allocate(3);
        let (a, b, c, d) = (reserve(1), reserve(1), reserve(1), reserve(1));
        // The addresses as they are at `a` (page 0), and as page 1 has them.
        // (Not the page numbers: their entry's own is read again after its
        // unmapping.)
        let list = |at: usize, second: usize| {
            // SAFETY: `at` is mapped read-write; 2 words.
            unsafe {
                let words = at as *mut usize;
                *words = a;
                *words.add(1) = second;
            }
        };
        assert_eq!(XMemMapPhysicalPages(b as *mut c_void, 1, &pages[1]), b as *mut c_void);
        list(b, d);
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 1, &pages[0]), a as *mut c_void);
        list(a, c);
        // The first entry maps page 1 at `a`: the second is then `d`.
        let numbers = [pages[1], pages[2]];
        assert_eq!(XMemMapPhysicalPagesScatter(a as *const *mut c_void, 2, numbers.as_ptr()), 1);
        assert_eq!(query(c as *const c_void).host.State, MEM_RESERVE);
        assert_eq!(query(d as *const c_void).host.State, MEM_COMMIT);
        for at in [a, b, d] {
            XMemMapPhysicalPages(at as *mut c_void, 1, std::ptr::null());
        }
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A null address fails its scatter entry, and the entries after it
    /// are not done.
    #[test]
    fn scatter_stops_at_a_null_address() {
        let pages = allocate(1);
        let b = reserve(1);
        let addresses = [std::ptr::null_mut(), b as *mut c_void];
        assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, [0, pages[0]].as_ptr()), 0);
        assert_eq!(query(b as *const c_void).host.State, MEM_RESERVE);
        // `VICTORY_REMAP_FAST` finds the null page empty, and the call
        // still returns its (null) address.
        assert!(same_pages(&physical(), 0, 1, std::ptr::null()));
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
    }

    /// `VICTORY_REMAP_FAST`: a remapping to the pages shown already maps
    /// nothing, but leaves its views read-write as a remapping would (and
    /// tells the GPU side, `victory_gpu::title_memory_remapped`).
    #[test]
    fn same_page_remaps_reset_protection() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        let pages = allocate(3);
        let (a, b) = (reserve(2), reserve(1));
        let protect = |at: usize| query(at as *const c_void).host.Protect;
        let read_only = |at: usize, n: usize| {
            let mut old = 0;
            // SAFETY: pages of a view of ours.
            assert_ne!(unsafe { VirtualProtect(at as *const c_void, n * PAGE, PAGE_READONLY, &mut old) }, 0);
        };
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 2, pages.as_ptr()), a as *mut c_void);
        assert!(map_alias(b as u64, (a + PAGE) as u64, PAGE as u64));

        read_only(a, 2);
        assert!(!same_pages(&physical(), a + PAGE, 1, &pages[2]), "other pages");
        assert_eq!(protect(a + PAGE), PAGE_READONLY);
        assert!(same_pages(&physical(), a + PAGE, 1, &pages[1]));
        // The whole view, as a remapping maps its rest again.
        assert_eq!((protect(a), protect(a + PAGE)), (PAGE_READWRITE, PAGE_READWRITE));
        assert_eq!(physical().views.get(&a).map(|v| v.0), Some(2 * PAGE), "the view stays");

        read_only(b, 1);
        assert!(!same_alias(&physical(), a, a + PAGE, 2 * PAGE), "a source in the destination");
        assert!(same_alias(&physical(), b, a + PAGE, PAGE));
        assert_eq!(protect(b), PAGE_READWRITE);

        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 2, std::ptr::null()), a as *mut c_void);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert!(same_pages(&physical(), a, 2, std::ptr::null()), "already empty");
        assert!(same_alias(&physical(), b, 0, PAGE));
        assert_eq!(query(a as *const c_void).host.State, MEM_RESERVE);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// `VICTORY_MAP_RUNS`: which pages of a page-mapping copy form runs, and
    /// which are left alone.
    #[test]
    fn alias_plans_join_consecutive_slices_and_skip_same_pages() {
        let (s, t) = (1, 2);
        // Slices 0, 1, 2 of `s`, then 7 of `s`, then 8 of `t`.
        let wanted = [(s, 0), (s, 1), (s, 2), (s, 7), (t, 8)];
        assert_eq!(alias_plan(&wanted, &[false; 5]), [(0, 3, false), (3, 4, false), (4, 5, false)]);
        // Pages that show theirs already are one run, consecutive or not,
        // and cut the runs to map.
        assert_eq!(
            alias_plan(&wanted, &[false, true, false, true, true]),
            [(0, 1, false), (1, 2, true), (2, 3, false), (3, 5, true)]
        );
        assert_eq!(alias_plan(&wanted, &[true; 5]), [(0, 5, true)]);
        assert!(alias_plan(&[], &[]).is_empty());
    }

    /// `VICTORY_MAP_RUNS` on real views: a run is one view, pages already
    /// right are left alone (but read-write again), placeholders of single
    /// pages are joined, a later unmapping of part of a run keeps the rest,
    /// and a source that ends maps what came before it.
    #[test]
    fn page_mapping_copies_in_runs() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        let runs = |to: usize, from: usize, n: usize| alias_in_runs(&mut physical(), to, from, n * PAGE);
        let view = |at: usize| physical().views.get(&at).map(|v| v.0 / PAGE);
        let pages = allocate(3);
        let (a, b) = (reserve(3), reserve(3));
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 3, pages.as_ptr()), a as *mut c_void);
        // SAFETY: both mapped read-write when called.
        let same = |k: usize, page: usize, value: u8| unsafe {
            ((a + page * PAGE + 64) as *mut u8).write(value);
            assert_eq!(((b + k * PAGE + 64) as *const u8).read(), value, "page {k}");
        };
        assert!(runs(b, a, 3));
        assert_eq!(view(b), Some(3), "one view");
        let mut old = 0;
        // SAFETY: a view of ours.
        assert_ne!(unsafe { VirtualProtect(b as *const c_void, 3 * PAGE, PAGE_READONLY, &mut old) }, 0);
        assert!(runs(b, a, 3));
        assert_eq!(view(b), Some(3), "left alone");
        assert_eq!(query(b as *const c_void).host.Protect, PAGE_READWRITE);
        for k in 0..3 {
            same(k, k, 40 + k as u8);
        }
        // Page 0 in the middle, and back: the pages around it stay.
        assert!(runs(b + PAGE, a, 1));
        same(1, 0, 51);
        same(2, 2, 52);
        assert!(runs(b, a, 3));
        assert_eq!((view(b), view(b + PAGE), view(b + 2 * PAGE)), (Some(1), Some(1), Some(1)));
        same(1, 1, 53);
        // Three placeholders of a page each become one view.
        assert!(runs(b, 0, 3));
        assert_eq!(query(b as *const c_void).host.State, MEM_RESERVE);
        assert!(runs(b, a, 3));
        assert_eq!(view(b), Some(3), "joined");
        // Unmapping the middle of the run keeps its ends.
        assert_eq!(XMemMapPhysicalPages((b + PAGE) as *mut c_void, 1, std::ptr::null()), (b + PAGE) as *mut c_void);
        assert_eq!(query((b + PAGE) as *const c_void).host.State, MEM_RESERVE);
        same(0, 0, 54);
        same(2, 2, 55);
        // A source that ends: what came before it is mapped, the rest not.
        assert_eq!(XMemMapPhysicalPages((a + PAGE) as *mut c_void, 1, std::ptr::null()), (a + PAGE) as *mut c_void);
        assert!(runs(b + PAGE, a + 2 * PAGE, 1));
        assert!(!runs(b, a, 2));
        same(0, 0, 56);
        assert_eq!(query((b + PAGE) as *const c_void).host.State, MEM_RESERVE);
        same(2, 2, 57);
        assert!(runs(b, 0, 3));
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 3, std::ptr::null()), a as *mut c_void);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// `VICTORY_MAP_RUNS`: a page left alone inside a larger view is made
    /// read-write again by itself; the view and its other pages stay as
    /// they are.
    #[test]
    fn a_page_left_alone_resets_only_itself() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        let protect = |at: usize| query(at as *const c_void).host.Protect;
        let pages = allocate(3);
        let (a, b) = (reserve(3), reserve(3));
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 3, pages.as_ptr()), a as *mut c_void);
        assert!(alias_in_runs(&mut physical(), b, a, 3 * PAGE));
        let mut old = 0;
        // SAFETY: a view of ours.
        assert_ne!(unsafe { VirtualProtect(b as *const c_void, 3 * PAGE, PAGE_READONLY, &mut old) }, 0);
        assert!(alias_in_runs(&mut physical(), b, a, PAGE));
        assert_eq!(physical().views.get(&b).map(|v| v.0), Some(3 * PAGE), "the view stays");
        assert_eq!(protect(b), PAGE_READWRITE);
        assert_eq!((protect(b + PAGE), protect(b + 2 * PAGE)), (PAGE_READONLY, PAGE_READONLY));
        assert!(alias_in_runs(&mut physical(), b, 0, 3 * PAGE));
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 3, std::ptr::null()), a as *mut c_void);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// `VICTORY_MAP_RUNS`: a run that fails to map leaves the pages after it
    /// showing nothing, not what they showed before.
    #[test]
    fn a_failed_run_clears_the_runs_after_it() {
        let pages = allocate(3);
        let (a, b) = (reserve(2), reserve(2));
        // Pages 0 and 2 at `a`: two runs. `b + PAGE` shows page 1, stale.
        let source = [pages[0], pages[2]];
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 2, source.as_ptr()), a as *mut c_void);
        assert_eq!(XMemMapPhysicalPages((b + PAGE) as *mut c_void, 1, &pages[1]), (b + PAGE) as *mut c_void);
        // `b` is no placeholder any more, so the first run cannot map there.
        // SAFETY: placeholders of ours: page `b` split off, then given up.
        unsafe {
            VirtualFree(b as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            assert_ne!(VirtualFree(b as *mut c_void, 0, MEM_RELEASE), 0);
        }
        assert!(!alias_in_runs(&mut physical(), b, a, 2 * PAGE));
        assert!(slice_at(&physical().views, b + PAGE).is_none());
        assert_eq!(query((b + PAGE) as *const c_void).host.State, MEM_RESERVE);
        assert_eq!(XMemMapPhysicalPages(a as *mut c_void, 2, std::ptr::null()), a as *mut c_void);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Pages say where the title has them mapped, byte for byte (after
    /// fork PR #571's test; its runs are whole pages).
    #[test]
    fn pages_say_where_they_are_mapped() {
        let pages = allocate(3);
        let reserve = |n: usize| {
            let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            assert!(!at.is_null());
            at
        };
        // `b` keeps a page unmapped at either end, so that no run at `b`
        // ever touches one at `a`, wherever the host puts the two.
        let (a, b) = (reserve(3), reserve(4));
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
        // All three at `a`; the last two at `b`, from its second page on.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        let second = b.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
        let (a, second) = (a as usize, second as usize);
        let sorted = |mut runs: Vec<(usize, usize)>| {
            runs.sort_unstable();
            runs
        };
        assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
        // A request for part of a page names those bytes alone.
        assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
        // Across two pages, shown at two addresses each: one run at each.
        assert_eq!(
            shown_at(&pages, PAGE + 1, PAGE),
            sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
        );
        // A page unmapped in the middle of a view: the rest still shows.
        let middle = (a + PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
        assert_eq!(
            shown_at(&pages, 0, 3 * PAGE),
            sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
        );
        // Too few pages for the bytes: nothing (the write fails too).
        assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
        for (at, n) in [(a, 3), (second, 2)] {
            let at = at as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        }
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }
}
