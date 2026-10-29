/**
 * Simulator view-model tests (P5): the transcript bits preview (protocol
 * table parity: same budget, same middle truncation) and the timeout field
 * validation used by the Run button.
 */

import { describe, expect, it } from "vitest";

import { BITS_PREVIEW_MAX, truncateMiddle } from "./protocolTable";
import { DEFAULT_SIM_TIMEOUT_SECS, parseSimTimeout, simBitsPreview } from "./simulatorModel";

describe("simBitsPreview", () => {
  it("leaves short bit strings untouched", () => {
    expect(simBitsPreview("")).toBe("");
    expect(simBitsPreview("01011010")).toBe("01011010");
    expect(simBitsPreview("1".repeat(BITS_PREVIEW_MAX))).toBe("1".repeat(BITS_PREVIEW_MAX));
  });

  it("middle-truncates long strings to the protocol-table budget", () => {
    const bits = "01".repeat(200); // 400 chars
    expect(simBitsPreview(bits)).toBe(truncateMiddle(bits, BITS_PREVIEW_MAX));
    expect(simBitsPreview(bits).length).toBe(BITS_PREVIEW_MAX);
    expect(simBitsPreview(bits)).toContain("…");
    // head and tail survive
    const preview = simBitsPreview(bits);
    expect(preview.startsWith(bits.slice(0, preview.indexOf("…")))).toBe(true);
    expect(preview.endsWith(bits.slice(bits.length - (BITS_PREVIEW_MAX - 1 - preview.indexOf("…"))))).toBe(true);
  });
});

describe("parseSimTimeout", () => {
  it("parses the default and plain integers", () => {
    expect(parseSimTimeout(String(DEFAULT_SIM_TIMEOUT_SECS))).toEqual({ ok: true, secs: 30 });
    expect(parseSimTimeout("5")).toEqual({ ok: true, secs: 5 });
    expect(parseSimTimeout(" 120 ")).toEqual({ ok: true, secs: 120 });
  });

  it("tolerates digit separators like the other numeric fields", () => {
    expect(parseSimTimeout("1_000")).toEqual({ ok: true, secs: 1000 });
    expect(parseSimTimeout("1,000")).toEqual({ ok: true, secs: 1000 });
  });

  it("rejects zero, negative, fractional and non-numeric values", () => {
    for (const bad of ["0", "-1", "1.5", "soon", ""]) {
      expect(parseSimTimeout(bad).ok).toBe(false);
    }
  });
});
