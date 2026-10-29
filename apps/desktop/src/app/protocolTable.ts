/**
 * Protocol table view-model (P2) — pure formatting/slicing math, no React,
 * no DOM; the component stays a thin render loop over these helpers.
 *
 * Virtualization is by slicing: only the rows intersecting the scroll
 * viewport (plus a small overscan) exist in the DOM, at a fixed row height,
 * so memory and node count do not grow with the message count.
 */

import type { FieldLabel, MessageInfo } from "../bridge/api";
import type { Interval } from "../render/WaveformRenderer";
import { intervalsOverlap } from "./selection";

/** Fixed row height (px) — the virtualization grid depends on it. */
export const ROW_HEIGHT = 44;

/** Rows rendered above/below the viewport to make scrolling seamless. */
export const ROW_OVERSCAN = 4;

/** Default character budget for the bits preview column. */
export const BITS_PREVIEW_MAX = 64;

/**
 * Bits ("0101…") -> hex string. Incomplete final nibbles are right-padded
 * with zeros (URH semantics: the trailing bits form the low bits of the last
 * nibble), so `"1"` -> `"8"` and `"01011010"` -> `"5a"`.
 */
export function bitsToHex(bits: string): string {
  let out = "";
  for (let i = 0; i < bits.length; i += 4) {
    let nibble = 0;
    for (let j = 0; j < 4; j++) {
      nibble = nibble * 2 + (bits[i + j] === "1" ? 1 : 0);
    }
    out += nibble.toString(16);
  }
  return out;
}

/**
 * Middle-truncate a string to at most `max` characters, keeping head and
 * tail and joining them with an ellipsis character. Lengths below 4 cannot
 * express head+ellipsis+tail and are returned unchanged (when short enough).
 */
export function truncateMiddle(s: string, max: number): string {
  if (max < 4 || s.length <= max) return s;
  const keep = max - 1; // the ellipsis costs one character
  const headLen = Math.ceil(keep / 2);
  const tailLen = keep - headLen;
  return `${s.slice(0, headLen)}…${s.slice(s.length - tailLen)}`;
}

/**
 * Does the message's `[start_sample, end_sample)` overlap the selection
 * interval? Half-open semantics: a message ending exactly where the
 * selection starts (or vice versa) does NOT overlap.
 */
export function messageOverlapsSelection(m: MessageInfo, sel: Interval | null): boolean {
  if (sel === null) return false;
  return intervalsOverlap({ start: m.startSample, end: m.endSample }, sel);
}

/** Which rows to render for a scroll position: `[first, first + count)`. */
export function visibleRows(
  scrollTop: number,
  viewportHeight: number,
  totalRows: number,
): { first: number; count: number } {
  if (totalRows === 0 || viewportHeight <= 0) return { first: 0, count: 0 };
  const first = Math.max(0, Math.min(Math.floor(scrollTop / ROW_HEIGHT) - ROW_OVERSCAN, totalRows - 1));
  const visible = Math.ceil((viewportHeight + (scrollTop - first * ROW_HEIGHT)) / ROW_HEIGHT);
  const count = Math.min(visible + ROW_OVERSCAN, totalRows - first);
  return { first, count: Math.max(0, count) };
}

/**
 * View that frames a message with breathing room on both sides: padding is
 * a quarter of the message span (min {@link MIN_MESSAGE_PAD} samples). The
 * caller's `setView` clamps into the signal; this only computes the target.
 */
export const MIN_MESSAGE_PAD = 64;

export function viewForMessage(m: MessageInfo): { start: number; end: number } {
  const span = Math.max(m.endSample - m.startSample, 1);
  const pad = Math.max(Math.round(span / 4), MIN_MESSAGE_PAD);
  return { start: m.startSample - pad, end: m.endSample + pad };
}

/** One AWRE label positioned over a message's bits column. */
export interface LabelSegment {
  name: string;
  bitStart: number;
  bitEnd: number;
  colorIndex: number;
  /** Left edge as a fraction of the bits column [0, 1]. */
  left: number;
  /** Width as a fraction of the bits column (0, 1]. */
  width: number;
}

/**
 * Layout the AWRE labels belonging to one message as column fractions.
 * Labels are clamped into `[0, bitCount]`; degenerate (empty) messages or
 * fully out-of-range labels contribute nothing.
 */
export function labelSegments(
  labels: readonly FieldLabel[],
  messageIndex: number,
  bitCount: number,
): LabelSegment[] {
  if (bitCount <= 0) return [];
  const out: LabelSegment[] = [];
  for (const l of labels) {
    if (l.messageIndex !== messageIndex) continue;
    const start = Math.max(0, Math.min(l.bitStart, bitCount));
    const end = Math.max(0, Math.min(l.bitEnd, bitCount));
    if (end <= start) continue;
    out.push({
      name: l.name,
      bitStart: start,
      bitEnd: end,
      colorIndex: l.colorIndex,
      left: start / bitCount,
      width: (end - start) / bitCount,
    });
  }
  // Left-to-right so overlapping fields stack in reading order.
  out.sort((a, b) => a.bitStart - b.bitStart || b.bitEnd - a.bitEnd);
  return out;
}

/** Palette for AWRE label bars, indexed by `color_index` (modulo length). */
export const LABEL_COLORS = [
  "#e57373",
  "#64b5f6",
  "#81c784",
  "#ffb74d",
  "#ba68c8",
  "#4db6ac",
  "#f06292",
  "#a1887f",
] as const;

export function labelColor(colorIndex: number): string {
  return LABEL_COLORS[colorIndex % LABEL_COLORS.length];
}
