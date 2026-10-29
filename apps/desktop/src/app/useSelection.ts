/**
 * Selection view-model (P1-remainder). The selection *state* stays in
 * `useSignalView` (single source of truth — the renderer overlay, the
 * analysis panel and the protocol table all read the same value); this hook
 * adds the normalized half-open {@link Interval} projection plus the actions
 * that need more than the raw state (zoom-to-selection, edge moves).
 *
 * Pure math lives in selection.ts and is unit-tested; this file is wiring.
 */

import { type Dispatch, type SetStateAction, useCallback, useMemo } from "react";

import { type SelectionRange, type ViewRange } from "../render/WaveformRenderer";
import {
  type Interval,
  moveSelectionEdge,
  normalizeSelectionInterval,
  viewForSelection,
} from "./selection";

export interface UseSelectionOptions {
  selection: SelectionRange | null;
  setSelection: (sel: SelectionRange | null) => void;
  clearSelection: () => void;
  setView: Dispatch<SetStateAction<ViewRange>>;
}

export function useSelection(options: UseSelectionOptions) {
  const { selection, setSelection, clearSelection, setView } = options;

  /** Normalized half-open `[start, end)`, or null when empty/absent. */
  const interval: Interval | null = useMemo(() => normalizeSelectionInterval(selection), [selection]);

  /** True when the interval is usable (zoom-to-selection, "selection only"). */
  const hasSelection = interval !== null;

  /** Zoom the view to exactly the selection (clamped by the view model). */
  const zoomToSelection = useCallback((): void => {
    const target = viewForSelection(selection);
    if (target === null) return;
    setView(target);
  }, [selection, setView]);

  /**
   * Move one selection edge to `sample` (edge drag). The other edge stays
   * put; an edge dragged across the other simply swaps the visual order.
   */
  const moveEdge = useCallback(
    (edge: "start" | "end", sample: number): void => {
      const next = moveSelectionEdge(selection, edge, sample);
      if (next !== null) setSelection(next);
    },
    [selection, setSelection],
  );

  /** Select an ordered interval programmatically (clamped by the view model). */
  const selectInterval = useCallback(
    (iv: Interval): void => {
      if (iv.start >= iv.end) return;
      setSelection({ selStart: iv.start, selEnd: iv.end });
    },
    [setSelection],
  );

  return {
    interval,
    hasSelection,
    zoomToSelection,
    moveEdge,
    selectInterval,
    clear: clearSelection,
  };
}

export type SelectionApi = ReturnType<typeof useSelection>;
