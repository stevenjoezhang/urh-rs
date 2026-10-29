/**
 * Simulator view-model (P5) — pure helpers, no React, no DOM: the transcript
 * bits preview (protocol-table parity) and the timeout field validation.
 * Sequencing lives in useSimulator; parsing in the bridge.
 */

import { BITS_PREVIEW_MAX, truncateMiddle } from "./protocolTable";

/**
 * Middle-truncated bits preview for a transcript entry — same budget and
 * same truncation style as the protocol table's bits column.
 */
export function simBitsPreview(bits: string): string {
  return truncateMiddle(bits, BITS_PREVIEW_MAX);
}

/** Default run timeout (seconds); also the initial editor value. */
export const DEFAULT_SIM_TIMEOUT_SECS = 30;

/** Editable timeout text (seconds). */
export type ParseSimTimeoutResult =
  | { ok: true; secs: number }
  | { ok: false; message: string };

/** Strip digit separators the rate input also tolerates. */
function parseNumberText(text: string): number {
  return Number.parseFloat(text.replace(/[,\s_]/g, ""));
}

/**
 * Validate the timeout field: a positive integer number of seconds (digit
 * separators tolerated, like every other numeric field in the app).
 */
export function parseSimTimeout(text: string): ParseSimTimeoutResult {
  const v = parseNumberText(text);
  if (!Number.isFinite(v) || !Number.isInteger(v) || v < 1) {
    return { ok: false, message: `"${text}" is not a positive integer number of seconds` };
  }
  return { ok: true, secs: v };
}
