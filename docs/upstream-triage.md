# Upstream VIctory triage (2026-11-26)

`victory` (upstream VIctory `63bcca4`) has 479 commits not in `victory-fork`
(merge base `ffe310e`): 342 non-merge. Both sides changed 80 of the same
files; base-relative hunks overlap in 52, including every hot GPU file. A full
`git merge victory` is not worth it: cherry-pick, and port larger changes onto
this fork's own submit and memory paths.

## To bring in, ranked

| # | Upstream commits | Area | Impact | Conflict | Note |
|--:|---|---|---|---|---|
| 1 | `23a4b62` `cc5f961` once-per-submit uploads, changed mips only | GPU | high | high | keep our dirty-upload logic; port the dedup and per-mip refresh |
| 2 | `56f75f8` `8a3d975` `ea81289` `8f2a6f1` parallel refresh, staging chunks, barrier batch, scratch reuse | GPU | high | high | texture refresh out of heap locks |
| 3 | `69d1e8e` `4959d34` `11e9a54` budget eviction, recent use, placed heaps | GPU | high | high | regressed frame time upstream: measure |
| 4 | `1953339` `6cc6aa3` `2922141` skip clean copies, suballocate small views | GPU | high | high | submit-path work |
| 5 | `3e10052` release retired D3D12 objects off-thread | GPU | med | med | |
| 6 | `e04c078` constant tables after instructions | shader | med | med | NVIDIA allocation size |
| 7 | `4106550` faster shadow write-back merge | GPU | med | med | benchmark against our masked loop |
| 8 | `64b80c3` `705a2f6` shared swizzle tables, inlined micro-tile runs | formats | med | med | CPU detiling |
| 9 | `7389303` `286e33f` `9c58a1c` shader metadata, packs, offline PSOs | formats/GPU | high | high | our precompilation goal |
| 10 | `bca3ecd` `a73d59e` fresh translations returned, concurrent cache publish | shader | med | med | |
| 11 | `321a4bc` defer PSO saves on allocation failure | GPU | med | med | |
| 12 | `9cc37c5` honor queue memory waits | GPU | high | high | reconcile with our submit thread |
| 13 | `315cea1` prelude barrier order, clamp scratch | GPU | high | high | barrier part only |
| 14 | `ea488a9` `78ed12e` `6fcbbe9` typed UAV, null CBV, unaligned bytecode | GPU | med | med | a device-removal trigger |
| 15 | `5c4c685` drop executable bytes after mapping | loader | med | low | |

Order: small backports first (14, 15, 10, 11), then the texture changes in
measured steps.

## Skipped

Docs-only, unrelated HLE/UI/input/audio, diagnostics-only, broad shader test
churn, and what this fork already has in another form (depth bounds,
descriptor mirroring, early indirect rejection, snapshot-pool reduction,
DirectStorage fixes). Deferred: the page-table and marked-write stack
(`27c3dc9` `cd94d35` `781e846` `cfc2344`), tied to upstream's guard and
copy-back model.
