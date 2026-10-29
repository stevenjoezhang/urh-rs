/**
 * Pure selection math (P1-remainder). The *state* lives in the signal view
 * model as a `SelectionRange` (drag anchor + head, either order, exactly as
 * the drag produced it); everything downstream — analysis ranges, protocol
 * table highlighting, zoom-to-selection — goes through the normalized
 * half-open {@link Interval} produced here, so there is a single source of
 * truth and a single place where ordering/emptiness is decided.
 *
 * Half-open semantics throughout: `[start, end)`, `start < end`, integer
 * sample indices. A selection whose anchor and head coincide is *empty* and
 * normalizes to `null`.
 */

import { type SelectionRange, type ViewRange, type Interval, viewSampleToX } from "../render/WaveformRenderer";

export type { Interval };

/**
 * The selection as a half-open sample interval, or `null` when there is no
 * (non-empty) selection. The raw anchor/head order is discarded here.
 */
export function normalizeSelectionInterval(sel: SelectionRange | null): Interval | null {
  if (sel === null) return null;
  if (sel.selStart === sel.selEnd) return null;
  return {
    start: Math.min(sel.selStart, sel.selEnd),
    end: Math.max(sel.selStart, sel.selEnd),
  };
}

/** View that exactly frames the selection (zoom-to-selection target). */
export function viewForSelection(sel: SelectionRange | null): ViewRange | null {
  const iv = normalizeSelectionInterval(sel);
  return iv === null ? null : { start: iv.start, end: iv.end };
}

/** Half-open overlap test: touching intervals (`a.end === b.start`) do NOT overlap. */
export function intervalsOverlap(a: Interval, b: Interval): boolean {
  return a.start < b.end && b.start < a.end;
}

/** Which selection edge (if any) is within `hitPx` CSS pixels of `x`. */
export type SelectionEdge = "start" | "end";

export function selectionEdgeAt(
  sel: SelectionRange | null,
  view: ViewRange,
  cssW: number,
  x: number,
  hitPx: number,
): SelectionEdge | null {
  const iv = normalizeSelectionInterval(sel);
  if (iv === null) return null;
  const xStart = viewSampleToX(view, cssW, iv.start);
  const xEnd = viewSampleToX(view, cssW, iv.end);
  // Both edges checked even when the band is narrower than 2*hitPx; the
  // nearer one wins so the pointer never grabs the far edge through the band.
  const dStart = Math.abs(x - xStart);
  const dEnd = Math.abs(x - xEnd);
  if (dStart <= hitPx && dStart <= dEnd) return "start";
  if (dEnd <= hitPx) return "end";
  return null;
}

/**
 * Selection after dragging one edge to `sample` (the other edge stays put).
 * The result keeps the anchor/head shape so it round-trips through
 * `useSignalView.setSelection`; `moveSelectionEdge` writes the fixed edge
 * into the anchor and the moved sample into the head.
 */
export function moveSelectionEdge(
  sel: SelectionRange | null,
  edge: SelectionEdge,
  sample: number,
): SelectionRange | null {
  const iv = normalizeSelectionInterval(sel);
  if (iv === null) return null;
  const fixed = edge === "start" ? iv.end : iv.start;
  return { selStart: fixed, selEnd: sample };
}

/** Set an ordered interval as the selection (table/programmatic selection). */
export function selectionFromInterval(iv: Interval): SelectionRange {
  return { selStart: iv.start, selEnd: iv.end };
}
