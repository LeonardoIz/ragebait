//! Budget-driven eviction of reloadable texture copies.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};

use super::{
    HostTexture, KINDS, Resource, texture_budget, texture_bytes, texture_layout, textures,
};
use crate::framelog::{T, add};
use crate::object::This;

/// Set `VICTORY_VRAM_EVICT=1` to evict cold texture copies under VRAM
/// pressure. Off by default.
pub(crate) fn eviction_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VICTORY_VRAM_EVICT").is_some_and(|v| v == "1"))
}

const HEADROOM: u64 = 32 << 20;
const MAX_VICTIMS: usize = 128;
/// Recording threads can request a trim without taking the submission lock.
static ALLOCATION_FAILED: AtomicBool = AtomicBool::new(false);

/// The explicit texture cap is independent of the adapter's overall budget.
fn pressure(video: Option<[u64; 4]>, filled: u64, cap: Option<u64>) -> (u64, u64) {
    let local = video.map_or(0, |[budget, used, ..]| excess(used, budget / 100 * 97));
    let textures = cap.map_or(0, |cap| excess(filled, cap));
    (local, textures)
}

fn excess(used: u64, limit: u64) -> u64 {
    if limit != 0 && used >= limit {
        (used - limit).saturating_add(HEADROOM)
    } else {
        0
    }
}

/// Keep room for pending copies without trimming into the part of the
/// texture cache that admission would immediately fill again.
fn reserved_pressure(used: u64, limit: u64, filled: u64) -> u64 {
    excess(used, limit).min(filled.saturating_sub(super::TEXTURE_ROOM_FLOOR))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    used: u64,
    address: usize,
}

/// Pure part of [`cold`]: the state a recorder sees under `made`'s lock.
/// Selection and the revalidation after relocking compare its result.
fn cold_state(
    address: usize,
    used: u64,
    filled: bool,
    idle: bool,
    owners: usize,
    references: u32,
) -> Option<Candidate> {
    (filled && idle && owners == 1 && references == 1).then_some(Candidate { used, address })
}

fn cold(address: usize, resource: &Resource, host: &Arc<HostTexture>) -> Option<Candidate> {
    let used = resource.host.used.load(Relaxed);
    cold_state(
        address,
        used,
        host.filled,
        crate::host::long_ago(used, 1_000, 4),
        Arc::strong_count(host),
        crate::host::references(&host.texture),
    )
}

/// The caller holds the title-memory reading guard through removal.
pub(super) fn reloadable(address: usize, resource: &Resource) -> bool {
    let texture = std::cell::OnceCell::new();
    let layout = || texture.get_or_init(|| texture_layout(&resource.desc));
    may_reload(
        resource.desc.sample_count,
        resource.heap_flags,
        || crate::texture::has_host_contents(address),
        || {
            layout()
                .as_ref()
                .map(|t| (t.planes.len(), t.planes[0].layout.desc.swizzle.xor()))
        },
        || {
            layout().as_ref().is_some_and(|t| {
                crate::texture::committed(
                    resource.data().cast_const(),
                    t.planes[0].layout.size as usize,
                )
            })
        },
    )
}

/// Pure part of [`reloadable`]: only a copy whose contents title memory can
/// give back in full. `host_contents` covers both a host copy/resolve write
/// (`host_wrote`) and an alias copy (`THROUGH`); `layout` is the plane count
/// and whether plane 0 uses an XOR swizzle. The checks run in this order.
fn may_reload(
    sample_count: u32,
    heap_flags: u32,
    host_contents: impl FnOnce() -> bool,
    layout: impl FnOnce() -> Option<(usize, bool)>,
    committed: impl FnOnce() -> bool,
) -> bool {
    const ALLOW_DISPLAY: u32 = 8;
    sample_count <= 1
        && heap_flags & ALLOW_DISPLAY == 0
        && !host_contents()
        && layout() == Some((1, false))
        && committed()
}

/// Whether a host copy may be made now: `retry_at` is 0 or has passed.
pub(super) fn may_retry(retry_at: u64, now_ms: u64) -> bool {
    retry_at == 0 || now_ms >= retry_at
}

/// After a failed host creation with eviction `enabled`: the time of the
/// next try for a transient out-of-memory error on a filled texture (and a
/// trim is requested by setting `ALLOCATION_FAILED`), or `None` for a
/// permanent refusal.
pub(super) fn retry_at(
    enabled: bool,
    out_of_memory: bool,
    filled: bool,
    now_ms: u64,
) -> Option<u64> {
    (enabled && out_of_memory && filled).then(|| {
        ALLOCATION_FAILED.store(true, Relaxed);
        now_ms + 1_000
    })
}

/// A new host copy of a texture evicted under pressure must be uploaded
/// again from title memory before it is read.
pub(super) fn carry_eviction(evicted: &AtomicBool, reupload: &AtomicBool) {
    if evicted.swap(false, Relaxed) {
        reupload.store(true, Relaxed);
    }
}

/// When a submitted upload of a replacement copy runs (`defer_host`):
/// clears the copy's flag, true the first time.
pub(crate) fn reuploaded(reupload: &AtomicBool) -> bool {
    reupload.swap(false, Relaxed)
}

/// When [`evict_if_pressured`] last ran (`crate::host::now_ms`).
static LAST_RUN_MS: AtomicU64 = AtomicU64::new(0);
const STALL_MS: u64 = 5_000;

/// Whether a trim was needed but none ran for [`STALL_MS`].
fn stalled(last_run_ms: u64, now_ms: u64, needed: impl FnOnce() -> bool) -> bool {
    now_ms.saturating_sub(last_run_ms) >= STALL_MS && needed()
}

/// The submitting thread found no idle moment to trim in (late or relay
/// flights in the air, a submission under way, or the direct fence behind).
/// Counted in the frame log's "eviction checks stalled", at most once per
/// 100 ms, while a trim was needed and none ran for [`STALL_MS`].
pub(crate) fn eviction_skipped(video: impl FnOnce() -> Option<[u64; 4]>) {
    static COUNTED_TENTH: AtomicU64 = AtomicU64::new(0);
    static SAID: std::sync::Once = std::sync::Once::new();
    let now = crate::host::now_ms();
    let tenth = now / 100 + 1;
    // Claimed before the check, so the DXGI query runs at most once per 100 ms.
    if COUNTED_TENTH.swap(tenth, Relaxed) == tenth
        || !stalled(LAST_RUN_MS.load(Relaxed), now, || {
            ALLOCATION_FAILED.load(Relaxed)
                || pressure(video(), texture_bytes(), texture_budget()) != (0, 0)
        })
    {
        return;
    }
    add(T::VramEvictStalled, 1);
    SAID.call_once(|| {
        tracing::warn!("VRAM eviction needed but not run for 5 s: no idle moment between submissions (late or relay flights?)");
    });
}

/// Call under the host submission lock after the previous fence has
/// completed, and before preparing the next lists. A list being recorded,
/// a pending upload, or a descriptor that can still be indexed by a shader
/// keeps an Arc or COM reference of its own.
pub(crate) fn evict_if_pressured(video: Option<[u64; 4]>) {
    LAST_RUN_MS.store(crate::host::now_ms(), Relaxed);
    let filled = texture_bytes();
    let cap = texture_budget();
    let (mut local, mut textures_needed) = pressure(video, filled, cap);
    let mut reserved = 0;
    let mut above_floor = filled.saturating_sub(super::TEXTURE_ROOM_FLOOR);
    if crate::shadow::card_room_shared()
        && cap.is_none()
        && let Some([budget, used, ..]) = video
    {
        reserved = reserved_pressure(
            super::texture_vram_used(used),
            super::texture_vram_limit(budget),
            filled,
        );
    }
    if ALLOCATION_FAILED.swap(false, Relaxed) {
        local = local.max(HEADROOM);
    }
    if local == 0 && textures_needed == 0 && reserved == 0 {
        return;
    }
    let mut retired = Vec::new();
    let mut evicted_bytes = 0;
    {
        let all = textures();
        let mut candidates = Vec::new();
        for &address in all.iter() {
            // SAFETY: the table's lock keeps its resources alive.
            let resource = unsafe { &*(address as *const Resource) };
            if let Ok(made) = resource.host.made.try_lock()
                && let Some((host, None)) = made.as_ref()
                && let Some(candidate) = cold(address, resource, host)
            {
                candidates.push(candidate);
            }
        }
        candidates.sort_unstable();
        for Candidate { used, address } in candidates {
            if (local == 0 && textures_needed == 0 && reserved == 0) || retired.len() == MAX_VICTIMS {
                break;
            }
            // SAFETY: the same table lock still protects this resource.
            let resource = unsafe { &*(address as *const Resource) };
            let Some(_reading) = crate::watch::title_memory_reading_now() else {
                break;
            };
            let Ok(mut made) = resource.host.made.try_lock() else {
                continue;
            };
            let Some((host, None)) = made.as_ref() else {
                continue;
            };
            // A recorder may have used or pinned it since selection.
            if cold(address, resource, host) != Some(Candidate { used, address }) {
                continue;
            }
            let tiles = host
                .tiled
                .as_ref()
                .map_or(0, crate::tiles::Tiled::given_bytes);
            let bytes = host.bytes + tiles;
            // A freed tile may remain in a shared heap: it makes room
            // under the texture cap, but cannot be credited to DXGI.
            if bytes == 0
                || (host.bytes == 0 && textures_needed == 0)
                || (local == 0 && textures_needed == 0 && bytes > above_floor)
                || !reloadable(address, resource)
            {
                continue;
            }
            local = local.saturating_sub(host.bytes);
            textures_needed = textures_needed.saturating_sub(bytes);
            above_floor = above_floor.saturating_sub(bytes);
            reserved = reserved.saturating_sub(host.bytes).min(above_floor);
            let stage = crate::texture::take_staged(address as This);
            retired.push((made.take(), stage));
            resource.host.gone.store(true, Relaxed);
            resource.host.evicted.store(true, Relaxed);
            KINDS[2].fetch_add(1, Relaxed);
            KINDS[3].fetch_add(bytes, Relaxed);
            evicted_bytes += bytes;
        }
    }
    let count = retired.len() as u64;
    // The host fence and list/descriptor checks ensure there are no GPU
    // users. Release immediately (under the submit lock) instead of on the reaper,
    // so the room checks see the freed cache space on the next pass.
    drop(retired);
    add(T::VramEvictedBytes, evicted_bytes);
    add(T::VramEvictedResources, count);
}

#[cfg(test)]
mod tests {
    use super::{
        ALLOCATION_FAILED, Candidate, HEADROOM, carry_eviction, cold_state, may_reload, may_retry,
        pressure, reserved_pressure, retry_at, reuploaded, stalled,
    };
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    #[test]
    fn reservation_trims_only_above_the_admission_floor() {
        let floor = super::super::TEXTURE_ROOM_FLOOR;
        let limit = 7 << 30;
        assert_eq!(reserved_pressure(limit - 1, limit, floor + HEADROOM), 0);
        assert_eq!(reserved_pressure(limit + 1, limit, floor), 0);
        assert_eq!(reserved_pressure(limit + 1, limit, floor + 1), 1);
        assert_eq!(reserved_pressure(limit, limit, floor + HEADROOM), HEADROOM);
    }

    #[test]
    fn pressure_respects_both_budgets() {
        let budget = 8 << 30;
        let line = budget / 100 * 97;
        assert_eq!(
            pressure(Some([budget, line - 1, 0, 0]), 99, Some(100)),
            (0, 0)
        );
        assert_eq!(
            pressure(Some([budget, line + 1, 0, 0]), 100, Some(100)),
            (HEADROOM + 1, HEADROOM)
        );
        assert_eq!(pressure(Some([0, budget, 0, 0]), 0, Some(0)), (0, 0));
        assert_eq!(pressure(None, 101, Some(100)), (0, HEADROOM + 1));
    }

    #[test]
    fn selects_oldest_idle_copies_without_list_or_descriptor_references() {
        let mut victims = [
            cold_state(1, 10, true, false, 1, 1),
            cold_state(2, 1, true, true, 2, 1),
            cold_state(3, 2, true, true, 1, 2),
            cold_state(7, 0, false, true, 1, 1),
            cold_state(4, 9, true, true, 1, 1),
            cold_state(5, 3, true, true, 1, 1),
            cold_state(6, 3, true, true, 1, 1),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        victims.sort_unstable();
        let at = |used, address| Candidate { used, address };
        assert_eq!(victims, [at(3, 5), at(3, 6), at(9, 4)]);
    }

    #[test]
    fn revalidation_after_relocking_drops_copies_used_or_pinned_since() {
        let selected = cold_state(5, 3, true, true, 1, 1).unwrap();
        // Unchanged: still evicted.
        assert_eq!(cold_state(5, 3, true, true, 1, 1), Some(selected));
        // Asked for again since (a newer `used`, recent or not).
        assert_ne!(cold_state(5, 4, true, true, 1, 1), Some(selected));
        assert_ne!(cold_state(5, 4, true, false, 1, 1), Some(selected));
        // A list holds an Arc, or a descriptor slot a COM reference.
        assert_ne!(cold_state(5, 3, true, true, 2, 1), Some(selected));
        assert_ne!(cold_state(5, 3, true, true, 1, 2), Some(selected));
    }

    #[test]
    fn only_copies_title_memory_gives_back_whole_are_reloadable() {
        let ok = |samples, flags, host, layout, committed| {
            may_reload(samples, flags, || host, || layout, || committed)
        };
        assert!(ok(1, 0, false, Some((1, false)), true));
        // A host copy, resolve or alias copy (`host_wrote`, `THROUGH`).
        assert!(!ok(1, 0, true, Some((1, false)), true));
        assert!(!ok(4, 0, false, Some((1, false)), true));
        assert!(!ok(1, 8, false, Some((1, false)), true));
        assert!(!ok(1, 0, false, Some((2, false)), true));
        assert!(!ok(1, 0, false, Some((1, true)), true));
        assert!(!ok(1, 0, false, None, true));
        assert!(!ok(1, 0, false, Some((1, false)), false));
        // Host contents are excluded before the layout or memory is read.
        assert!(!may_reload(
            1,
            0,
            || true,
            || panic!("layout read"),
            || panic!("memory read")
        ));
    }

    #[test]
    fn evicted_copy_is_reuploaded_once_and_retries_wait_after_out_of_memory() {
        let (evicted, reupload) = (AtomicBool::new(true), AtomicBool::new(false));
        carry_eviction(&evicted, &reupload);
        assert!(!evicted.load(Relaxed) && reupload.load(Relaxed));
        // The submitted upload's `defer_host` clears it, counted once.
        assert!(reuploaded(&reupload));
        assert!(!reuploaded(&reupload));
        // A copy made again without an eviction needs no reupload.
        carry_eviction(&evicted, &reupload);
        assert!(!reupload.load(Relaxed));

        assert!(may_retry(0, 5));
        assert!(!may_retry(1_005, 1_004));
        assert!(may_retry(1_005, 1_005));
        assert_eq!(retry_at(false, true, true, 5), None);
        assert!(!ALLOCATION_FAILED.load(Relaxed));
        assert_eq!(retry_at(true, false, true, 5), None);
        assert_eq!(retry_at(true, true, false, 5), None);
        assert!(!ALLOCATION_FAILED.load(Relaxed));
        assert_eq!(retry_at(true, true, true, 5), Some(1_005));
        assert!(ALLOCATION_FAILED.swap(false, Relaxed));
    }

    #[test]
    fn a_stall_is_counted_only_when_needed_and_late() {
        assert!(!stalled(1_000, 5_999, || panic!(
            "pressure read before 5 s"
        )));
        assert!(!stalled(1_000, 6_000, || false));
        assert!(stalled(1_000, 6_000, || true));
        // Never run since the start.
        assert!(stalled(0, 5_000, || true));
    }
}
