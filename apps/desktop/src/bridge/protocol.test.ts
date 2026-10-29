import { describe, expect, it } from "vitest";

import {
  type BinComponent,
  type DecodedEnvelope,
  CAPTURE_SAMPLE_BYTES,
  ProtocolError,
  WaveformMode,
  binValue,
  decodeCaptureBatchBody,
  decodeEnvelope,
  floatsPerBin,
  decodeSpectrogramTile,
  freqBinsForWindow,
  hopForWindow,
  isSpectrogramWindow,
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

// --- spectrogram tiles (v2) ------------------------------------------------------

/**
 * Build a spectrogram tile frame: header with dtype = 2, mode = 3, `nTimeBins`
 * time bins and a payload of `nTimeBins x freqBins` u8.
 */
function tileFrame(
  h: { requestId: number; nTimeBins: number; version?: number; dtype?: number; mode?: number },
  windowSize: 256 | 1024 | 4096,
  payload?: Uint8Array,
): ArrayBuffer {
  const freqBins = windowSize / 2;
  const bytes = payload ?? new Uint8Array(h.nTimeBins * freqBins);
  const buf = new ArrayBuffer(HEADER + bytes.length);
  const dv = new DataView(buf);
  dv.setUint32(0, 0x31524855, true); // b"URH1"
  dv.setUint16(4, h.version ?? 1, true);
  dv.setUint16(6, h.dtype ?? 2, true); // u8
  dv.setUint32(8, h.requestId, true);
  dv.setUint32(12, h.nTimeBins, true);
  dv.setUint32(16, h.mode ?? 3, true); // spectrogram
  dv.setUint32(20, 0, true);
  new Uint8Array(buf, HEADER).set(bytes);
  return buf;
}

describe("spectrogram window helpers", () => {
  it("couple freq bins and hop to the window size", () => {
    expect(freqBinsForWindow(256)).toBe(128);
    expect(freqBinsForWindow(1024)).toBe(512);
    expect(freqBinsForWindow(4096)).toBe(2048);
    expect(hopForWindow(1024)).toBe(512);
  });

  it("accept exactly the contract's window set", () => {
    for (const w of [256, 1024, 4096]) expect(isSpectrogramWindow(w)).toBe(true);
    for (const w of [0, 128, 512, 2048, 8192, 1024.5]) expect(isSpectrogramWindow(w)).toBe(false);
  });
});

describe("decodeSpectrogramTile", () => {
  /** Same rejection style as the envelope tests: catch + reason assert. */
  function expectTileError(frame: unknown, windowSize: 256 | 1024 | 4096, reason: string): void {
    try {
      decodeSpectrogramTile(frame as ArrayBuffer, windowSize);
    } catch (e) {
      expect(e).toBeInstanceOf(ProtocolError);
      const err = e as ProtocolError;
      expect(err.reason).toBe(reason);
      return;
    }
    expect.fail(`expected decodeSpectrogramTile to throw (${reason})`);
  }

  it("decodes header fields and the u8 payload as a view (no copy)", () => {
    const t = 3;
    const buf = tileFrame({ requestId: 77, nTimeBins: t }, 256);
    const payload = new Uint8Array(buf, HEADER);
    for (let i = 0; i < payload.length; i++) payload[i] = (i * 7) & 0xff;
    const tile = decodeSpectrogramTile(buf, 256);
    expect(tile.requestId).toBe(77);
    expect(tile.windowSize).toBe(256);
    expect(tile.freqBins).toBe(128);
    expect(tile.nTimeBins).toBe(3);
    expect(tile.data.length).toBe(3 * 128);
    expect(tile.data.buffer).toBe(buf);
    expect(tile.data.byteOffset).toBe(HEADER);
    // row-major: data[t * freqBins + f]
    expect(tile.data[0]).toBe(0);
    expect(tile.data[1]).toBe(7);
    expect(tile.data[128]).toBe(128 & 0xff);
  });

  it("accepts a zero time-bin tile with an empty payload (range < one hop)", () => {
    const tile = decodeSpectrogramTile(tileFrame({ requestId: 1, nTimeBins: 0 }, 1024), 1024);
    expect(tile.nTimeBins).toBe(0);
    expect(tile.data.length).toBe(0);
  });

  it("accepts an unaligned Uint8Array view (u8 payload needs no alignment)", () => {
    const base = tileFrame({ requestId: 5, nTimeBins: 1 }, 256);
    const big = new ArrayBuffer(3 + base.byteLength);
    new Uint8Array(big, 3).set(new Uint8Array(base));
    const view = new Uint8Array(big, 3, base.byteLength); // payload at odd offset
    const tile = decodeSpectrogramTile(view, 256);
    expect(tile.requestId).toBe(5);
    expect(tile.data.byteOffset).toBe(3 + HEADER);
  });

  it("rejects a bad magic", () => {
    const buf = tileFrame({ requestId: 1, nTimeBins: 1 }, 256);
    new DataView(buf).setUint32(0, 0x31524856, true);
    expectTileError(buf, 256, "magic");
  });

  it("rejects a bad version", () => {
    expectTileError(tileFrame({ requestId: 1, nTimeBins: 1, version: 2 }, 256), 256, "version");
  });

  it("rejects an f32 dtype (envelope frame passed to the tile decoder)", () => {
    expectTileError(tileFrame({ requestId: 1, nTimeBins: 1, dtype: 1 }, 256), 256, "dtype");
  });

  it("rejects an envelope mode", () => {
    expectTileError(tileFrame({ requestId: 1, nTimeBins: 1, mode: 1 }, 256), 256, "mode");
  });

  it("rejects a buffer shorter than the header", () => {
    expectTileError(new ArrayBuffer(23), 256, "buffer");
  });

  it("rejects a truncated payload", () => {
    const full = tileFrame({ requestId: 1, nTimeBins: 2 }, 256); // 24 + 2*128
    const truncated = full.slice(0, full.byteLength - 10);
    expectTileError(truncated, 256, "truncated-payload");
  });

  it("rejects extra trailing bytes", () => {
    const full = tileFrame({ requestId: 1, nTimeBins: 2 }, 256);
    const padded = new ArrayBuffer(full.byteLength + 1);
    new Uint8Array(padded).set(new Uint8Array(full));
    expectTileError(padded, 256, "payload-size");
  });

  it("rejects a payload sized for another window", () => {
    // 2 time bins of a 256-window tile (2*128 bytes) decoded as 1024 (needs 2*512)
    const buf = tileFrame({ requestId: 1, nTimeBins: 2 }, 256);
    expectTileError(buf, 1024, "truncated-payload");
  });

  it("rejects a non-buffer input", () => {
    expectTileError(null, 256, "buffer");
  });
});

// --- capture preview batch bodies (v2, P3) ----------------------------------------

/** Two known complex samples as LE f32 bytes: (1.5, -2.25) and (0, 0.5). */
function sampleBytes(): Uint8Array {
  const buf = new ArrayBuffer(16);
  const dv = new DataView(buf);
  dv.setFloat32(0, 1.5, true);
  dv.setFloat32(4, -2.25, true);
  dv.setFloat32(8, 0, true);
  dv.setFloat32(12, 0.5, true);
  return new Uint8Array(buf);
}

function expectBodyError(body: unknown, reason: string): void {
  try {
    decodeCaptureBatchBody(body);
  } catch (e) {
    expect(e).toBeInstanceOf(ProtocolError);
    expect((e as ProtocolError).reason).toBe(reason);
    return;
  }
  expect.fail(`expected decodeCaptureBatchBody to throw (${reason})`);
}

describe("decodeCaptureBatchBody", () => {
  it("fixes the wire sample size at 8 bytes", () => {
    expect(CAPTURE_SAMPLE_BYTES).toBe(8);
  });

  it("decodes an ArrayBuffer as a no-copy view with LE f32 values", () => {
    const bytes = sampleBytes();
    const iq = decodeCaptureBatchBody(bytes.buffer);
    expect(iq).toBeInstanceOf(Float32Array);
    expect(iq.buffer).toBe(bytes.buffer); // view, not a copy
    expect(Array.from(iq)).toEqual([1.5, -2.25, 0, 0.5]);
  });

  it("decodes an aligned Uint8Array view without copying", () => {
    const bytes = sampleBytes();
    const iq = decodeCaptureBatchBody(bytes);
    expect(iq.buffer).toBe(bytes.buffer);
    expect(iq.byteOffset).toBe(0);
    expect(Array.from(iq)).toEqual([1.5, -2.25, 0, 0.5]);
  });

  it("copies a misaligned Uint8Array view once (f32 needs 4-byte alignment)", () => {
    const padded = new ArrayBuffer(2 + 16);
    new Uint8Array(padded, 2).set(sampleBytes());
    const unaligned = new Uint8Array(padded, 2, 16);
    const iq = decodeCaptureBatchBody(unaligned);
    expect(iq.buffer).not.toBe(padded); // copied out of the misaligned view
    expect(Array.from(iq)).toEqual([1.5, -2.25, 0, 0.5]);
  });

  it("decodes a base64 string body", () => {
    const bytes = sampleBytes();
    const b64 = btoa(String.fromCharCode(...bytes));
    const iq = decodeCaptureBatchBody(b64);
    expect(Array.from(iq)).toEqual([1.5, -2.25, 0, 0.5]);
  });

  it("decodes a JSON byte array body", () => {
    const iq = decodeCaptureBatchBody(Array.from(sampleBytes()));
    expect(Array.from(iq)).toEqual([1.5, -2.25, 0, 0.5]);
  });

  it("round-trips a larger interleaved I/Q buffer", () => {
    const n = 2048; // one real preview batch worth of complex samples
    const src = new Float32Array(n * 2);
    for (let s = 0; s < n; s++) {
      src[s * 2] = Math.sin(s / 64);
      src[s * 2 + 1] = Math.cos(s / 32) * 0.5;
    }
    const iq = decodeCaptureBatchBody(new Uint8Array(src.buffer, src.byteOffset, src.byteLength));
    expect(iq.length).toBe(n * 2);
    for (let j = 0; j < iq.length; j++) expect(iq[j]).toBeCloseTo(src[j], 7);
  });

  it("rejects lengths that are not non-zero multiples of 8", () => {
    expectBodyError(new ArrayBuffer(0), "batch-size");
    expectBodyError(new ArrayBuffer(4), "batch-size");
    expectBodyError(new ArrayBuffer(12), "batch-size");
    expectBodyError([], "batch-size");
    expectBodyError(new Uint8Array(4), "batch-size");
  });

  it("rejects strings that are not base64", () => {
    expectBodyError("not base64 !!!", "batch-body");
  });

  it("rejects float sample arrays loudly (byte convention mismatch)", () => {
    let caught: unknown = null;
    try {
      decodeCaptureBatchBody([0.5, -2.25, 0, 0.5]);
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeInstanceOf(ProtocolError);
    const err = caught as ProtocolError;
    expect(err.reason).toBe("batch-body");
    expect(err.message).toContain("u8 bytes");
  });

  it("rejects byte arrays with out-of-range or non-integer entries", () => {
    expectBodyError([256, 0, 0, 0, 0, 0, 0, 0], "batch-body");
    expectBodyError([-1, 0, 0, 0, 0, 0, 0, 0], "batch-body");
    expectBodyError(["1", 0, 0, 0, 0, 0, 0, 0], "batch-body");
  });

  it("rejects non-buffer bodies", () => {
    expectBodyError(null, "buffer");
    expectBodyError(16, "buffer");
    expectBodyError({ length: 16 }, "buffer");
  });
});
