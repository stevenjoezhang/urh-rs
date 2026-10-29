# ADR-001: Workspace layout — keep the root package, add layers as members

Status: accepted (2026-10-29)

## Decision

The root package stays at the repository root and keeps the name `urh` (the
published crate). GUI layers are added as workspace members:

```
Cargo.toml            # package urh + [workspace] (default-members = ["."])
crates/urh-app/       # service layer: SignalRepository, envelopes, jobs
apps/desktop/         # React + TypeScript + Vite frontend
apps/desktop/src-tauri/  # Tauri 2 shell (thin command adapter)
```

`default-members = ["."]` so plain `cargo test` / `cargo clippy` at the root
keep covering only the `urh` crate — the existing CI jobs and the
`tests/cli_compare.rs` Python ground-truth flow are untouched. The GUI is
built explicitly in the `gui` CI job, which installs the Linux webkit/gtk
dev dependencies the Tauri shell needs.

## Consequences

- No source moves; the bit-for-bit test history of the core stays attached to
  the same paths.
- The Tauri app cannot build on machines without webkit2gtk dev packages —
  accepted, because CI covers it and the core must never require GUI deps.
- `cargo publish` from the root publishes only the `urh` crate; `urh-app` is
  not (yet) published.

## Alternatives considered

- Moving the core into `crates/urh-core/` and making the root a virtual
  manifest: cleaner long-term, but it rewrites every path in the test
  infrastructure (`URH_REF_SRC` handling, CI working directories) for zero
  functional gain in the first slice.
