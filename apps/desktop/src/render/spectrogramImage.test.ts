/**
 * Spectrogram tile → RGBA mapping tests: colormap behavior, the vertical
 * flip that puts DC at the bottom row, and the column (time-bin) layout.
 */

import { describe, expect, it } from "vitest";

import { ProtocolError, decodeSpectrogramTile } from "../bridge/protocol";
import { spectrogramColor, tilePixels } from "./spectrogramImage";

const HEADER = 24;

/** Decode-side helper: build a tile frame with distinguishable payload bytes. */
function tile(nTimeBins: number, windowSize: 256 | 1024 | 4096): ReturnType<typeof decodeSpectrogramTile> {
  const freqBins = windowSize / 2;
  const buf = new ArrayBuffer(HEADER + nTimeBins * freqBins);
  const dv = new DataView(buf);
  dv.setUint32(0, 0x31524855, true);
  dv.setUint16(4, 1, true);
  dv.setUint16(6, 2, true); // u8
  dv.setUint32(8, 1, true); // request id
  dv.setUint32(12, nTimeBins, true);
  dv.setUint32(16, 3, true); // spectrogram mode
  dv.setUint32(20, 0, true);
  const payload = new Uint8Array(buf, HEADER);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 31) & 0xff;
  return decodeSpectrogramTile(buf, windowSize);
}

describe("spectrogramColor", () => {
  it("hits the exact control points at the ends", () => {
    expect([...spectrogramColor(0)]).toEqual([0, 0, 4]);
    expect([...spectrogramColor(255)]).toEqual([252, 255, 164]);
  });

  it("interpolates monotonically between stops", () => {
    // 0.25 stop = (87, 16, 110) at v = 63.75, so v = 64 is on it.
    const c = spectrogramColor(64);
    expect(c[0]).toBe(87);
    expect(c[1]).toBe(16);
    expect(c[2]).toBe(110);
    // v = 128 is t = 128/255 = 0.50196, just past the 0.5 stop (188, 55, 84):
    // f = 0.00784 into [0.5, 0.75] -> (188.48, 55.68, 83.41) -> (188, 56, 83).
    const mid = spectrogramColor(128);
    expect([...mid]).toEqual([188, 56, 83]);
    const below = spectrogramColor(32);
    const above = spectrogramColor(96);
    expect(below[2]).toBeGreaterThan(4); // purple rising
    expect(above[0]).toBeGreaterThan(below[0]);
  });

  it("clamps out-of-range values", () => {
    expect([...spectrogramColor(-5)]).toEqual([0, 0, 4]);
    expect([...spectrogramColor(300)]).toEqual([252, 255, 164]);
    expect([...spectrogramColor(Number.NaN)]).toEqual([0, 0, 4]);
  });
});

describe("tilePixels", () => {
  it("produces an RGBA buffer of nTimeBins x freqBins", () => {
    const t = tile(5, 256);
    const px = tilePixels(t);
    expect(px.width).toBe(5);
    expect(px.height).toBe(128);
    expect(px.rgba.length).toBe(5 * 128 * 4);
  });

  it("flips the frequency axis: DC (f = 0) is the BOTTOM row", () => {
    // Tile with a single time bin; set f=0 (DC) to 255 and f=max to 0.
    const windowSize = 256;
    const freqBins = windowSize / 2;
    const buf = new ArrayBuffer(HEADER + freqBins);
    const dv = new DataView(buf);
    dv.setUint32(0, 0x31524855, true);
    dv.setUint16(4, 1, true);
    dv.setUint16(6, 2, true);
    dv.setUint32(8, 1, true);
    dv.setUint32(12, 1, true); // one time bin
    dv.setUint32(16, 3, true);
    dv.setUint32(20, 0, true);
    const payload = new Uint8Array(buf, HEADER);
    payload[0] = 255; // DC loudest
    payload[freqBins - 1] = 0; // Nyquist quietest
    const t = decodeSpectrogramTile(buf, windowSize);
    const px = tilePixels(t);
    const bottomRow = (px.height - 1) * px.width; // y = height-1
    const topRow = 0;
    // bottom-left pixel = DC = the hot color
    expect([...px.rgba.slice(bottomRow * 4, bottomRow * 4 + 3)]).toEqual([...spectrogramColor(255)]);
    // top-left pixel = Nyquist = the cold color
    expect([...px.rgba.slice(topRow * 4, topRow * 4 + 3)]).toEqual([...spectrogramColor(0)]);
    // opaque everywhere
    expect(px.rgba[bottomRow * 4 + 3]).toBe(255);
  });

  it("maps time bin t to column t (left = earliest)", () => {
    const t = tile(3, 256);
    const px = tilePixels(t);
    for (let col = 0; col < 3; col++) {
      const f = 5;
      const y = px.height - 1 - f; // unflip
      const idx = (y * px.width + col) * 4;
      const [r, g, b] = spectrogramColor(t.data[col * t.freqBins + f]);
      expect([...px.rgba.slice(idx, idx + 3)]).toEqual([r, g, b]);
    }
  });

  it("handles the empty (zero time-bin) tile", () => {
    const t = tile(0, 1024);
    const px = tilePixels(t);
    expect(px.width).toBe(0);
    expect(px.height).toBe(512);
    expect(px.rgba.length).toBe(0);
  });

  it("only ever receives decoded tiles (sanity: raw frames are rejected)", () => {
    expect(() => decodeSpectrogramTile(new ArrayBuffer(10), 256)).toThrowError(ProtocolError);
  });
});
