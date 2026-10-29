/**
 * Selection math tests (P1-remainder): drag → interval normalization,
 * clamping behavior through the view model's contract, edge hit-testing and
 * edge moves, overlap semantics, and the zoom-to-selection target.
 */

import { describe, expect, it } from "vitest";

import type { SelectionRange, ViewRange } from "../render/WaveformRenderer";
import {
  intervalsOverlap,
  moveSelectionEdge,
  normalizeSelectionInterval,
  selectionEdgeAt,
  selectionFromInterval,
  viewForSelection,
} from "./selection";
import { clampView } from "./useSignalView";

describe("normalizeSelectionInterval", () => {
  it("orders anchor/head regardless of drag direction", () => {
    expect(normalizeSelectionInterval({ selStart: 100, selEnd: 50 })).toEqual({ start: 50, end: 100 });
    expect(normalizeSelectionInterval({ selStart: 50, selEnd: 100 })).toEqual({ start: 50, end: 100 });
  });

  it("treats a zero-width (click) selection as no selection", () => {
    expect(normalizeSelectionInterval({ selStart: 42, selEnd: 42 })).toBeNull();
  });

  it("passes null through", () => {
    expect(normalizeSelectionInterval(null)).toBeNull();
  });

  it("keeps single-sample selections (half-open [n, n+1))", () => {
    expect(normalizeSelectionInterval({ selStart: 7, selEnd: 8 })).toEqual({ start: 7, end: 8 });
  });
});

describe("viewForSelection (zoom-to-selection target)", () => {
  it("frames exactly the interval", () => {
    expect(viewForSelection({ selStart: 900, selEnd: 100 })).toEqual({ start: 100, end: 900 });
  });

  it("is null without a usable selection", () => {
    expect(viewForSelection(null)).toBeNull();
    expect(viewForSelection({ selStart: 5, selEnd: 5 })).toBeNull();
  });

  it("clamps into the signal through the view model's clampView", () => {
    // A selection running past the end cannot occur (setSelection clamps),
    // but the composition must still stay in bounds: clampView keeps the
    // span and shifts the window back to fit.
    const target = viewForSelection({ selStart: 950, selEnd: 1200 });
    expect(target).toEqual({ start: 950, end: 1200 });
    expect(clampView(target as ViewRange, 1000)).toEqual({ start: 750, end: 1000 });
  });

  it("respects the MIN_SPAN zoom floor via clampView (anchored at start)", () => {
    const target = viewForSelection({ selStart: 10, selEnd: 12 });
    expect(clampView(target as ViewRange, 100000)).toEqual({ start: 10, end: 26 });
  });
});

describe("intervalsOverlap (half-open)", () => {
  it("is true for partial overlap in either order", () => {
    expect(intervalsOverlap({ start: 10, end: 20 }, { start: 15, end: 25 })).toBe(true);
    expect(intervalsOverlap({ start: 15, end: 25 }, { start: 10, end: 20 })).toBe(true);
  });

  it("is true for containment", () => {
    expect(intervalsOverlap({ start: 0, end: 100 }, { start: 10, end: 20 })).toBe(true);
  });

  it("is false for touching intervals", () => {
    expect(intervalsOverlap({ start: 10, end: 20 }, { start: 20, end: 30 })).toBe(false);
  });

  it("is false for disjoint intervals", () => {
    expect(intervalsOverlap({ start: 10, end: 20 }, { start: 30, end: 40 })).toBe(false);
  });

  it("treats a degenerate (zero-length) interval as its point: inside = overlap", () => {
    // Selections are normalized (empty -> null) so this only matters for
    // zero-extent messages; the canonical formula reports point containment.
    expect(intervalsOverlap({ start: 10, end: 10 }, { start: 0, end: 100 })).toBe(true);
    expect(intervalsOverlap({ start: 10, end: 10 }, { start: 20, end: 30 })).toBe(false);
  });
});

describe("selectionEdgeAt (handle hit-testing)", () => {
  const view: ViewRange = { start: 0, end: 1000 };
  const cssW = 500; // 2 samples per px

  it("finds the start and end edges within the hit tolerance", () => {
    const sel: SelectionRange = { selStart: 100, selEnd: 500 };
    expect(selectionEdgeAt(sel, view, cssW, 100 / 2, 6)).toBe("start");
    expect(selectionEdgeAt(sel, view, cssW, 500 / 2, 6)).toBe("end");
    // just outside tolerance
    expect(selectionEdgeAt(sel, view, cssW, 100 / 2 - 7, 6)).toBeNull();
  });

  it("prefers the nearer edge when the band is narrower than 2x tolerance", () => {
    // 0.5 px/sample: samples 800..820 map to x 400..410.
    const sel: SelectionRange = { selStart: 800, selEnd: 820 };
    // x=405 is 5px from start(400) and 5px from end(410) -> start wins by tie rule
    expect(selectionEdgeAt(sel, view, cssW, 405, 6)).toBe("start");
    // x=407 is 7 from start (outside tolerance), 3 from end -> end
    expect(selectionEdgeAt(sel, view, cssW, 407, 6)).toBe("end");
  });

  it("returns null inside the band away from the edges", () => {
    const sel: SelectionRange = { selStart: 100, selEnd: 900 };
    expect(selectionEdgeAt(sel, view, cssW, 250, 6)).toBeNull();
  });

  it("returns null without a selection (or an empty one)", () => {
    expect(selectionEdgeAt(null, view, cssW, 10, 6)).toBeNull();
    expect(selectionEdgeAt({ selStart: 5, selEnd: 5 }, view, cssW, 5 / 2, 6)).toBeNull();
  });
});

describe("moveSelectionEdge", () => {
  it("moves the start edge, keeping the end fixed", () => {
    const next = moveSelectionEdge({ selStart: 100, selEnd: 500 }, "start", 50);
    expect(normalizeSelectionInterval(next)).toEqual({ start: 50, end: 500 });
  });

  it("moves the end edge, keeping the start fixed", () => {
    const next = moveSelectionEdge({ selStart: 100, selEnd: 500 }, "end", 700);
    expect(normalizeSelectionInterval(next)).toEqual({ start: 100, end: 700 });
  });

  it("crossing the other edge swaps the order (interval just renormalizes)", () => {
    const next = moveSelectionEdge({ selStart: 100, selEnd: 500 }, "start", 600);
    expect(normalizeSelectionInterval(next)).toEqual({ start: 500, end: 600 });
  });

  it("collapsing onto the other edge yields an empty (null) interval", () => {
    const next = moveSelectionEdge({ selStart: 100, selEnd: 500 }, "end", 100);
    expect(normalizeSelectionInterval(next)).toBeNull();
  });

  it("returns null without a selection", () => {
    expect(moveSelectionEdge(null, "start", 5)).toBeNull();
  });
});

describe("selectionFromInterval", () => {
  it("round-trips through normalize", () => {
    const sel = selectionFromInterval({ start: 30, end: 80 });
    expect(sel).toEqual({ selStart: 30, selEnd: 80 });
    expect(normalizeSelectionInterval(sel)).toEqual({ start: 30, end: 80 });
  });
});
