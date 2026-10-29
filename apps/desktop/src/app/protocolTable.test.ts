/**
 * Protocol table view-model tests (P2): bits→hex, middle truncation,
 * selection-overlap highlighting, viewport slicing, message framing and
 * AWRE label layout.
 */

import { describe, expect, it } from "vitest";

import type { MessageInfo } from "../bridge/api";
import {
  BITS_PREVIEW_MAX,
  ROW_HEIGHT,
  ROW_OVERSCAN,
  bitsToHex,
  labelColor,
  labelSegments,
  messageOverlapsSelection,
  truncateMiddle,
  viewForMessage,
  visibleRows,
} from "./protocolTable";

function message(overrides: Partial<MessageInfo> = {}): MessageInfo {
  return {
    index: 0,
    bitCount: 8,
    pause: 100,
    plainBits: "01011010",
    startSample: 1000,
    endSample: 1800,
    ...overrides,
  };
}

describe("bitsToHex", () => {
  it("converts complete nibbles", () => {
    expect(bitsToHex("")).toBe("");
    expect(bitsToHex("0101")).toBe("5");
    expect(bitsToHex("01011010")).toBe("5a");
    expect(bitsToHex("11111111")).toBe("ff");
    expect(bitsToHex("0000000100100011")).toBe("0123");
  });

  it("right-pads the final partial nibble (URH semantics)", () => {
    expect(bitsToHex("1")).toBe("8");
    expect(bitsToHex("01")).toBe("4");
    expect(bitsToHex("010")).toBe("4");
    expect(bitsToHex("0101101")).toBe("5a"); // 0101 101(0)
  });
});

describe("truncateMiddle", () => {
  it("returns short strings unchanged", () => {
    expect(truncateMiddle("0101", 10)).toBe("0101");
    expect(truncateMiddle("010110101001", 12)).toBe("010110101001");
  });

  it("keeps head and tail around the ellipsis at exactly max length", () => {
    const out = truncateMiddle("0101101010010110", 9);
    expect(out.length).toBe(9);
    expect(out).toBe("0101…0110");
    expect(out.startsWith("0101")).toBe(true);
    expect(out.endsWith("0110")).toBe(true);
  });

  it("handles odd and even budgets", () => {
    expect(truncateMiddle("0101101010010110", 8)).toBe("0101…110");
    expect(truncateMiddle("0101101010010110", 7)).toBe("010…110");
  });

  it("never truncates below the ellipsis floor", () => {
    expect(truncateMiddle("01011010", 3)).toBe("01011010");
  });

  it("previews long messages at the table's default budget", () => {
    const bits = "01".repeat(200);
    const out = truncateMiddle(bits, BITS_PREVIEW_MAX);
    expect(out.length).toBe(BITS_PREVIEW_MAX);
    expect(out).toContain("…");
  });
});

describe("messageOverlapsSelection (bidirectional highlight)", () => {
  const sel = { start: 1000, end: 2000 };

  it("highlights messages straddling either edge", () => {
    expect(messageOverlapsSelection(message({ startSample: 500, endSample: 1500 }), sel)).toBe(true);
    expect(messageOverlapsSelection(message({ startSample: 1500, endSample: 2500 }), sel)).toBe(true);
  });

  it("highlights contained and containing messages", () => {
    expect(messageOverlapsSelection(message({ startSample: 1200, endSample: 1800 }), sel)).toBe(true);
    expect(messageOverlapsSelection(message({ startSample: 0, endSample: 5000 }), sel)).toBe(true);
  });

  it("does not highlight touching or disjoint messages (half-open)", () => {
    expect(messageOverlapsSelection(message({ startSample: 0, endSample: 1000 }), sel)).toBe(false);
    expect(messageOverlapsSelection(message({ startSample: 2000, endSample: 3000 }), sel)).toBe(false);
  });

  it("is false without a selection", () => {
    expect(messageOverlapsSelection(message(), null)).toBe(false);
  });
});

describe("visibleRows (viewport slicing)", () => {
  it("renders from the top with overscan on both ends", () => {
    const { first, count } = visibleRows(0, 440, 1000);
    expect(first).toBe(0);
    expect(count).toBe(Math.ceil(440 / ROW_HEIGHT) + ROW_OVERSCAN);
  });

  it("scrolls the slice with the viewport", () => {
    const { first } = visibleRows(ROW_HEIGHT * 10, 440, 1000);
    expect(first).toBe(10 - ROW_OVERSCAN);
  });

  it("clamps the slice near the end (overscan above the last rows)", () => {
    const { first, count } = visibleRows(ROW_HEIGHT * 999, 440, 1000);
    expect(first).toBe(999 - ROW_OVERSCAN);
    expect(count).toBe(1 + ROW_OVERSCAN);
  });

  it("never exceeds the row count for small tables", () => {
    const { first, count } = visibleRows(0, 4000, 3);
    expect(first).toBe(0);
    expect(count).toBe(3);
  });

  it("returns nothing for empty tables or invisible viewports", () => {
    expect(visibleRows(0, 100, 0)).toEqual({ first: 0, count: 0 });
    expect(visibleRows(0, 0, 10)).toEqual({ first: 0, count: 0 });
  });
});

describe("viewForMessage (scroll a message into view)", () => {
  it("frames the message with a quarter-span of padding", () => {
    expect(viewForMessage(message({ startSample: 1000, endSample: 1800 }))).toEqual({
      start: 800,
      end: 2000,
    });
  });

  it("applies a minimum padding for short messages", () => {
    expect(viewForMessage(message({ startSample: 1000, endSample: 1010 }))).toEqual({
      start: 936,
      end: 1074,
    });
  });

  it("targets may leave [0, total); the view model clamps", () => {
    const target = viewForMessage(message({ startSample: 0, endSample: 10 }));
    expect(target.start).toBeLessThan(0); // clamped by setView downstream
  });
});

describe("labelSegments (AWRE label bars)", () => {
  const labels = [
    { messageIndex: 0, name: "preamble", bitStart: 0, bitEnd: 8, colorIndex: 0 },
    { messageIndex: 0, name: "sync", bitStart: 8, bitEnd: 16, colorIndex: 3 },
    { messageIndex: 1, name: "length", bitStart: 0, bitEnd: 4, colorIndex: 1 },
    { messageIndex: 3, name: "other message", bitStart: 0, bitEnd: 4, colorIndex: 2 },
  ];

  it("keeps only the requested message's labels, laid out as fractions", () => {
    const segs = labelSegments(labels, 0, 32);
    expect(segs).toHaveLength(2);
    expect(segs[0]).toMatchObject({ name: "preamble", left: 0, width: 0.25 });
    expect(segs[1]).toMatchObject({ name: "sync", left: 0.25, width: 0.25 });
  });

  it("sorts left-to-right regardless of input order", () => {
    const reversed = [...labels].reverse();
    const segs = labelSegments(reversed, 0, 32);
    expect(segs.map((s) => s.name)).toEqual(["preamble", "sync"]);
  });

  it("clamps out-of-range bit positions into the message", () => {
    const segs = labelSegments(
      [{ messageIndex: 0, name: "tail", bitStart: 30, bitEnd: 99, colorIndex: 0 }],
      0,
      32,
    );
    expect(segs[0]).toMatchObject({ bitStart: 30, bitEnd: 32, left: 30 / 32, width: 2 / 32 });
  });

  it("drops labels that clamp away entirely", () => {
    expect(labelSegments([{ messageIndex: 0, name: "gone", bitStart: 40, bitEnd: 50, colorIndex: 0 }], 0, 32)).toEqual([]);
    expect(labelSegments(labels, 0, 0)).toEqual([]);
  });

  it("maps color indices through the palette modulo its length", () => {
    expect(labelColor(0)).toBe(labelColor(8));
    expect(labelColor(3)).not.toBe(labelColor(4));
  });
});
