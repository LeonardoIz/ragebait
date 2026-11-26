//! Budget-driven eviction of reloadable texture copies.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
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
static ALLOCATION_FAILED: AtomicBool = AtomicBool::new(false);

/// Recording threads can request a trim without taking the submission lock.
pub(super) fn request_after_oom() {
    ALLOCATION_FAILED.store(true, Relaxed);
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    used: u64,
    address: usize,
}

fn candidate(
    address: usize,
    used: u64,
    idle: bool,
    owners: usize,
    references: u32,
) -> Option<Candidate> {
    (idle && owners == 1 && references == 1).then_some(Candidate { used, address })
}

fn cold(address: usize, resource: &Resource, host: &Arc<HostTexture>) -> Option<Candidate> {
    let used = resource.host.used.load(Relaxed);
    let owners = Arc::strong_count(host);
    if !host.filled || !crate::host::long_ago(used, 1_000, 4) || owners != 1 {
        return None;
    }
    candidate(
        address,
        used,
        true,
        owners,
        crate::host::references(&host.texture),
    )
}

/// The caller holds the title-memory reading guard through removal.
pub(super) fn reloadable(address: usize, resource: &Resource) -> bool {
    const ALLOW_DISPLAY: u32 = 8;
    if resource.desc.sample_count > 1
        || resource.heap_flags & ALLOW_DISPLAY != 0
        || crate::texture::has_host_contents(address)
    {
        return false;
    }
    texture_layout(&resource.desc).is_some_and(|texture| {
        texture.planes.len() == 1
            && !texture.planes[0].layout.desc.swizzle.xor()
            && crate::texture::committed(
                resource.data().cast_const(),
                texture.planes[0].layout.size as usize,
            )
    })
}

/// Call under the host submission lock after the previous fence has
/// completed, and before preparing the next lists. A list being recorded,
/// a pending upload, or a descriptor that can still be indexed by a shader
/// keeps an Arc or COM reference of its own.
pub(crate) fn evict_if_pressured(video: Option<[u64; 4]>) {
    let (mut local, mut textures_needed) = pressure(video, texture_bytes(), texture_budget());
    if ALLOCATION_FAILED.swap(false, Relaxed) {
        local = local.max(HEADROOM);
    }
    if local == 0 && textures_needed == 0 {
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
            if (local == 0 && textures_needed == 0) || retired.len() == MAX_VICTIMS {
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
                || !reloadable(address, resource)
            {
                continue;
            }
            local = local.saturating_sub(host.bytes);
            textures_needed = textures_needed.saturating_sub(bytes);
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
    use super::{Candidate, HEADROOM, candidate, pressure};

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
            candidate(1, 10, false, 1, 1),
            candidate(2, 1, true, 2, 1),
            candidate(3, 2, true, 1, 2),
            candidate(4, 9, true, 1, 1),
            candidate(5, 3, true, 1, 1),
            candidate(6, 3, true, 1, 1),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        victims.sort_unstable();
        assert_eq!(
            victims,
            [
                Candidate {
                    used: 3,
                    address: 5
                },
                Candidate {
                    used: 3,
                    address: 6
                },
                Candidate {
                    used: 9,
                    address: 4
                },
            ]
        );
    }
}
