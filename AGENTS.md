# Working in ragebait

ragebait runs an Xbox Series title's x86-64 code natively on Windows (HLE
layer, D3D12.X on host D3D12, console shaders to DXIL). Rust workspace; see
`docs/architecture.md` for the crate map.

## Hard rules

- **Nothing from the game is ever committed** (assets, code, strings, shader
  dumps, logs with game data). Read `docs/asset-policy.md`. Game data lives
  outside the repo, configured in `victory.toml` (git-ignored, never commit it).
- Code, comments, docs and commit messages are in **English**.
- Never push to the `victory` or `victory-fork` remotes or branches: they are
  read-only mirrors. Work lands on `main` (or a task branch merged into it).
- Do not rename the `victory-*` crates or reorganize large files without being
  asked: it makes merges from the mirrors expensive.

## How to change code

- The smallest complete change. Reuse what exists; no speculative options,
  abstractions or "for later" code.
- New behaviour that may cost speed or correctness goes behind a `VICTORY_*`
  environment switch, documented in a comment where it is read, like the
  existing ones.
- New non-trivial logic gets a small unit test. Tests must not need game data
  (tests that do take paths from environment variables and skip when unset).
- Approximations of console behaviour get a `shortcut: <limit>, <when to
  upgrade>` comment.

## Gates (all must pass before a change is done)

```powershell
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo xtask policy
```

If a parallel test gate fails, rerun it with `-- --test-threads=1`. If
the serial run passes, bisect the test set with filters, keeping parallel
execution enabled to isolate the tests that interact.

The pre-commit hook (`cargo xtask install-hooks`) runs the policy check. If the
repository folder was moved, run `cargo clean` first: `xtask` bakes in its
source path.

## Reporting

End with what changed (files), the gate results with any failing output, and
what was not checked. Do not claim a speed gain without a measurement.
