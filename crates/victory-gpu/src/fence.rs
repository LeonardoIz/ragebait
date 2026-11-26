//! Fences: a 64-bit value and the events waiting for it. Nothing runs on a
//! GPU yet, so only signals (the title's, later our queues') move one.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{self, E_NOTIMPL, FENCE, HRESULT, Iid, Object, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

pub(crate) const FENCE_IIDS: [Iid; 4] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Fence {
    object: Object,
    /// Titles also read it straight from memory, at the fence's GPU
    /// address.
    value: AtomicU64,
    flags: u32,
    /// Events to set once the value reaches theirs (handles as integers).
    waiters: Mutex<Vec<(u64, usize)>>,
    /// For [`pace`]: when each wait above was asked for.
    #[cfg(all(windows, feature = "diagnostics"))]
    asked: Mutex<Vec<pace::Asked>>,
}

fn fence<'a>(this: This) -> &'a Fence {
    // SAFETY: called from fence methods only.
    unsafe { &*this.cast::<Fence>() }
}

pub(crate) extern "system" fn create(
    _: This,
    initial: u64,
    flags: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::debug!(initial, flags, "fence");
    let fence = Box::new(Fence {
        object: Object::new(&FUNCTIONS),
        value: AtomicU64::new(initial),
        flags,
        waiters: Mutex::new(Vec::new()),
        #[cfg(all(windows, feature = "diagnostics"))]
        asked: Mutex::new(Vec::new()),
    });
    object::hand_out(FENCE, fence, iid, &FENCE_IIDS, out)
}

fn set_event(event: usize) {
    #[cfg(windows)]
    // SAFETY: the title passed an event handle.
    unsafe {
        windows_sys::Win32::System::Threading::SetEvent(event as *mut c_void)
    };
    #[cfg(not(windows))]
    let _ = event;
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        FENCE,
        iid,
        object::is(iid, &FENCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Fence>(this)
}
pub(crate) extern "system" fn completed(this: This) -> u64 {
    fence(this).value.load(Ordering::Acquire)
}
extern "system" fn set_event_on_completion(this: This, value: u64, event: *mut c_void) -> HRESULT {
    let fence = fence(this);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    if fence.value.load(Ordering::Acquire) >= value {
        if !event.is_null() {
            set_event(event as usize);
        }
        #[cfg(all(windows, feature = "diagnostics"))]
        pace::ready();
        return S_OK;
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    pace::waits(this, &fence.asked, value, fence.value.load(Ordering::Acquire));
    if event.is_null() {
        // A null event means "block until reached" (`VICTORY_FENCE_WAIT`):
        // an event of ours waits with the title's, registered under the
        // lock the value was read under, and the 250 ms count from here.
        #[cfg(windows)]
        let blocking = fence_wait()
            .then(|| latch(&mut waiters, value))
            .flatten()
            .map(|latch| (latch, std::time::Instant::now() + std::time::Duration::from_millis(250)));
        drop(waiters);
        // With submissions sent to the card and not waited for
        // (`crate::host::late`) the signal may be behind them: they are
        // settled, which sets it.
        // shortcut: the settlement waits for the card with no deadline of
        // its own (it runs with the switch off too); it only counts against
        // the 250 ms; give it one if runs show it long.
        #[cfg(windows)]
        if crate::host::late() {
            crate::host::settle_for_fence();
            if blocking.is_none() && fence.value.load(Ordering::Acquire) >= value {
                return S_OK;
            }
        }
        #[cfg(windows)]
        if let Some((latch, deadline)) = blocking {
            if block_until(fence, value, latch, deadline) {
                return S_OK;
            }
            tracing::warn!(value, "blocking fence wait timed out");
            return E_NOTIMPL;
        }
        // shortcut: fails instead of blocking unless `VICTORY_FENCE_WAIT=1`,
        // as a signal may never come; turn it on by default once runs show
        // the waits end.
        tracing::warn!(value, "blocking fence wait not implemented");
        return E_NOTIMPL;
    }
    waiters.push((value, event as usize));
    S_OK
}

/// `VICTORY_FENCE_WAIT=1`: `SetEventOnCompletion` with a null event blocks
/// until the fence reaches the value, 250 ms at most, instead of failing at
/// once (after cgbj e05d463). Off by default: it stalls the title's thread
/// where the call used to return.
#[cfg(windows)]
fn fence_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_FENCE_WAIT").is_ok_and(|v| v.trim() == "1"))
}

/// A manual-reset event of ours for a blocking wait for `value`, among
/// the fence's waiters (whose lock the caller holds): [`set`] sets it as it
/// does the title's, so a wait sees the value reached even if the fence is
/// moved back before it looks.
#[cfg(windows)]
fn latch(waiters: &mut Vec<(u64, usize)>, value: u64) -> Option<usize> {
    // SAFETY: a plain manual-reset event, not signalled.
    let event = unsafe { windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        tracing::warn!("no event for a blocking fence wait");
        return None;
    }
    waiters.push((value, event as usize));
    Some(event as usize)
}

/// Waits until [`set`] reached `value` (`latch`, from [`latch`]), until
/// `deadline` at most, every 10 ms running the queues' held lists if no
/// other thread is at it (a signal may be behind them). Whether it got
/// there; `latch` is gone afterwards.
#[cfg(windows)]
fn block_until(fence: &Fence, value: u64, latch: usize, deadline: std::time::Instant) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let reached = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        // SAFETY: our event, closed only below.
        let set = unsafe { WaitForSingleObject(latch as *mut c_void, (left.as_millis() as u32).min(10)) } == WAIT_OBJECT_0;
        if set || fence.value.load(Ordering::Acquire) >= value {
            break true;
        }
        if left.is_zero() {
            break false;
        }
        crate::queue::try_release_held();
    };
    // Under the lock, so `set` no longer sets it once closed.
    fence.waiters.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| w.1 != latch);
    // SAFETY: ours, no longer among the waiters.
    unsafe { CloseHandle(latch as *mut c_void) };
    reached
}
/// Moves the fence to `value` and sets the events that waited for it.
pub(crate) fn set(this: This, value: u64) {
    let fence = fence(this);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    fence.value.store(value, Ordering::Release);
    #[cfg(all(windows, feature = "diagnostics"))]
    pace::reached(this, &fence.asked, value);
    waiters.retain(|&(wanted, event)| {
        let reached = wanted <= value;
        if reached {
            set_event(event);
        }
        !reached
    });
}
pub(crate) extern "system" fn signal(this: This, value: u64) -> HRESULT {
    set(this, value);
    // A queue may have waited for it: what it held runs now.
    #[cfg(windows)]
    crate::queue::release_held(false);
    S_OK
}

extern "system" fn creation_flags(this: This) -> u32 {
    fence(this).flags
}
/// Where the value lives (GPU addresses are CPU addresses).
extern "system" fn gpu_address(this: This) -> u64 {
    (&raw const fence(this).value) as u64
}

/// How the title paces itself on its fences, measured (diagnostics
/// builds): which value it waits for, how long until our line gets the
/// fence there, and how many frames lie between the title's `Signal` on
/// its queue and the fence being set. Run 233 put presents behind the
/// line and the frame did not get shorter: the title waited somewhere
/// else, and this says for what.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod pace {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::Instant;

    use crate::object::This;

    pub(crate) struct Asked {
        value: u64,
        at: Instant,
        frame: u64,
        thread: u32,
    }

    /// The title's `Signal` calls on a queue not yet at the fence:
    /// fence, value, when, frame.
    static QUEUED: Mutex<Vec<(usize, u64, Instant, u64)>> = Mutex::new(Vec::new());
    /// The thread that presents.
    static RENDER: AtomicU32 = AtomicU32::new(0);
    /// Since the last line: waits already met; waits not met; of those,
    /// the render thread's; reached waits, their microseconds, their
    /// frames between; the render thread's reached waits and microseconds;
    /// signals set, their microseconds since the title's call, their
    /// frames between.
    static N: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
    static SAID: AtomicU64 = AtomicU64::new(0);
    static LINE_AT: AtomicU64 = AtomicU64::new(0);

    /// Whether events are said one by one now: four seconds of every two
    /// minutes of the wall clock, the same seconds in which the loader says
    /// the title's `mwaitx` waits (`console_insn`), to lay the two side by side.
    fn detail_now() -> bool {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).is_ok_and(|d| d.as_secs() % 120 < 4)
    }

    fn thread() -> u32 {
        // SAFETY: a plain query.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
    }

    pub(super) fn ready() {
        N[0].fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn waits(fence: This, asked: &Mutex<Vec<Asked>>, value: u64, now: u64) {
        let thread = thread();
        let render = thread == RENDER.load(Ordering::Relaxed);
        N[1].fetch_add(1, Ordering::Relaxed);
        if render {
            N[2].fetch_add(1, Ordering::Relaxed);
        }
        let frame = crate::queue::presented();
        if detail_now() || SAID.fetch_add(1, Ordering::Relaxed) < 48 {
            tracing::info!(
                "pace: the title waits for fence {fence:?} value {value} (it is at {now}) in frame {frame}, on {}",
                if render { "the render thread".to_string() } else { format!("thread {thread}") }
            );
        }
        let mut asked = asked.lock().unwrap_or_else(|e| e.into_inner());
        if asked.len() < 4096 {
            asked.push(Asked { value, at: Instant::now(), frame, thread });
        }
    }

    /// The title called `Signal` for `fence` on a queue.
    pub(crate) fn signal_called(fence: usize, value: u64) {
        let mut queued = QUEUED.lock().unwrap_or_else(|e| e.into_inner());
        if queued.len() < 4096 {
            queued.push((fence, value, Instant::now(), crate::queue::presented()));
        }
    }

    pub(super) fn reached(fence: This, asked: &Mutex<Vec<Asked>>, value: u64) {
        let frame = crate::queue::presented();
        let render = RENDER.load(Ordering::Relaxed);
        {
            let mut queued = QUEUED.lock().unwrap_or_else(|e| e.into_inner());
            queued.retain(|&(f, v, at, called)| {
                let done = f == fence as usize && v <= value;
                if done {
                    N[8].fetch_add(1, Ordering::Relaxed);
                    N[9].fetch_add(at.elapsed().as_micros() as u64, Ordering::Relaxed);
                    N[10].fetch_add(frame.saturating_sub(called), Ordering::Relaxed);
                    if detail_now() || SAID.fetch_add(1, Ordering::Relaxed) < 48 {
                        tracing::info!(
                            "pace: fence {fence:?} is set to {v}, {:.1} ms after the title's Signal for it (called in frame {called}, set in frame {frame})",
                            at.elapsed().as_micros() as f64 / 1e3
                        );
                    }
                }
                !done
            });
        }
        let mut asked = asked.lock().unwrap_or_else(|e| e.into_inner());
        asked.retain(|a| {
            let done = a.value <= value;
            if done {
                let us = a.at.elapsed().as_micros() as u64;
                N[3].fetch_add(1, Ordering::Relaxed);
                N[4].fetch_add(us, Ordering::Relaxed);
                N[5].fetch_add(frame.saturating_sub(a.frame), Ordering::Relaxed);
                if a.thread == render {
                    N[6].fetch_add(1, Ordering::Relaxed);
                    N[7].fetch_add(us, Ordering::Relaxed);
                }
                if detail_now() || SAID.fetch_add(1, Ordering::Relaxed) < 48 {
                    tracing::info!(
                        "pace: fence {fence:?} reached {} {:.1} ms after the wait for it (asked in frame {}, reached in frame {frame})",
                        a.value,
                        us as f64 / 1e3,
                        a.frame
                    );
                }
            }
            !done
        });
    }

    /// Once a frame, on the thread that presents: a line every 256 frames.
    pub(crate) fn frame() {
        RENDER.store(thread(), Ordering::Relaxed);
        let frame = crate::queue::presented();
        let last = LINE_AT.load(Ordering::Relaxed);
        if frame < last + 256 {
            return;
        }
        LINE_AT.store(frame, Ordering::Relaxed);
        let n: Vec<f64> = N.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64).collect();
        let frames = (frame - last).max(1) as f64;
        let per = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.0 };
        tracing::info!(
            "pace, per frame: fence waits already met {:.2}, not met {:.2} (the render thread's {:.2}); a wait is met {:.1} ms and {:.2} frames after it was asked \
             (the render thread's: {:.1} ms, {:.1} ms a frame); the title's queue signals: {:.2} a frame, a fence is set {:.1} ms and {:.2} frames after the title's Signal call",
            n[0] / frames,
            n[1] / frames,
            n[2] / frames,
            per(n[4], n[3]) / 1e3,
            per(n[5], n[3]),
            per(n[7], n[6]) / 1e3,
            n[7] / frames / 1e3,
            n[8] / frames,
            per(n[9], n[8]) / 1e3,
            per(n[10], n[8]),
        );
    }
}

/// IUnknown 3, object 4, device child 1, fence 3, fence 1 (2, a console
/// one among them).
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<FENCE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = completed as *const (); // 0x40
    f[9] = set_event_on_completion as *const (); // 0x48
    f[10] = signal as *const (); // 0x50
    f[11] = creation_flags as *const (); // 0x58
    f[12] = gpu_address as *const (); // 0x60
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[cfg(windows)]
    #[test]
    fn signals_set_waiting_events() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 1, 0, &FENCE_IIDS[0], &mut f), S_OK);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 1);
        // SAFETY: plain auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let wait = slot::<extern "system" fn(This, u64, *mut c_void) -> HRESULT>(f, 0x48);
        let signal = slot::<extern "system" fn(This, u64) -> HRESULT>(f, 0x50);
        assert_eq!(wait(f, 5, event), S_OK);
        // SAFETY: a valid event.
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(signal(f, 5), S_OK);
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 5, event), S_OK, "already reached");
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 9, std::ptr::null_mut()), E_NOTIMPL);
        // The title reads the value at the fence's address.
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f);
        // SAFETY: the fence's value.
        assert_eq!(unsafe { *(at as *const u64) }, 5);
        // SAFETY: ours.
        unsafe { CloseHandle(event) };
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }

    /// `VICTORY_FENCE_WAIT`: a blocking wait ends when another thread sets
    /// the fence, and gives up after 250 ms.
    #[cfg(windows)]
    #[test]
    fn blocking_waits_wake_on_set_or_time_out() {
        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 0, 0, &FENCE_IIDS[0], &mut f), S_OK);
        let at = f as usize;
        let setter = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            set(at as This, 3);
        });
        let wait = |value: u64| {
            let latch = latch(&mut fence(f).waiters.lock().unwrap(), value).unwrap();
            block_until(fence(f), value, latch, std::time::Instant::now() + std::time::Duration::from_millis(250))
        };
        let started = std::time::Instant::now();
        assert!(wait(3));
        assert!(started.elapsed() < std::time::Duration::from_millis(240), "woken, not timed out");
        setter.join().unwrap();
        let started = std::time::Instant::now();
        assert!(!wait(4));
        assert!(started.elapsed() >= std::time::Duration::from_millis(250));
        assert!(fence(f).waiters.lock().unwrap().is_empty(), "the waits' events are gone");
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }

    /// `VICTORY_FENCE_WAIT`: a wait sees its value reached even when the
    /// fence is moved back before the waiting thread looks.
    #[cfg(windows)]
    #[test]
    fn a_blocking_wait_sees_a_value_set_and_moved_back() {
        let mut f = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 0, 0, &FENCE_IIDS[0], &mut f), S_OK);
        let latch = latch(&mut fence(f).waiters.lock().unwrap(), 5).unwrap();
        set(f, 5);
        set(f, 4);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        assert!(block_until(fence(f), 5, latch, deadline));
        assert!(std::time::Instant::now() < deadline, "woken, not timed out");
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }
}
