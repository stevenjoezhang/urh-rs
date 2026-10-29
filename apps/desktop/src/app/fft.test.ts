/**
 * FFT tests (P3 waterfall): known closed-form spectra (DC, complex
 * exponentials with bin wrap, real sine symmetry, impulse), a comparison
 * against a naive O(n²) DFT on pseudo-random input, Parseval's identity, the
 * smallest sizes hand-checked, and the size contract.
 */

import { describe, expect, it } from "vitest";

import { Fft, isPowerOfTwo } from "./fft";

/** Naive forward DFT: X[k] = Σ_j x[j]·e^(−2πi·j·k/N) — the reference. */
function naiveDft(re: readonly number[], im: readonly number[]): { re: number[]; im: number[] } {
  const n = re.length;
  const outRe = new Array<number>(n).fill(0);
  const outIm = new Array<number>(n).fill(0);
  for (let k = 0; k < n; k++) {
    for (let j = 0; j < n; j++) {
      const a = (-2 * Math.PI * j * k) / n;
      const c = Math.cos(a);
      const s = Math.sin(a);
      outRe[k] += re[j] * c - im[j] * s;
      outIm[k] += re[j] * s + im[j] * c;
    }
  }
  return { re: outRe, im: outIm };
}

function magnitude(re: ArrayLike<number>, im: ArrayLike<number>): number[] {
  const out: number[] = [];
  for (let k = 0; k < re.length; k++) out.push(Math.hypot(re[k], im[k]));
  return out;
}

/** Deterministic pseudo-random numbers in [−1, 1) (no seed machinery needed). */
function lcg(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (1664525 * s + 1013904223) >>> 0;
    return s / 0x100000000 - 0.5;
  };
}

function closeTo(a: number, b: number, eps = 1e-9): boolean {
  return Math.abs(a - b) <= eps * Math.max(1, Math.abs(a), Math.abs(b));
}

describe("isPowerOfTwo", () => {
  it("accepts 1, 2, 4 … 4096 and rejects the rest", () => {
    for (const n of [1, 2, 4, 8, 16, 1024, 4096]) expect(isPowerOfTwo(n)).toBe(true);
    for (const n of [0, -2, 3, 6, 1000, 1024.5, NaN]) expect(isPowerOfTwo(n)).toBe(false);
  });
});

/**
 * Tolerances: the FFT rounds to f32 at every butterfly pass, so errors grow
 * ~log2(N) f32 ulps; closed-form checks therefore use ~1e-4 absolute floors
 * (peak values are O(N)) and the naive-DFT comparison scales with N.
 */
const ZERO_EPS = 1e-4;

describe("Fft — closed-form spectra", () => {
  it("maps a DC signal to bin 0 only", () => {
    const n = 16;
    const re = new Float32Array(n).fill(1);
    const im = new Float32Array(n);
    new Fft(n).transform(re, im);
    expect(re[0]).toBeCloseTo(n, 6);
    expect(im[0]).toBeCloseTo(0, 6);
    for (let k = 1; k < n; k++) {
      expect(Math.hypot(re[k], im[k])).toBeLessThan(ZERO_EPS);
    }
  });

  it("maps e^(+2πi·j·k0/N) to a single peak at bin k0", () => {
    const n = 32;
    for (const k0 of [1, 5, 13]) {
      const re = new Float32Array(n);
      const im = new Float32Array(n);
      for (let j = 0; j < n; j++) {
        re[j] = Math.cos((2 * Math.PI * j * k0) / n);
        im[j] = Math.sin((2 * Math.PI * j * k0) / n);
      }
      new Fft(n).transform(re, im);
      const mag = magnitude(re, im);
      const peak = mag.indexOf(Math.max(...mag));
      expect(peak).toBe(k0);
      expect(mag[k0]).toBeCloseTo(n, 5);
      for (let k = 0; k < n; k++) {
        if (k !== k0) expect(mag[k]).toBeLessThan(ZERO_EPS);
      }
    }
  });

  it("wraps a negative-frequency exponential to bin N−k0", () => {
    const n = 32;
    const k0 = 5;
    const re = new Float32Array(n);
    const im = new Float32Array(n);
    for (let j = 0; j < n; j++) {
      re[j] = Math.cos((2 * Math.PI * j * k0) / n);
      im[j] = -Math.sin((2 * Math.PI * j * k0) / n);
    }
    new Fft(n).transform(re, im);
    const mag = magnitude(re, im);
    expect(mag[n - k0]).toBeCloseTo(n, 5);
    expect(Math.max(...mag.filter((_, k) => k !== n - k0))).toBeLessThan(ZERO_EPS);
  });

  it("gives a real sine symmetric peaks at k0 and N−k0", () => {
    const n = 64;
    const k0 = 7;
    const re = new Float32Array(n);
    const im = new Float32Array(n);
    for (let j = 0; j < n; j++) re[j] = Math.sin((2 * Math.PI * j * k0) / n);
    new Fft(n).transform(re, im);
    const mag = magnitude(re, im);
    expect(mag[k0]).toBeCloseTo(n / 2, 4);
    expect(mag[n - k0]).toBeCloseTo(n / 2, 4);
    expect(Math.abs(mag[k0] - mag[n - k0])).toBeLessThan(ZERO_EPS);
    for (let k = 0; k < n; k++) {
      if (k !== k0 && k !== n - k0) expect(mag[k]).toBeLessThan(ZERO_EPS);
    }
  });

  it("maps an impulse to a flat spectrum of magnitude 1", () => {
    const n = 16;
    const re = new Float32Array(n);
    const im = new Float32Array(n);
    re[3] = 1;
    new Fft(n).transform(re, im);
    for (let k = 0; k < n; k++) {
      expect(Math.hypot(re[k], im[k])).toBeCloseTo(1, 6);
    }
  });

  it("computes size 2 and 4 exactly", () => {
    const f2 = new Fft(2);
    const re2 = new Float32Array([3, -1]);
    const im2 = new Float32Array([0, 0]);
    f2.transform(re2, im2);
    expect(re2[0]).toBe(2);
    expect(re2[1]).toBe(4);

    const f4 = new Fft(4);
    const re4 = new Float32Array([1, 2, 3, 4]);
    const im4 = new Float32Array([0, 0, 0, 0]);
    f4.transform(re4, im4);
    expect(re4[0]).toBe(10); // sum
    expect(re4[2]).toBe(-2); // alternating sum
    expect(re4[1]).toBeCloseTo(-2, 10);
    expect(re4[3]).toBeCloseTo(-2, 10);
    expect(im4[1]).toBeCloseTo(2, 10);
    expect(im4[3]).toBeCloseTo(-2, 10);
  });
});

describe("Fft — against the naive DFT", () => {
  for (const n of [2, 4, 16, 32, 64, 1024]) {
    it(`matches the naive DFT on random input (N=${n})`, () => {
      const rand = lcg(0x5eed + n);
      const re = Float32Array.from({ length: n }, rand);
      const im = Float32Array.from({ length: n }, rand);
      const want = naiveDft(Array.from(re), Array.from(im));
      new Fft(n).transform(re, im);
      // f32 rounding accumulates ~log2(N) ulps through the butterfly passes.
      const eps = Math.max(1e-6, n * 1e-7);
      for (let k = 0; k < n; k++) {
        expect(closeTo(re[k], want.re[k], eps)).toBe(true);
        expect(closeTo(im[k], want.im[k], eps)).toBe(true);
      }
    });
  }

  it("satisfies Parseval: Σ|x|² = (1/N)·Σ|X|²", () => {
    const n = 128;
    const rand = lcg(42);
    const re = Float32Array.from({ length: n }, rand);
    const im = Float32Array.from({ length: n }, rand);
    let energy = 0;
    for (let j = 0; j < n; j++) energy += re[j] * re[j] + im[j] * im[j];
    new Fft(n).transform(re, im);
    let spectrum = 0;
    for (let k = 0; k < n; k++) spectrum += re[k] * re[k] + im[k] * im[k];
    expect(spectrum / n).toBeCloseTo(energy, 6);
  });
});

describe("Fft — contract", () => {
  it("rejects non-power-of-two and too-small sizes", () => {
    for (const bad of [0, 1, 3, 12, 1000, -4, 2.5]) {
      expect(() => new Fft(bad)).toThrowError(RangeError);
    }
  });

  it("rejects arrays of the wrong length", () => {
    const f = new Fft(8);
    expect(() => f.transform(new Float32Array(7), new Float32Array(8))).toThrowError(RangeError);
    expect(() => f.transform(new Float32Array(8), new Float32Array(9))).toThrowError(RangeError);
  });

  it("defaults the waterfall size through captureModel (1024, bins 512)", async () => {
    const { WATERFALL_FFT_SIZE, WaterfallColumns } = await import("./captureModel");
    expect(WATERFALL_FFT_SIZE).toBe(1024);
    expect(new WaterfallColumns().freqBins).toBe(512);
  });
});
