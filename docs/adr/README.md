# ADRs

Architecture decision records for the GUI effort. Each ADR is immutable once
accepted; superseding an ADR means writing a new one that references it.

- [ADR-001](ADR-001-workspace-layout.md) — keep the root `urh` package, add GUI layers as workspace members
- [ADR-002](ADR-002-signal-storage.md) — MVP loads whole files in memory; paged/mmap backend later
- [ADR-003](ADR-003-capture-queues.md) — capture/recording queue policy (deferred to P3)
- [ADR-004](ADR-004-coordinates-and-framing.md) — integer sample coordinates, half-open ranges, URH1 binary frame
- [ADR-005](ADR-005-hardware-acceptance.md) — hardware acceptance devices (deferred to P3)
