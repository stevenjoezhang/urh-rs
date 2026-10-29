# ADR-003: Capture/recording queue policy

Status: deferred (to be decided in P3) — recorded 2026-10-29

## Issue

`urh::devices::Device::start_rx` takes a `std::sync::mpsc::Sender<IqChunk>` —
an unbounded channel. If live capture outpaces the consumer, memory grows
without bound. The GUI application layer must own a bounded buffer, an
overflow policy, and explicit drop accounting.

## Direction (not yet binding)

- Recording and preview are separate consumers with separate bounded queues;
  recording has priority.
- Preview frames may be dropped oldest-first; recorded samples must not be
  silently dropped — loss must be counted and surfaced in the UI.
- The driver callback thread only moves chunks, never blocks on UI or DSP.

## Why deferred

The P0/P1 slice is offline-only and does not touch `Device`. Deciding queue
sizes, overflow behavior and disk-full recovery requires measurements against
a real device (P3), and changing the `Device` trait is explicitly out of
scope for the first slice. This ADR exists so the decision is not forgotten.
