/**
 * Live-preview view-model tests (P3): the rolling batch ring, seq-gap
 * accounting, exact per-column min/max (the envelope layout the live waveform
 * renderer consumes), the Hann window / dB mapping shared with the backend's
 * spectrogram tiles, and WaterfallColumns against an independent naive-DFT
 * reference implementation of the whole pipeline.
 */

import { describe, expect, it } from "vitest";

import {
  type LiveBatch,
  BatchRing,
  PREVIEW_BATCH_RING_CAPACITY,
  WATERFALL_DB_RANGE,
  WATERFALL_FFT_SIZE,
  WaterfallColumns,
  dbOf,
  dbToByte,
  hannWindow,
  seqGap,
  waveformColumns,
} from "./captureModel";

/** A batch of constant complex values (2·count floats). */
function constBatch(seq: number, i: number, q: number, count: number): LiveBatch {
  const iq = new Float32Array(count * 2);
  for (let s = 0; s < count; s++) {
    iq[s * 2] = i;
    iq[s * 2 + 1] = q;
  }
  return { seq, iq };
}

describe("BatchRing", () => {
  it("defaults to the contract's ~10-batch capacity", () => {
    expect(PREVIEW_BATCH_RING_CAPACITY).toBe(10);
    expect(new BatchRing().capacity).toBe(10);
  });

  it("keeps insertion order, oldest to newest", () => {
    const ring = new BatchRing(3);
    for (let seq = 0; seq < 3; seq++) ring.push(constBatch(seq, seq, -seq, 1));
    expect(ring.length).toBe(3);
    expect(ring.batches().map((b) => b.seq)).toEqual([0, 1, 2]);
  });

  it("evicts the oldest batches once full", () => {
    const ring = new BatchRing(3);
    for (let seq = 0; seq < 6; seq++) ring.push(constBatch(seq, 0, 0, 2));
    expect(ring.length).toBe(3);
    expect(ring.batches().map((b) => b.seq)).toEqual([3, 4, 5]);
  });

  it("counts complex samples across the window", () => {
    const ring = new BatchRing(10);
    ring.push(constBatch(0, 0, 0, 4));
    ring.push(constBatch(1, 0, 0, 6));
    expect(ring.totalSamples()).toBe(10);
  });

  it("clears", () => {
    const ring = new BatchRing(2);
    ring.push(constBatch(0, 0, 0, 1));
    ring.clear();
    expect(ring.length).toBe(0);
    expect(ring.batches()).toEqual([]);
    ring.push(constBatch(5, 0, 0, 1));
    expect(ring.batches().map((b) => b.seq)).toEqual([5]); // head was reset
  });

  it("rejects capacities below 1 (and non-integers)", () => {
    expect(() => new BatchRing(0)).toThrowError(RangeError);
    expect(() => new BatchRing(-1)).toThrowError(RangeError);
    expect(() => new BatchRing(1.5)).toThrowError(RangeError);
    expect(() => new BatchRing(1)).not.toThrow();
  });
});

describe("seqGap", () => {
  it("treats the first batch as the baseline", () => {
    expect(seqGap(null, 0)).toBe(0);
    expect(seqGap(null, 41)).toBe(0);
  });

  it("counts missing seqs between consecutive batches", () => {
    expect(seqGap(0, 1)).toBe(0);
    expect(seqGap(0, 5)).toBe(4);
    expect(seqGap(10, 13)).toBe(2);
  });

  it("does not count duplicates or out-of-order seqs", () => {
    expect(seqGap(7, 7)).toBe(0);
    expect(seqGap(7, 3)).toBe(0);
  });
});

describe("waveformColumns", () => {
  it("returns the interleaved [iMin,iMax,qMin,qMax] layout", () => {
    const batches = [constBatch(0, -0.25, 0.5, 3), constBatch(1, 0.75, -0.125, 3)];
    const cols = waveformColumns(batches, 1);
    expect(cols.length).toBe(4);
    expect(cols[0]).toBeCloseTo(-0.25, 7); // iMin
    expect(cols[1]).toBeCloseTo(0.75, 7); // iMax
    expect(cols[2]).toBeCloseTo(-0.125, 7); // qMin
    expect(cols[3]).toBeCloseTo(0.5, 7); // qMax
  });

  it("computes exact per-column min/max over the window", () => {
    // 10 complex samples with known extremes per half; 2 columns of 5.
    const iq = new Float32Array([0, 0, -0.5, 0.1, 0.2, -0.3, 0.9, -0.05, 0, 0, 0.1, 0.4, -0.2, 0.25, 0.05, -0.6, 0.3, 0.15, -0.1, 0.2]);
    const cols = waveformColumns([{ seq: 0, iq }], 2);
    expect(cols.length).toBe(8);
    // column 0 covers samples 0..4: I in [0,-0.5,0.2,0.9,0], Q in [0,0.1,-0.3,-0.05,0]
    expect(cols[0]).toBeCloseTo(-0.5, 7);
    expect(cols[1]).toBeCloseTo(0.9, 7);
    expect(cols[2]).toBeCloseTo(-0.3, 7);
    expect(cols[3]).toBeCloseTo(0.1, 7);
    // column 1 covers samples 5..9: I in [0.1,-0.2,0.05,0.3,-0.1], Q in [0.4,0.25,-0.6,0.15,0.2]
    expect(cols[4]).toBeCloseTo(-0.2, 7);
    expect(cols[5]).toBeCloseTo(0.3, 7);
    expect(cols[6]).toBeCloseTo(-0.6, 7);
    expect(cols[7]).toBeCloseTo(0.4, 7);
  });

  it("never loses a spike to decimation (peak lands in its column)", () => {
    const iq = new Float32Array(2000); // 1000 complex samples, all zero…
    iq[777 * 2] = 0.9; // …except I of sample 777
    iq[777 * 2 + 1] = -0.8;
    const cols = waveformColumns([{ seq: 0, iq }], 100);
    // sample 777 lands in column floor(777·100/1000) = 77
    const o = 77 * 4;
    expect(cols[o]).toBeCloseTo(0, 7); // other values are 0
    expect(cols[o + 1]).toBeCloseTo(0.9, 7);
    expect(cols[o + 2]).toBeCloseTo(-0.8, 7);
    expect(cols[o + 3]).toBeCloseTo(0, 7);
    // and no other column saw it
    for (let c = 0; c < 100; c++) {
      if (c === 77) continue;
      expect(cols[c * 4 + 1]).toBeCloseTo(0, 7);
    }
  });

  it("keeps ±Infinity (empty) columns when there are more columns than samples", () => {
    const cols = waveformColumns([constBatch(0, 0.5, -0.5, 2)], 5);
    expect(cols.length).toBe(20);
    // 2 samples over 5 columns: sample idx → column floor(idx·5/2), so
    // columns 0 and 2 hold them; columns 1, 3, 4 stay empty.
    expect(cols[0 * 4 + 1]).toBeCloseTo(0.5, 7);
    expect(cols[2 * 4 + 3]).toBeCloseTo(-0.5, 7);
    const empty = 4;
    expect(cols[empty * 4]).toBe(Infinity);
    expect(cols[empty * 4 + 1]).toBe(-Infinity);
    // the renderer's emptiness test: iMin > iMax
    expect(cols[empty * 4] > cols[empty * 4 + 1]).toBe(true);
  });

  it("returns empty output for no batches or invalid column counts", () => {
    expect(waveformColumns([], 8).length).toBe(0);
    expect(waveformColumns([constBatch(0, 1, 1, 1)], 0).length).toBe(0);
    expect(waveformColumns([constBatch(0, 1, 1, 1)], 1.5).length).toBe(0);
  });

  it("concatenates batches oldest→newest into one window", () => {
    const cols = waveformColumns([constBatch(0, -1, 2, 2), constBatch(1, 3, -4, 2)], 1);
    expect(cols[0]).toBeCloseTo(-1, 7);
    expect(cols[1]).toBeCloseTo(3, 7);
    expect(cols[2]).toBeCloseTo(-4, 7);
    expect(cols[3]).toBeCloseTo(2, 7);
  });
});

describe("hannWindow", () => {
  it("matches np.hanning: endpoints 0, sum (m−1)/2", () => {
    const w = hannWindow(8);
    expect(w.length).toBe(8);
    expect(w[0]).toBeCloseTo(0, 7);
    expect(w[7]).toBeCloseTo(0, 7);
    let sum = 0;
    for (const v of w) sum += v;
    // The symmetric Hann sums to (m−1)/2 = 3.5 (the periodic one to m/2) —
    // the extra cos(2π)=1 endpoint term costs the half sample.
    expect(sum).toBeCloseTo(3.5, 6);
    // …and the formula itself (the peak sits between samples for even m).
    expect(w[4]).toBeCloseTo(0.5 - 0.5 * Math.cos((2 * Math.PI * 4) / 7), 7);
  });

  it("is symmetric", () => {
    const w = hannWindow(16);
    for (let i = 0; i < 16; i++) expect(w[i]).toBeCloseTo(w[15 - i], 7);
  });

  it("degenerates size 1 to [1]", () => {
    expect(Array.from(hannWindow(1))).toEqual([1]);
  });

  it("rejects invalid sizes", () => {
    expect(() => hannWindow(0)).toThrowError(RangeError);
    expect(() => hannWindow(-2)).toThrowError(RangeError);
    expect(() => hannWindow(2.5)).toThrowError(RangeError);
  });
});

describe("dB mapping (tile conventions)", () => {
  it("dbOf floors at 20·log10(1e-12) = −240 and is ~0 at 1", () => {
    expect(dbOf(0)).toBeCloseTo(-240, 6);
    expect(dbOf(1)).toBeCloseTo(20 * Math.log10(1 + 1e-12), 9);
    expect(dbOf(0.5)).toBeCloseTo(-6.0206, 4);
  });

  it("dbToByte clamps at the reference and 100 dB below it", () => {
    expect(WATERFALL_DB_RANGE).toBe(100);
    expect(dbToByte(0, 0)).toBe(255); // at the reference
    expect(dbToByte(6, 0)).toBe(255); // above it
    expect(dbToByte(-100, 0)).toBe(0); // at db_min
    expect(dbToByte(-150, 0)).toBe(0); // below it
    expect(dbToByte(-50, 0)).toBe(128); // halfway
  });

  it("saturates NaN to 0 through the Uint8Array store (backend `as u8`)", () => {
    const arr = new Uint8Array(1);
    arr[0] = dbToByte(NaN, 0);
    expect(arr[0]).toBe(0);
  });
});

/** Independent reference for one waterfall column (naive DFT, tile dB rules). */
function referenceColumn(iq: Float32Array, fftSize: number): Uint8Array {
  const freqBins = fftSize / 2;
  const w = hannWindow(fftSize);
  const re = new Array<number>(fftSize).fill(0);
  const im = new Array<number>(fftSize).fill(0);
  const n = Math.min(fftSize, iq.length >> 1);
  for (let s = 0; s < n; s++) {
    re[s] = iq[s * 2] * w[s];
    im[s] = iq[s * 2 + 1] * w[s];
  }
  const out = new Uint8Array(freqBins);
  let dbRef = Number.NEGATIVE_INFINITY;
  const db: number[] = [];
  for (let k = 0; k < freqBins; k++) {
    let sr = 0;
    let si = 0;
    for (let j = 0; j < fftSize; j++) {
      const a = (-2 * Math.PI * j * k) / fftSize;
      sr += re[j] * Math.cos(a) - im[j] * Math.sin(a);
      si += re[j] * Math.sin(a) + im[j] * Math.cos(a);
    }
    const d = 20 * Math.log10(Math.hypot(sr, si) / fftSize + 1e-12);
    db.push(d);
    if (d > dbRef) dbRef = d;
  }
  for (let k = 0; k < freqBins; k++) out[k] = dbToByte(db[k], dbRef);
  return out;
}

/** A complex exponential at bin k0 with amplitude a (plus tiny noise-free DC). */
function toneBatch(k0: number, fftSize: number, a = 0.8): Float32Array {
  const iq = new Float32Array(fftSize * 2);
  for (let j = 0; j < fftSize; j++) {
    iq[j * 2] = a * Math.cos((2 * Math.PI * j * k0) / fftSize);
    iq[j * 2 + 1] = a * Math.sin((2 * Math.PI * j * k0) / fftSize);
  }
  return iq;
}

describe("WaterfallColumns", () => {
  it("renders fftSize/2 bins (DC first) with the contract defaults", () => {
    expect(WATERFALL_FFT_SIZE).toBe(1024);
    const wf = new WaterfallColumns();
    expect(wf.fftSize).toBe(1024);
    expect(wf.freqBins).toBe(512);
  });

  it("peaks a DC signal at bin 0 and decays with Hann leakage", () => {
    const wf = new WaterfallColumns(64);
    const col = wf.push(constBatch(0, 1, 0, 64).iq);
    expect(col.length).toBe(32);
    expect(col[0]).toBe(255);
    // Hann sidelobes: bin 1 is ~6 dB down, then ~1/k² amplitude decay —
    // the same leakage the offline spectrogram tiles show for DC.
    expect(col[1]).toBeLessThan(255);
    expect(col[2]).toBeLessThan(col[1]);
    expect(col[6]).toBeLessThan(col[2]);
    for (let k = 1; k < 32; k++) expect(col[k]).toBeLessThanOrEqual(col[0]);
  });

  it("peaks a tone at its bin (within the rendered 0..Nyquist range)", () => {
    const wf = new WaterfallColumns(64);
    for (const k0 of [1, 7, 20, 31]) {
      const col = wf.push(toneBatch(k0, 64));
      let peak = 0;
      for (let k = 1; k < col.length; k++) {
        if (col[k] > col[peak]) peak = k;
      }
      expect(peak).toBe(k0);
      expect(col[k0]).toBe(255);
    }
  });

  it("matches the naive-DFT reference on pseudo-random input", () => {
    const fftSize = 32;
    const wf = new WaterfallColumns(fftSize);
    const iq = new Float32Array(fftSize * 2);
    let s = 7;
    for (let j = 0; j < iq.length; j++) {
      s = (1664525 * s + 1013904223) >>> 0;
      iq[j] = s / 0x80000000 - 1; // [−1, 1)
    }
    expect(Array.from(wf.push(iq))).toEqual(Array.from(referenceColumn(iq, fftSize)));
  });

  it("updates the running reference before mapping (loudest bin renders 255)", () => {
    const wf = new WaterfallColumns(64);
    // First column: its own peak is the reference, so it renders 255 too.
    const quiet = wf.push(toneBatch(3, 64, 0.01));
    const refQuiet = wf.referenceDb;
    expect(quiet[3]).toBe(255);
    // A 39 dB louder tone raises the reference…
    const loud = wf.push(toneBatch(3, 64, 0.9));
    expect(wf.referenceDb).toBeGreaterThan(refQuiet);
    expect(loud[3]).toBe(255);
    // …and the quiet level now sits ~39 dB below it: visible, no longer 255.
    const quietAgain = wf.push(toneBatch(3, 64, 0.01));
    expect(quietAgain[3]).toBeGreaterThan(0);
    expect(quietAgain[3]).toBeLessThan(255);
  });

  it("zero-pads batches shorter than the FFT size", () => {
    const fftSize = 32;
    const wf = new WaterfallColumns(fftSize);
    const short = new Float32Array(fftSize); // half-length
    for (let j = 0; j < short.length / 2; j++) {
      short[j * 2] = Math.cos((2 * Math.PI * 4 * j) / (fftSize / 2));
      short[j * 2 + 1] = Math.sin((2 * Math.PI * 4 * j) / (fftSize / 2));
    }
    expect(Array.from(wf.push(short))).toEqual(Array.from(referenceColumn(short, fftSize)));
    expect(wf.referenceDb).toBeGreaterThan(-240);
  });

  it("handles an empty batch (all-zero column)", () => {
    const wf = new WaterfallColumns(16);
    const col = wf.push(new Float32Array(0));
    expect(col.length).toBe(8);
    expect(col.every((v) => v === 0)).toBe(true);
  });

  it("resets the running reference between sessions", () => {
    const wf = new WaterfallColumns(64);
    wf.push(toneBatch(5, 64, 0.9));
    expect(wf.referenceDb).toBeGreaterThan(-240);
    wf.reset();
    expect(wf.referenceDb).toBe(Number.NEGATIVE_INFINITY);
  });
});
