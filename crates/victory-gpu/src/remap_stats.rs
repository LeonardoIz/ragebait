use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, Once};

use crate::framelog::{T, total};

fn lazy_totals() -> [u64; 6] {
    [
        T::RemapLazyDeferred,
        T::RemapLazyDiscarded,
        T::RemapLazyViews,
        T::RemapLazyFaults,
        T::RemapLazyFailures,
        T::RemapLazyUs,
    ]
    .map(total)
}

fn lazy_counts([deferred, discarded, views, faults, failures, us]: [u64; 6]) -> String {
    format!(
        "lazy_deferred={deferred} lazy_discarded={discarded} lazy_materialized={views} lazy_fault_views={faults} lazy_failures={failures} lazy_materialize_us={us}"
    )
}

const RECENT_FRAMES: u64 = 8;
type Backing = usize;

static STATE: Mutex<Option<Stats>> = Mutex::new(None);

/// VICTORY_REMAP_STATS=1 records destination and backing reuse. Off by
/// default. Only aggregates are logged, even at exit.
pub fn on() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_REMAP_STATS").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// Timers already recorded by xmem under its physical-page lock.
pub fn timing() -> [u64; 4] {
    [
        T::RemapUnmapUs,
        T::RemapMapUs,
        T::RemapCommitUs,
        T::RemapWaitUs,
    ]
    .map(total)
}

/// The system's unmapping and mapping calls so far ([`T::RemapUnmaps`],
/// [`T::RemapViews`]), read around each call like [`timing`].
pub fn os_calls() -> [u64; 2] {
    [T::RemapUnmaps, T::RemapViews].map(total)
}

struct Page {
    frame: u64,
    touches: u64,
    frame_touches: u64,
    current: Option<Backing>,
    // Backing immediately before the latest change and when it was replaced.
    previous: Option<(Backing, u64)>,
}

#[derive(Default)]
struct Counts {
    calls: u64,
    failed: u64,
    touches: u64,
    same_frame: u64,
    recent: u64,
    unchanged: u64,
    aliased: u64,
    adjacent: u64,
    frequency: [u64; 4],
    returns: [u64; 4],
    after_clear: [u64; 4],
    sizes: [u64; 5],
    us: [u64; 5],
    os: [u64; 2],
    backings: HashSet<Backing>,
}

impl Counts {
    fn call(&mut self, n: usize, ok: bool, adjacent: bool, us: [u64; 5], os: [u64; 2]) {
        self.calls += 1;
        self.failed += u64::from(!ok);
        self.adjacent += u64::from(adjacent);
        self.sizes[match n {
            0 => 0,
            1 => 1,
            2..=4 => 2,
            5..=16 => 3,
            _ => 4,
        }] += 1;
        for (sum, elapsed) in self.us.iter_mut().zip(us) {
            *sum += elapsed;
        }
        for (sum, calls) in self.os.iter_mut().zip(os) {
            *sum += calls;
        }
    }

    fn touch(
        &mut self,
        count: u64,
        gap: Option<u64>,
        before: Option<Backing>,
        after: Option<Backing>,
        aliased: bool,
        returned: Option<u64>,
    ) {
        self.touches += 1;
        self.same_frame += u64::from(gap == Some(0));
        self.recent += u64::from(gap.is_some_and(|gap| (1..=RECENT_FRAMES).contains(&gap)));
        self.unchanged += u64::from(before == after);
        self.aliased += u64::from(aliased);
        self.backings.extend(before);
        self.backings.extend(after);
        let bucket = |n| match n {
            1 => 0,
            2 => 1,
            3..=4 => 2,
            _ => 3,
        };
        self.frequency[bucket(count)] += 1;
        if count > 1 {
            self.frequency[bucket(count - 1)] -= 1;
        }
        if let Some(gap) = returned {
            let buckets = if before.is_some() {
                &mut self.returns
            } else {
                &mut self.after_clear
            };
            buckets[match gap {
                0 => 0,
                1 => 1,
                2..=RECENT_FRAMES => 2,
                _ => 3,
            }] += 1;
        }
    }

    fn activity(&self) -> String {
        format!(
            "calls={} failed={} touches={} pages={} backings={} same_frame={} prev_1_8={} unchanged={} mapped_elsewhere={} adjacent_calls={}",
            self.calls,
            self.failed,
            self.touches,
            self.frequency.iter().sum::<u64>(),
            self.backings.len(),
            self.same_frame,
            self.recent,
            self.unchanged,
            self.aliased,
            self.adjacent
        )
    }

    fn histograms(&self) -> String {
        format!(
            "touches_per_page[1,2,3-4,5+]={:?} call_pages[0,1,2-4,5-16,17+]={:?} swap_return_frames[0,1,2-8,9+]={:?} clear_return_frames[0,1,2-8,9+]={:?}",
            self.frequency, self.sizes, self.returns, self.after_clear
        )
    }

    fn time(&self) -> String {
        format!(
            "us[unmap,map,commit,reader_wait,bookkeeping]={:?} os_calls[unmap,map]={:?}",
            self.us, self.os
        )
    }
}

#[derive(Default)]
struct Stats {
    frame: u64,
    current: Counts,
    total: Counts,
    pages: HashMap<usize, Page>,
    previous_end: Option<usize>,
    last_lazy: [u64; 6],
}

impl Stats {
    fn touch(&mut self, at: usize, before: Option<Backing>, after: Option<Backing>, aliased: bool) {
        let frame = self.frame;
        let page = self.pages.entry(at).or_insert(Page {
            frame,
            touches: 0,
            frame_touches: 0,
            current: before,
            previous: None,
        });
        let gap = (page.touches != 0).then(|| frame - page.frame);
        if gap != Some(0) {
            page.frame_touches = 0;
        }
        // A mapping changed outside the previously observed target range.
        // Discard its stale history instead of attributing a possible return.
        if page.current != before {
            page.previous = None;
        }
        let mut returned = None;
        if before != after {
            if let Some((backing, departed)) = page.previous
                && after == Some(backing)
            {
                returned = Some(frame - departed);
            }
            page.previous = before.map(|backing| (backing, frame));
        }
        page.current = after;
        page.frame = frame;
        page.touches += 1;
        page.frame_touches += 1;
        self.current
            .touch(page.frame_touches, gap, before, after, aliased, returned);
        self.total
            .touch(page.touches, gap, before, after, aliased, returned);
    }
}

/// Record a complete call under the physical-page lock. A concurrent present
/// assigns the whole call to the interval in which it is recorded.
pub fn record(
    base: usize,
    end: usize,
    n: usize,
    ok: bool,
    us: [u64; 5],
    os: [u64; 2],
    pages: impl Iterator<Item = (usize, Option<Backing>, Option<Backing>, bool)>,
) {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        // SAFETY: the callback lives for the life of the host process.
        if unsafe { atexit(summary) } != 0 {
            tracing::warn!("could not register remap stats exit summary");
        }
    });
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let stats = guard.get_or_insert_with(Stats::default);
    let adjacent = ok && n != 0 && stats.previous_end == Some(base);
    stats.previous_end = (ok && n != 0).then_some(end);
    stats.current.call(n, ok, adjacent, us, os);
    stats.total.call(n, ok, adjacent, us, os);
    for (at, before, after, aliased) in pages {
        stats.touch(at, before, after, aliased);
    }
}

pub(crate) fn frame(log: bool) {
    let (frame, counts, lazy) = {
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let stats = guard.get_or_insert_with(Stats::default);
        stats.frame += 1;
        stats.previous_end = None;
        let totals = lazy_totals();
        let lazy = std::array::from_fn(|i| totals[i].wrapping_sub(stats.last_lazy[i]));
        stats.last_lazy = totals;
        (stats.frame, std::mem::take(&mut stats.current), lazy)
    };
    if log {
        tracing::info!(
            "frame {frame} remap stats | {} | {} | {} | {}",
            counts.activity(),
            counts.histograms(),
            counts.time(),
            lazy_counts(lazy)
        );
    }
}

unsafe extern "C" {
    fn atexit(f: extern "C" fn()) -> i32;
}

extern "C" fn summary() {
    log_summary();
}

/// Also called by the title's explicit process termination paths.
pub fn log_summary() {
    // Shutdown can race an in-flight remap. Never block exit on its lock.
    if let Ok(guard) = STATE.try_lock()
        && let Some(s) = guard.as_ref()
        && s.total.calls != 0
    {
        tracing::info!(
            "remap stats total | frames={} tail_calls={} | {}",
            s.frame,
            s.current.calls,
            s.total.activity()
        );
        tracing::info!("remap stats distribution | {}", s.total.histograms());
        tracing::info!("remap stats time | {} | {}", s.total.time(), lazy_counts(lazy_totals()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_history_counts_reuse_and_returns_by_frame() {
        let mut s = Stats::default();
        s.touch(1, None, Some(10), false);
        s.touch(1, Some(10), Some(20), false);
        s.touch(1, Some(20), Some(10), true);
        s.touch(2, None, Some(10), true);
        assert_eq!(s.current.frequency, [1, 0, 1, 0]);
        assert_eq!(s.current.same_frame, 2);
        assert_eq!(s.current.returns, [1, 0, 0, 0]);
        assert_eq!(s.current.backings.len(), 2);
        assert_eq!(s.current.aliased, 2);

        s.frame = 1;
        s.current = Counts::default();
        s.touch(1, Some(10), None, false);
        s.frame = 9;
        s.current = Counts::default();
        s.touch(1, None, Some(10), false);
        s.touch(2, Some(10), Some(10), false);
        assert_eq!(s.current.recent, 1);
        assert_eq!(s.current.unchanged, 1);
        assert_eq!(s.current.after_clear, [0, 0, 1, 0]);
        assert_eq!(s.current.frequency, [2, 0, 0, 0]);
        assert_eq!(s.total.frequency, [0, 1, 0, 1]);

        // The prior state was changed outside the observed range.
        s.touch(1, Some(20), Some(10), false);
        assert_eq!(s.total.returns, [1, 0, 0, 0]);
    }
}
