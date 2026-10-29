import { describe, expect, it } from "vitest";

import {
  type BinComponent,
  type DecodedEnvelope,
  ProtocolError,
  WaveformMode,
  binValue,
  decodeEnvelope,
  floatsPerBin,
} from "./protocol";

const HEADER = 24;

interface HeaderFields {
  magic?: number; // little-endian u32, default b"URH1"
  version?: number;
  dtype?: number;
  requestId: number;
  nBins: number;
  mode: number;
  flags?: number;
}

/** Build a frame with a raw payload of exactly `payloadBytes` bytes. */
function rawFrame(h: HeaderFields, payloadBytes: number): ArrayBuffer {
  const buf = new ArrayBuffer(HEADER + payloadBytes);
  const dv = new DataView(buf);
  dv.setUint32(0, h.magic ?? 0x31524855, true);
  dv.setUint16(4, h.version ?? 1, true);
  dv.setUint16(6, h.dtype ?? 1, true);
  dv.setUint32(8, h.requestId, true);
  dv.setUint32(12, h.nBins, true);
  dv.setUint32(16, h.mode, true);
  dv.setUint32(20, h.flags ?? 0, true);
  return buf;
}

/**
 * Build a valid frame from per-bin component values. `values[k * fpb + c]`
 * is bin k's component c (Iq: [min_i, max_i, min_q, max_q];
 * Amplitude: [min_mag, max_mag]).
 */
function frame(h: HeaderFields, values: number[]): ArrayBuffer {
  const buf = rawFrame(h, values.length * 4);
  new Float32Array(buf, HEADER).set(values);
  return buf;
}

/** Half-open expectation helper: exact f32 round-trip values. */
function iqBins(n: number): number[] {
  const out: number[] = [];
  for (let k = 0; k < n; k++) {
    out.push(-0.5 - k / 4, 0.25 + k / 4, 0.125 * k, -0.75 + k / 8);
  }
  return out;
}

function magBins(n: number): number[] {
  const out: number[] = [];
  for (let k = 0; k < n; k++) {
    out.push(k / 8, 0.5 + k / 4);
  }
  return out;
}

function expectProtocolError(frame: unknown, reason: string): void {
  try {
    decodeEnvelope(frame as ArrayBuffer);
  } catch (e) {
    expect(e).toBeInstanceOf(ProtocolError);
    const err = e as ProtocolError;
    expect(err.reason).toBe(reason);
    return;
  }
  expect.fail(`expected decodeEnvelope to throw (${reason})`);
}

describe("decodeEnvelope — Iq mode", () => {
  it("decodes header fields and payload values", () => {
    const n = 4;
    const env = decodeEnvelope(frame({ requestId: 42, nBins: n, mode: 1 }, iqBins(n)));
    expect(env.requestId).toBe(42);
    expect(env.mode).toBe(WaveformMode.Iq);
    expect(env.nBins).toBe(n);
    expect(floatsPerBin(env.mode)).toBe(4);
    // interleaved order: [min_i, max_i, min_q, max_q] per bin
    expect(Array.from(env.data)).toEqual(iqBins(n));
  });

  it("supports odd n_bins", () => {
    const n = 7;
    const env = decodeEnvelope(frame({ requestId: 7, nBins: n, mode: 1 }, iqBins(n)));
    expect(env.nBins).toBe(7);
    expect(env.data.length).toBe(28);
    expect(Array.from(env.data)).toEqual(iqBins(n));
  });

  it("echoes request_id, including large u32 values", () => {
    const n = 3;
    const env = decodeEnvelope(frame({ requestId: 0xffffffff, nBins: n, mode: 1 }, iqBins(n)));
    expect(env.requestId).toBe(0xffffffff);
  });

  it("returns views into the frame buffer (no copy)", () => {
    const n = 5;
    const buf = frame({ requestId: 1, nBins: n, mode: 1 }, iqBins(n));
    const env = decodeEnvelope(buf);
    expect(env.data.buffer).toBe(buf);
    expect(env.data.byteOffset).toBe(HEADER);
  });

  it("binValue reads named components with the right stride", () => {
    const n = 3;
    const env = decodeEnvelope(frame({ requestId: 1, nBins: n, mode: 1 }, iqBins(n)));
    const cases: Array<[BinComponent, number, number]> = [
      ["iMin", 0, -0.5],
      ["iMax", 0, 0.25],
      ["qMin", 0, 0],
      ["qMax", 0, -0.75],
      ["iMin", 2, -1],
      ["iMax", 2, 0.75],
      ["qMin", 2, 0.25],
      ["qMax", 2, -0.5],
    ];
    for (const [component, bin, expected] of cases) {
      expect(binValue(env, component, bin)).toBeCloseTo(expected, 12);
    }
    expect(() => binValue(env, "iMin", n)).toThrow(RangeError);
  });

  it("accepts a Uint8Array view of the same bytes", () => {
    const n = 3;
    const frameBuf = frame({ requestId: 9, nBins: n, mode: 1 }, iqBins(n));
    // embed the frame at a non-zero offset inside a larger buffer and view it
    const big = new ArrayBuffer(8 + frameBuf.byteLength);
    new Uint8Array(big, 8).set(new Uint8Array(frameBuf));
    const view = new Uint8Array(big, 8, frameBuf.byteLength);
    const env = decodeEnvelope(view);
    expect(env.requestId).toBe(9);
    expect(env.data.buffer).toBe(big);
    expect(env.data.byteOffset).toBe(8 + HEADER);
  });
});

describe("decodeEnvelope — Amplitude mode", () => {
  it("decodes magMin/magMax pairs", () => {
    const n = 4;
    const env = decodeEnvelope(frame({ requestId: 5, nBins: n, mode: 2 }, magBins(n)));
    expect(env.mode).toBe(WaveformMode.Amplitude);
    expect(floatsPerBin(env.mode)).toBe(2);
    expect(Array.from(env.data)).toEqual(magBins(n));
  });

  it("supports odd n_bins", () => {
    const n = 5;
    const env = decodeEnvelope(frame({ requestId: 5, nBins: n, mode: 2 }, magBins(n)));
    expect(env.nBins).toBe(5);
    expect(env.data.length).toBe(10);
  });

  it("binValue reads mag components with the right stride", () => {
    const n = 3;
    const env = decodeEnvelope(frame({ requestId: 5, nBins: n, mode: 2 }, magBins(n)));
    expect(binValue(env, "magMin", 1)).toBeCloseTo(0.125, 12);
    expect(binValue(env, "magMax", 1)).toBeCloseTo(0.75, 12);
  });
});

describe("decodeEnvelope — rejections", () => {
  it("rejects a bad magic", () => {
    const n = 2;
    expectProtocolError(frame({ magic: 0x31524856, requestId: 1, nBins: n, mode: 1 }, iqBins(n)), "magic");
  });

  it("rejects a bad version", () => {
    const n = 2;
    expectProtocolError(frame({ version: 2, requestId: 1, nBins: n, mode: 1 }, iqBins(n)), "version");
  });

  it("rejects a bad dtype", () => {
    const n = 2;
    expectProtocolError(frame({ dtype: 2, requestId: 1, nBins: n, mode: 1 }, iqBins(n)), "dtype");
  });

  it("rejects an unknown mode", () => {
    const n = 2;
    const buf = frame({ requestId: 1, nBins: n, mode: 1 }, iqBins(n));
    new DataView(buf).setUint32(16, 3, true); // 3 = Phase, reserved
    expectProtocolError(buf, "mode");
  });

  it("rejects a buffer shorter than the header", () => {
    expectProtocolError(new ArrayBuffer(23), "buffer");
    expectProtocolError(new Uint8Array(8), "buffer");
  });

  it("rejects n_bins = 0", () => {
    expectProtocolError(rawFrame({ requestId: 1, nBins: 0, mode: 1 }, 0), "n-bins");
  });

  it("rejects a truncated payload", () => {
    const n = 4;
    const full = frame({ requestId: 1, nBins: n, mode: 1 }, iqBins(n)); // 24 + 64 bytes
    const truncated = full.slice(0, full.byteLength - 8);
    expectProtocolError(truncated, "truncated-payload");
  });

  it("rejects n_bins x payload-size mismatch (extra trailing bytes)", () => {
    const n = 4;
    const full = frame({ requestId: 1, nBins: n, mode: 1 }, iqBins(n));
    const padded = new ArrayBuffer(full.byteLength + 4);
    new Uint8Array(padded).set(new Uint8Array(full));
    expectProtocolError(padded, "payload-size");
  });

  it("rejects an Amplitude-sized payload declared as Iq", () => {
    const n = 4;
    const buf = frame({ requestId: 1, nBins: n, mode: 2 }, magBins(n)); // 8n payload
    new DataView(buf).setUint32(16, 1, true); // relabel as Iq (needs 16n)
    expectProtocolError(buf, "truncated-payload");
  });

  it("rejects a misaligned Uint8Array view", () => {
    const n = 2;
    const buf = frame({ requestId: 1, nBins: n, mode: 1 }, iqBins(n));
    const odd = new Uint8Array(buf, 1); // payload would start at odd offset
    expectProtocolError(odd, "buffer");
  });

  it("rejects a non-buffer input", () => {
    expectProtocolError(null, "buffer");
    expectProtocolError("URH1", "buffer");
  });
});

describe("decodeEnvelope — degenerate envelopes", () => {
  it("decodes min == max == sample frames like any other", () => {
    // one bin per sample, min == max == sample value (contract envelope rule 2)
    const n = 6;
    const values: number[] = [];
    for (let k = 0; k < n; k++) values.push(k / 8, k / 8, -k / 8, -k / 8);
    const env: DecodedEnvelope = decodeEnvelope(frame({ requestId: 3, nBins: n, mode: 1 }, values));
    for (let k = 0; k < n; k++) {
      expect(binValue(env, "iMin", k)).toBeCloseTo(binValue(env, "iMax", k), 12);
    }
  });
});
