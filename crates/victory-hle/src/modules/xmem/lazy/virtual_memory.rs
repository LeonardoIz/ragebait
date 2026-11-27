use super::*;
use windows_sys::Win32::System::{
    Memory as mem,
    Threading::{GetCurrentProcessId, GetProcessId},
};

fn local(process: HANDLE) -> bool {
    // SAFETY: either a pseudo handle or a real handle can name this process.
    unsafe { process == GetCurrentProcess() || GetProcessId(process) == GetCurrentProcessId() }
}

pub(in crate::modules::xmem) extern "system" fn query(
    process: HANDLE,
    at: *const c_void,
    out: *mut MEMORY_BASIC_INFORMATION,
    size: usize,
) -> usize {
    let call = || {
        // SAFETY: the caller supplies a buffer of the given size.
        unsafe { mem::VirtualQueryEx(process, at, out, size) }
    };
    if !enabled() || !local(process) {
        return call();
    }
    let mut table = table();
    if !table.prepare((at as usize, 1)) || !table.prepare((out as usize, size)) {
        return 0;
    }
    call()
}

extern "system" fn VirtualQuery(
    at: *const c_void,
    out: *mut MEMORY_BASIC_INFORMATION,
    size: usize,
) -> usize {
    query(unsafe { GetCurrentProcess() }, at, out, size)
}

extern "system" fn VirtualQueryEx(
    process: HANDLE,
    at: *const c_void,
    out: *mut MEMORY_BASIC_INFORMATION,
    size: usize,
) -> usize {
    query(process, at, out, size)
}

extern "system" fn VirtualProtect(
    at: *const c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> BOOL {
    VirtualProtectEx(unsafe { GetCurrentProcess() }, at, size, protect, old)
}

extern "system" fn VirtualProtectEx(
    process: HANDLE,
    at: *const c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> BOOL {
    let call = || {
        // SAFETY: forwards the caller's address and protection output.
        unsafe { mem::VirtualProtectEx(process, at, size, protect, old) }
    };
    if !local(process) {
        return call();
    }
    let mut table = table();
    if !table.prepare((at as usize, size)) || !table.prepare((old as usize, size_of::<u32>())) {
        return 0;
    }
    call()
}

pub(super) extern "system" fn VirtualFree(at: *mut c_void, size: usize, kind: u32) -> BOOL {
    VirtualFreeEx(unsafe { GetCurrentProcess() }, at, size, kind)
}

extern "system" fn VirtualFreeEx(process: HANDLE, at: *mut c_void, size: usize, kind: u32) -> BOOL {
    mutate(process, at as usize, size, || {
        // SAFETY: forwards the arguments to the host.
        let ok = unsafe { mem::VirtualFreeEx(process, at, size, kind) };
        (ok, ok != 0)
    })
}

extern "system" fn VirtualAlloc2(
    process: HANDLE,
    at: *const c_void,
    size: usize,
    kind: u32,
    protect: u32,
    parameters: *mut MEM_EXTENDED_PARAMETER,
    count: u32,
) -> *mut c_void {
    allocate(process, at as usize, size, || {
        // SAFETY: forwards the arguments to the host.
        unsafe { mem::VirtualAlloc2(process, at, size, kind, protect, parameters, count) }
    })
}

extern "system" fn MapViewOfFile3(
    section: HANDLE,
    process: HANDLE,
    at: *const c_void,
    offset: u64,
    size: usize,
    kind: u32,
    protect: u32,
    parameters: *mut MEM_EXTENDED_PARAMETER,
    count: u32,
) -> MEMORY_MAPPED_VIEW_ADDRESS {
    MEMORY_MAPPED_VIEW_ADDRESS {
        Value: mutate(process, at as usize, size, || {
            // SAFETY: forwards the arguments to the host.
            let ptr = unsafe {
                mem::MapViewOfFile3(
                    section, process, at, offset, size, kind, protect, parameters, count,
                )
                .Value
            };
            (ptr, !ptr.is_null())
        }),
    }
}

pub(in crate::modules::xmem) fn allocate(
    process: HANDLE,
    at: usize,
    size: usize,
    call: impl FnOnce() -> *mut c_void,
) -> *mut c_void {
    if !enabled() {
        return call();
    }
    mutate(process, at, size, || {
        let ptr = call();
        (ptr, !ptr.is_null())
    })
}

/// A page is split out before we defer it, so a size-zero release at
/// another address cannot consume its placeholder. Make an intersecting
/// request see the native view it would have encountered with eager maps.
fn mutate<T: Default>(
    process: HANDLE,
    at: usize,
    size: usize,
    call: impl FnOnce() -> (T, bool),
) -> T {
    if at == 0 || (!process.is_null() && !local(process)) {
        return call().0;
    }
    let mut table = table();
    let len = size.max(1);
    if !table.prepare((at, len)) {
        return T::default();
    }
    let (result, changed) = call();
    if changed {
        let start = at & !(PAGE - 1);
        let end = at.saturating_add(len);
        let forgotten: Vec<_> = table.vacant.range(start..end).copied().collect();
        for page in forgotten {
            table.vacant.remove(&page);
        }
    }
    result
}

pub(in crate::modules) fn module() -> Option<HleModule> {
    enabled().then(|| HleModule {
        name: "lazy page mapping memory operations",
        dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-memory-*"],
        exports: vec![
            hle_export!(VirtualQuery),
            hle_export!(VirtualQueryEx),
            hle_export!(VirtualProtect),
            hle_export!(VirtualProtectEx),
            hle_export!(VirtualFree),
            hle_export!(VirtualFreeEx),
            hle_export!(VirtualAlloc2),
            hle_export!(MapViewOfFile3),
        ],
    })
}
