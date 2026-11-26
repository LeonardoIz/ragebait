//! Substitute a scaled colour copy for the console FidelityFX upscaler.
#![allow(non_snake_case)]

use crate::console::{self, Console};
use std::ffi::c_void;
use std::sync::OnceLock;
use victory_gpu::object::Object;

const DLL: &str = "amd_fidelityfx_upscaler_xbox.dll";
const VERSION: u64 = 0x0100_1001;
const OK: u32 = 0;
const UNKNOWN_TYPE: u32 = 2;
const RUNTIME_ERROR: u32 = 3;
const NO_PROVIDER: u32 = 4;
const PARAMETER: u32 = 6;
const CREATE: u64 = 0x1_0000;
const UPSCALE: u64 = 0x1_0001;

fn setting(value: Option<&str>, console: Console) -> (bool, &'static str) {
    match value {
        Some("1") => (true, "VICTORY_FFX_PASSTHROUGH=1"),
        Some("0") => (false, "VICTORY_FFX_PASSTHROUGH=0"),
        _ => (console == Console::SeriesS, "console default"),
    }
}

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // VICTORY_FFX_PASSTHROUGH=1 enables the blit, =0 runs the console
        // DLL. Unset defaults to on for Series S and off for Series X.
        let console = console::current();
        let (enabled, reason) = setting(
            std::env::var("VICTORY_FFX_PASSTHROUGH").ok().as_deref(),
            console,
        );
        tracing::info!(
            enabled,
            reason,
            console = console.name(),
            "FidelityFX passthrough"
        );
        enabled
    })
}

pub(crate) fn lookup(dll: &str, symbol: &str) -> Option<usize> {
    if !dll.eq_ignore_ascii_case(DLL) || !enabled() {
        return None;
    }
    Some(match symbol {
        "ffxConfigure" | "#1" => ffxConfigure as *const (),
        "ffxCreateContext" | "#2" => ffxCreateContext as *const (),
        "ffxDestroyContext" | "#3" => ffxDestroyContext as *const (),
        "ffxDispatch" | "#4" => ffxDispatch as *const (),
        "ffxQuery" | "#5" => ffxQuery as *const (),
        _ => return None,
    } as usize)
}

#[repr(C)]
struct Header {
    kind: u64,
    next: *const Header,
}

#[repr(C)]
struct Create {
    header: Header,
    flags: u32,
    max_render: [u32; 2],
    max_output: [u32; 2],
}

#[repr(C)]
struct Resource {
    object: *const Object,
    description: [u32; 8],
    state: u32,
}

// Prefix through upscaleSize. The trailing temporal options are unused.
#[repr(C)]
struct Dispatch {
    header: Header,
    list: *const Object,
    resources: [Resource; 7],
    jitter_and_motion_scale: [f32; 4],
    render: [u32; 2],
    output: [u32; 2],
}

struct Context {
    max_output: [u32; 2],
}

extern "C" fn ffxCreateContext(
    context: *mut *mut Context,
    desc: *const Header,
    _: *const c_void,
) -> u32 {
    if context.is_null() || desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the API supplies a readable header and a writable context slot.
    unsafe {
        if (*desc).kind != CREATE {
            return NO_PROVIDER;
        }
        context.write(Box::into_raw(Box::new(Context {
            max_output: (*desc.cast::<Create>()).max_output,
        })));
    }
    OK
}

extern "C" fn ffxDestroyContext(context: *mut *mut Context, _: *const c_void) -> u32 {
    if context.is_null() {
        return PARAMETER;
    }
    // SAFETY: the slot contains the pointer returned by create, or null.
    let context = unsafe { std::ptr::replace(context, std::ptr::null_mut()) };
    if !context.is_null() {
        // SAFETY: this consumes the context allocation exactly once.
        drop(unsafe { Box::from_raw(context) });
    }
    OK
}

extern "C" fn ffxConfigure(_: *mut *mut Context, _: *const Header) -> u32 {
    OK
}

/// Writes an output through the optional pointer stored in a query.
unsafe fn query_out<T>(desc: *mut Header, offset: usize, value: T) {
    // SAFETY: the query type specifies a pointer to T at this offset.
    let out = unsafe { desc.byte_add(offset).cast::<*mut T>().read() };
    if !out.is_null() {
        // SAFETY: writable output supplied by the caller.
        unsafe { out.write(value) };
    }
}

extern "C" fn ffxQuery(_: *mut *mut Context, desc: *mut Header) -> u32 {
    if desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the query header identifies the layout of the in/out fields.
    // shortcut: report one spatial sample and zero history allocation;
    // implement more queries if a title needs their outputs.
    unsafe {
        match (*desc).kind {
            4 => {
                let create_kind = desc.byte_add(0x10).cast::<u64>().read();
                let count = desc.byte_add(0x20).cast::<*mut u64>().read();
                if count.is_null() {
                    return PARAMETER;
                }
                if create_kind != CREATE {
                    count.write(0);
                } else {
                    if count.read() != 0 {
                        query_out(desc, 0x28, VERSION);
                        query_out(desc, 0x30, c"VIctory FidelityFX passthrough".as_ptr());
                    }
                    count.write(1);
                }
            }
            6 => {
                desc.byte_add(0x10).cast::<u64>().write(VERSION);
                desc.byte_add(0x18)
                    .cast::<*const std::ffi::c_char>()
                    .write(c"VIctory FidelityFX passthrough".as_ptr());
            }
            0x1_0002 => {
                let Some(ratio) = quality_ratio(desc.byte_add(0x10).cast::<u32>().read()) else {
                    return PARAMETER;
                };
                query_out(desc, 0x18, ratio);
            }
            0x1_0003 => {
                let [width, height, mode] = desc.byte_add(0x10).cast::<[u32; 3]>().read();
                let Some(ratio) = quality_ratio(mode) else {
                    return PARAMETER;
                };
                query_out(desc, 0x20, (width as f32 / ratio) as u32);
                query_out(desc, 0x28, (height as f32 / ratio) as u32);
            }
            0x1_0004 => query_out(desc, 0x18, 1i32),
            0x1_0005 => {
                query_out(desc, 0x18, 0.0f32);
                query_out(desc, 0x20, 0.0f32);
            }
            0x1_0008 => query_out(desc, 0x10, [0u64; 2]),
            0x1_0009 => query_out(desc, 0x30, [0u64; 2]),
            _ => return UNKNOWN_TYPE,
        }
    }
    OK
}

/// Upscale ratio of an FFX quality mode (native AA, quality, balanced,
/// performance, ultra performance), as the real provider reports it.
fn quality_ratio(mode: u32) -> Option<f32> {
    [1.0, 1.5, 1.7, 2.0, 3.0].get(mode as usize).copied()
}

/// FidelityFX's texture states mapped to D3D12_RESOURCE_STATES.
fn host_state(state: u32) -> Option<u32> {
    Some(match state {
        0x1 | 0x80 => 0,
        0x2 => 0x8,
        0x4 => 0x40,
        0x8 => 0x80,
        0xc => 0xc0,
        0x10 => 0x800,
        0x20 => 0x400,
        0x100 => 0x4,
        _ => return None,
    })
}

extern "C" fn ffxDispatch(context: *mut *mut Context, desc: *const Header) -> u32 {
    if context.is_null() || desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the caller supplies a context slot and a typed description.
    let (context, desc) = unsafe { (context.read().as_ref(), &*desc) };
    let Some(context) = context else {
        return PARAMETER;
    };
    if desc.kind != UPSCALE {
        return UNKNOWN_TYPE;
    }
    // SAFETY: the type was checked above.
    let dispatch = unsafe { &*std::ptr::from_ref(desc).cast::<Dispatch>() };
    let [color, .., output] = &dispatch.resources;
    let Some(states) = host_state(color.state).zip(host_state(output.state)) else {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| tracing::warn!(color = color.state, output = output.state, "FidelityFX passthrough: unmapped resource state"));
        return PARAMETER;
    };
    let size = if dispatch.output == [0, 0] {
        context.max_output
    } else {
        dispatch.output
    };
    if victory_gpu::upscale::record(
        dispatch.list,
        color.object,
        output.object,
        dispatch.render,
        size,
        states,
    ) {
        OK
    } else {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| tracing::warn!(render = ?dispatch.render, output = ?size, "FidelityFX passthrough dispatch could not be recorded"));
        RUNTIME_ERROR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn upscale_descriptor_offsets() {
        assert_eq!(size_of::<Header>(), 0x10);
        assert_eq!(offset_of!(Create, max_output), 0x1c);
        assert_eq!(size_of::<Resource>(), 0x30);
        assert_eq!(offset_of!(Resource, state), 0x28);
        assert_eq!(offset_of!(Dispatch, list), 0x10);
        assert_eq!(
            offset_of!(Dispatch, resources) + 6 * size_of::<Resource>(),
            0x138
        );
        assert_eq!(offset_of!(Dispatch, render), 0x178);
        assert_eq!(offset_of!(Dispatch, output), 0x180);

        // A synthetic SDK dispatch written by byte offset, independent of
        // the Rust field declarations. Zero represents unused resources.
        let mut raw = [0u64; 0x1b0 / 8];
        raw[0] = UPSCALE;
        raw[0x40 / 8] = 0xc; // Colour state
        raw[0x160 / 8] = 2; // Output state
        raw[0x178 / 8] = (720 << 32) | 1280;
        raw[0x180 / 8] = (1080 << 32) | 1920;
        // SAFETY: the aligned test bytes initialize the dispatch prefix.
        let args = unsafe { &*raw.as_ptr().cast::<Dispatch>() };
        let [color, .., output] = &args.resources;
        assert_eq!(args.header.kind, UPSCALE);
        assert_eq!(args.render, [1280, 720]);
        assert_eq!(args.output, [1920, 1080]);
        assert_eq!(host_state(color.state), Some(0xc0));
        assert_eq!(host_state(output.state), Some(0x8));
    }

    #[test]
    fn render_resolution_query() {
        let (mut width, mut height) = (0u32, 0u32);
        let mut raw = [0u64; 6];
        raw[0] = 0x1_0003;
        raw[2] = (1080 << 32) | 1920;
        raw[3] = 3; // performance
        raw[4] = &raw mut width as u64;
        raw[5] = &raw mut height as u64;
        assert_eq!(ffxQuery(std::ptr::null_mut(), raw.as_mut_ptr().cast()), OK);
        assert_eq!((width, height), (960, 540));
        raw[3] = 9;
        assert_eq!(ffxQuery(std::ptr::null_mut(), raw.as_mut_ptr().cast()), PARAMETER);
    }

    #[test]
    fn switch_overrides_both_console_defaults() {
        for console in [Console::SeriesS, Console::SeriesX] {
            assert_eq!(setting(None, console).0, console == Console::SeriesS);
            assert!(setting(Some("1"), console).0);
            assert!(!setting(Some("0"), console).0);
        }
    }
}
