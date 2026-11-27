//! Only GPU page aliases may be logical mappings without a native view.
//! Each deferred alias owns a single-page placeholder and an already
//! committed backing. Physical still records all logical mappings, so
//! GPU in-place access and page-list DirectStorage writes use the same
//! section and alias bookkeeping as eager views.
//!
//! Remaps acquire the physical, GPU remapping, guard and table locks in
//! that order. CPU faults and leaf kernel memory operations need only the
//! table. Removing a native view and publishing its replacement is one
//! table operation. An untouched alias can be replaced without an OS unmap.

use super::*;
use std::collections::BTreeSet;
use victory_gpu::framelog::{T, add};
use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{VirtualProtect, VirtualQuery};

/// A logical mapping and whether Windows has a view of it yet.
struct Entry {
    slice: Slice,
    native: bool,
}

#[derive(Default)]
struct Table {
    entries: BTreeMap<usize, Entry>,
    /// Single-page placeholders returned by our own unmaps or splits.
    vacant: BTreeSet<usize>,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(Default::default);

fn table() -> MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(|err| err.into_inner())
}

/// VICTORY_REMAP_LAZY=1 (off, read once) defers GPU page aliases. The
/// 64 KiB views used in this mode keep each page independently replaceable.
pub(crate) fn enabled() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| {
        if !std::env::var("VICTORY_REMAP_LAZY").is_ok_and(|v| v.trim() == "1") {
            return false;
        }
        if !install() {
            tracing::warn!("VICTORY_REMAP_LAZY: could not install page fault handler");
            return false;
        }
        victory_gpu::use_remap_views(with_views);
        true
    });
    *ON
}

fn install() -> bool {
    static INSTALLED: LazyLock<bool> = LazyLock::new(|| {
        // SAFETY: the callback and its table live for the process lifetime.
        !unsafe { AddVectoredExceptionHandler(1, Some(fault)) }.is_null()
    });
    *INSTALLED
}

unsafe extern "system" fn fault(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows supplies the exception record during dispatch.
    let record = unsafe { &*(*info).ExceptionRecord };
    if record.ExceptionCode != EXCEPTION_ACCESS_VIOLATION
        || record.NumberParameters < 2
        || record.ExceptionInformation[0] > 1
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let (access, at) = (
        record.ExceptionInformation[0],
        record.ExceptionInformation[1],
    );
    let base = at & !(PAGE - 1);
    let mut table = table();
    let Some(entry) = table.entries.get_mut(&base) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let retry = if entry.native {
        // Another thread may have resolved this fault. Respect any guard
        // or protection installed since then.
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: the query writes into a local structure.
        (unsafe { VirtualQuery(at as _, &mut info, size_of_val(&info)) }) != 0
            && info.State == MEM_COMMIT
            && (info.Protect == PAGE_READWRITE || (access == 0 && info.Protect == PAGE_READONLY))
    } else {
        materialize(base, entry, true)
    };
    if retry {
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}

fn split(at: usize) -> bool {
    // SAFETY: this operation only succeeds on a placeholder.
    unsafe { VirtualFree(at as _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) != 0 }
}

/// Maps exactly the existing single-page placeholder.
/// All deferred entries refer to backing already committed by an eager map.
fn native_map(at: usize, slice: &Slice) -> bool {
    // SAFETY: this section stays alive under the table lock and contains the slice.
    let view = unsafe {
        MapViewOfFile3(
            slice.section.handle,
            GetCurrentProcess(),
            at as _,
            (slice.index * PAGE) as u64,
            PAGE,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    view.Value as usize == at
}

fn native_unmap(at: usize) -> bool {
    let start = std::time::Instant::now();
    // SAFETY: the table serializes changes to this single-page view.
    let ok = unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: at as _ },
            MEM_PRESERVE_PLACEHOLDER,
        ) != 0
    };
    add(T::RemapUnmapUs, start.elapsed().as_micros() as u64);
    add(T::RemapUnmaps, 1);
    ok
}

fn materialize(at: usize, entry: &mut Entry, fault: bool) -> bool {
    if entry.native {
        return true;
    }
    let start = std::time::Instant::now();
    entry.native = native_map(at, &entry.slice);
    add(T::RemapLazyUs, start.elapsed().as_micros() as u64);
    if entry.native {
        add(T::RemapLazyViews, 1);
        if fault {
            add(T::RemapLazyFaults, 1);
        }
    } else {
        add(T::RemapLazyFailures, 1);
    }
    entry.native
}

impl Table {
    fn prepare(&mut self, range: (usize, usize)) -> bool {
        self.resolve(range, false)
    }

    fn resolve(&mut self, (at, len): (usize, usize), fault: bool) -> bool {
        len == 0
            || self
                .entries
                .range_mut((at & !(PAGE - 1))..at.saturating_add(len))
                .all(|(&at, entry)| materialize(at, entry, fault))
    }

    fn replace(&mut self, at: usize, slice: Option<Slice>, defer: bool) -> bool {
        let old = self.entries.get(&at);
        if let (Some(old), Some(slice)) = (old, &slice)
            && (defer || old.native)
            && remap_fast()
            && old.slice.key(0) == slice.key(0)
        {
            if old.native {
                let mut previous = 0;
                // SAFETY: the entry owns a committed view at this address.
                if unsafe { VirtualProtect(at as _, PAGE, PAGE_READWRITE, &mut previous) } == 0 {
                    return false;
                }
            }
            add(T::RemapSkips, 1);
            return true;
        }
        if let Some(old) = old {
            if old.native {
                if !native_unmap(at) {
                    return false;
                }
            } else {
                add(T::RemapLazyDiscarded, 1);
            }
            self.entries.remove(&at);
            self.vacant.insert(at);
        }
        let Some(slice) = slice else { return true };
        let start = std::time::Instant::now();
        let vacant = self.vacant.contains(&at) || split(at);
        if defer && vacant {
            self.vacant.remove(&at);
            self.entries.insert(
                at,
                Entry {
                    slice,
                    native: false,
                },
            );
            add(T::RemapLazyDeferred, 1);
            return true;
        }
        if !native_map(at, &slice) {
            return false;
        }
        let mapped = std::time::Instant::now();
        add(T::RemapMapUs, (mapped - start).as_micros() as u64);
        add(T::RemapViews, 1);
        add(T::RemapPages, 1);
        if !defer && !commit(at, PAGE) {
            if native_unmap(at) {
                self.vacant.insert(at);
            }
            return false;
        }
        add(T::RemapCommitUs, mapped.elapsed().as_micros() as u64);
        self.vacant.remove(&at);
        self.entries.insert(
            at,
            Entry {
                slice,
                native: true,
            },
        );
        true
    }
}

fn with_views(read: (usize, usize), write: (usize, usize), fault: bool, run: &mut dyn FnMut()) {
    // No GPU notifications are issued with this lock held. The supplied
    // operation is a leaf Windows call, which cannot invoke the VEH.
    let mut table = table();
    if table.resolve(read, fault) && table.resolve(write, fault) {
        run();
    }
}

pub(crate) fn prepare(at: usize, len: usize) -> bool {
    !enabled() || table().prepare((at, len))
}

fn replace(p: &mut Physical, at: usize, slice: Option<Slice>, defer: bool) -> bool {
    // MAPPING excludes in-place readers and readback tickets. The table
    // serializes a native fault or kernel access with the OS mapping change.
    let (ok, current) = victory_gpu::title_memory_changing(at, PAGE, || {
        let mut table = table();
        let ok = table.replace(at, slice, defer);
        let current = table.entries.get(&at).map(|entry| entry.slice.clone());
        let changed = ok || current.is_none();
        ((ok, current), changed)
    });
    if let Some((_, old)) = p.views.remove(&at) {
        let key = old.key(0);
        if let Some(addresses) = p.shown.get_mut(&key) {
            addresses.retain(|&other| other != at);
            if addresses.is_empty() {
                p.shown.remove(&key);
            }
        }
    }
    if let Some(slice) = current {
        victory_gpu::title_memory_view(
            at,
            PAGE,
            slice.section.handle as usize,
            (slice.index * PAGE) as u64,
        );
        p.shown.entry(slice.key(0)).or_default().push(at);
        p.views.insert(at, (PAGE, slice));
    }
    ok
}

fn clear(p: &mut Physical, at: usize, len: usize) -> bool {
    views_in(p, at, len)
        .into_iter()
        .all(|page| replace(p, page, None, false))
}

pub(super) fn map_pages(p: &mut Physical, base: usize, n: usize, pages: *const usize) -> bool {
    if !base.is_multiple_of(PAGE)
        || n.checked_mul(PAGE)
            .and_then(|len| base.checked_add(len))
            .is_none()
    {
        return false;
    }
    let _changing = victory_gpu::title_memory_remapping();
    if pages.is_null() {
        return clear(p, base, n * PAGE);
    }
    for i in 0..n {
        let at = base + i * PAGE;
        // SAFETY: this API requires the caller to supply n page numbers.
        let slice = p.pages.get(&unsafe { *pages.add(i) }).cloned();
        if slice.is_none() || !replace(p, at, slice, false) {
            clear(p, at, (n - i) * PAGE);
            return false;
        }
    }
    true
}

pub(super) fn alias(p: &mut Physical, to: usize, from: usize, len: usize) -> bool {
    if ![to, from, len]
        .into_iter()
        .all(|value| value.is_multiple_of(PAGE))
        || to.checked_add(len).is_none()
        || from.checked_add(len).is_none()
    {
        return false;
    }
    let _changing = victory_gpu::title_memory_remapping();
    if from == 0 {
        return clear(p, to, len);
    }
    // Read every source before anything changes, so a source inside `to`
    // (A to A, or a partial overlap) is not lost, as in `alias_in_runs`.
    let wanted: Vec<Slice> = (0..len / PAGE)
        .map_while(|k| slice_at(&p.views, from + k * PAGE))
        .collect();
    for (k, slice) in wanted.iter().enumerate() {
        if !replace(p, to + k * PAGE, Some(slice.clone()), true) {
            clear(p, to + k * PAGE, len - k * PAGE);
            return false;
        }
    }
    let done = wanted.len() * PAGE;
    if done < len {
        clear(p, to + done, len - done);
        return false;
    }
    true
}

mod virtual_memory;
pub(in crate::modules) use virtual_memory::module as virtual_memory_module;
pub(super) use virtual_memory::{allocate, query};

#[cfg(test)]
mod tests;
