<p align="center">
  <img src="docs/logo.png" alt="ragebait" width="640">
</p>

**ragebait** runs the Xbox Series title natively on Windows from **your own
dump** of the game. The title's x86-64 code runs unchanged inside our
process; ragebait supplies the platform it expects (system services, D3D12.X
on host D3D12, console shaders recompiled to DXIL).

A hard fork of [victory-fork](https://github.com/fxceless/victory-fork),
itself a fork of [VIctory](https://github.com/Marvisak/VIctory).

> **This repository contains no game files, code or assets.** You need your
> own copy. Not affiliated with or endorsed by the game's developer or
> publisher. See [docs/asset-policy.md](docs/asset-policy.md).

## Goals

1. **Resources.** Cut CPU, RAM and VRAM use: the process holds about 24 GB
   and the card is full, and most bottlenecks come from there.
2. **Precompilation.** Do everything possible ahead of time (shaders,
   pipelines, textures) so the real-time path carries less.
3. **Patches and mods.** A system to change the game itself, not only the
   layer, so it can be optimized from the inside too.

## Status

**Status page:** [rb.l7i3.dev](https://rb.l7i3.dev) (progress and changelog)

**Not playable yet.** Measured on the ragebait test machine (2026-11-26;
laptop, Ryzen 7 260, RTX 5060 Laptop 8 GB, 32 GB RAM):

| Stage | State |
|-------|-------|
| Boot, menus, start screen | works |
| Opening scene, Series X mode | renders in 3D, about 2–15 fps |
| Opening scene, Series S mode | renders in 3D (plain upscale in place of the console FSR), same fps |
| Crash after ~2 minutes (device lost) | fixed |
| Light in shadow | black: ray-traced lighting does not run yet |
| Memory | ~24 GB process; the 8 GB card fills, smaller card copies keep rendering steadier |
| Bottleneck | CPU side of the layer (submit thread, page remapping); the GPU idles most of each frame |

victory-fork measured 8–10 fps in the first gameplay scene on a different
machine (i7-14700KF, RTX 4070 12 GB); the two sets of numbers are not
comparable. Its measurements and experiments: [docs/victory-fork-log.md](docs/victory-fork-log.md).

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 and mesh shaders, Rust (pinned in `rust-toolchain.toml`).
The title needs several GiB of free commit at startup; if `crashes.log`
reports Windows error 1455, enlarge the paging file.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Precompile shaders and run:

```powershell
cargo build --release -p victory
target\release\victory.exe translate --warm all --jobs 16   # no paths: game_dir from victory.toml
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

To prebuild pipeline state objects (PSOs), set `$env:VICTORY_PSO_PREBUILD = '1'`
in PowerShell and run the game to record the pipelines it uses. Close the game,
clear the switch with `Remove-Item Env:VICTORY_PSO_PREBUILD`, and run
`target\release\victory.exe prebuild --jobs 4`. Then play normally. Rerun
prebuild after a driver or translator change.

| Variable | Default | Effect |
|----------|---------|--------|
| `VICTORY_STATS` | On | Show resource statistics in the FPS overlay |
| `VICTORY_PRESENT_COMPOSITE` | On | Composite multi-plane presents |
| `VICTORY_FFX_PASSTHROUGH` | On for Series S, off for Series X | Use a scaled copy in place of the console FidelityFX upscaler |
| `VICTORY_WAVE_WIDE_SEEN` | On | Enable wave-wide branches for the built-in list of shader hashes |
| `VICTORY_WAVE_WIDE_LIST` | Empty | Add comma-separated shader hashes to that selection |
| `VICTORY_WAVE_LDS_LOOSE=1` | Off | Retry wave64 compute shaders on 32-lane hosts with LDS barriers at divergent region boundaries. A read may miss the other host wave's write within a region |
| `VICTORY_SCC_LOCAL_ANY=1` | Off | Allow the local-wave retry to use each host wave's own result for scalar tests of whether a lane mask is nonempty |
| `VICTORY_FAIR_DUE=1` | Off | Alternate due texture retries with fresh slots |
| `VICTORY_TEXTURE_WATCH=1` | Off | Recheck small textures recently found changed. Up to 32 early checks per frame get the first eighth of the pass's remaining time and staging budget |
| `VICTORY_RELOOK_MS` | 4,000 ms, or 20,000 ms with fair scheduling | Set the retry window for textures whose memory is missing. The window is clamped to 16–256 frames by default or 16–1,024 with fair scheduling or an explicit override |
| `VICTORY_LOOKUP_CACHE` | Off | Cache host buffer lookups per thread |
| `VICTORY_REMAP_LAZY` | Off | Defer Windows views for GPU page aliases until accessed. Uses single-page views and takes precedence over `VICTORY_MAP_RUNS` and `VICTORY_REMAP_BATCH`. Works with `VICTORY_SECTION_MB` |
| `VICTORY_REMAP_FAST` | Off | Skip remaps that would leave the same pages in place |
| `VICTORY_REMAP_STATS` | Off | Measure physical-page remap churn, backing reuse, and time by operation |
| `VICTORY_SECTION_MB=2` | Off | Cap physical-page backing sections at 2 MiB. Works with `VICTORY_MAP_RUNS` and `VICTORY_REMAP_BATCH`. DirectStorage uses its copy path for writes that cross sections |
| `VICTORY_AMBIENT` | Off | Apply screen-space ambient lighting to explicitly selected compute passes |
| `VICTORY_RT_DISPATCH_FIX=1` | Off | Honor DispatchX thread counts for ray-tracing builder stages |
| `VICTORY_PSO_PREBUILD` | Off | Record pipelines for offline prebuild |

The best switch set measured so far is in
[diagnostics/README.md](diagnostics/README.md). Each `VICTORY_*` switch is
described in a comment where it is read.

Diagnostics build (logs, profiler, captures):

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug output |
| `VICTORY_D3D12_DEBUG=1` / `VICTORY_D3D12_DRED=1` | D3D12 debug layer / GPU failure data |
| `VICTORY_D3D12_WARP=1` | software adapter (slow; cannot reset the driver) |
| `VICTORY_GPU_TIMING=1` | each list timed on its own |
| `VICTORY_SHOT=<folder>` | capture of one whole frame |
| `VICTORY_FRAME_LOG=1` | length and breakdown of every frame |

With `VICTORY_REMAP_STATS=1`, each present closes one measurement interval.
`VICTORY_FRAME_LOG=1` logs a `frame N remap stats` line for that interval.
The lazy counters show alias pages deferred, discarded before materialization,
and materialized, including the subset resolved by a CPU fault. They also
show failed materialization attempts and elapsed materialization time.
Compare these alongside native unmapping time and whole-frame time. An
alias that has acquired a native view still needs a native unmap when
replaced or cleared. GPU residency checks, write guards, and kernel I/O
can materialize aliases before the title touches them.
The exit table combines all intervals, including `tail_calls` after the last
present. It reports only aggregates.

- `calls` includes alias and physical-page calls, with scatter counted as one
  call per destination. `failed` counts calls that returned failure.
  `call_pages` buckets calls by requested size in 64 KiB pages.
- `touches` counts requested destination pages, including no-ops and failed
  requests. `pages` counts distinct destinations, and `backings` counts
  distinct physical pages seen before or after those requests. `unchanged`
  counts touches whose final backing matches the initial one, including
  empty-to-empty touches.
- `same_frame` and `prev_1_8` classify repeat touches by when that destination
  was last touched. `touches_per_page` buckets distinct destinations by their
  touch count in the reported interval or in the whole run at exit.
- `swap_return_frames` counts A→B→A returns, and `clear_return_frames` counts
  A→empty→A returns. The gap starts when A was replaced. `mapped_elsewhere`
  counts changed destinations whose incoming backing is also mapped at
  another title destination after the call.
- `adjacent_calls` counts consecutive successful nonempty calls in the
  same frame whose destination ranges abut in increasing order. Adjacency
  measures destination locality. The frame log's existing views-to-pages
  counts show how much the actual mapping code coalesced.
- The `us` buckets give cumulative microseconds. `map` includes the
  placeholder split measured by the existing view mapper. `bookkeeping`
  is elapsed remap time after subtracting the existing map, unmap, commit
  and reader-wait timers. It includes untimed work such as run planning,
  placeholder coalescing and failure handling, and excludes stats collection.

Input: XInput pads. With none connected the keyboard is a pad (WASD/IJKL
sticks, arrows D-pad, Enter/Backspace/X/Y buttons, Q/E/Z/C shoulders and
triggers, Esc/Tab Menu/View).

## Branches

- `main`: ragebait development.
- `victory-fork`: mirror of victory-fork's `main` (remote `victory-fork`).
- `victory`: mirror of upstream VIctory's `main` (remote `victory`).

Update a mirror and bring changes in:

```powershell
git fetch victory-fork
git branch -f victory-fork victory-fork/main
git merge victory-fork            # or cherry-pick single commits
```

## Credits

- [VIctory](https://github.com/Marvisak/VIctory) by Marvisak and
  contributors: the loader, runtime services, D3D12 back end and shader
  recompiler.
- [victory-fork](https://github.com/fxceless/victory-fork) by fxceless and
  contributors, with work by myokex, Joshua Odom and the pull-request authors
  named in its history.
- [XCL](https://github.com/ivall/xcl) by ivall, as a behaviour reference.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
