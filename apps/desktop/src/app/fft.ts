/**
 * Pure radix-2 complex FFT (P3 live waterfall).
 *
 * Iterative Cooley–Tukey with a precomputed bit-reversal table and twiddle
 * factors, in-place on separate re/im arrays — the caller owns the buffers, so
 * a per-batch waterfall column costs one `transform` and no allocations. The
 * forward-DFT convention matches `urh::spectrogram`'s `fft`:
 *
 *     X[k] = Σ_j x[j] · e^(−2πi·j·k / N)
 *
 * Sizes must be powers of two ≥ 2 (the live waterfall uses 1024; the class is
 * general so tests can exercise small sizes against a naive DFT).
 */

/** Exact power-of-two test (1 counts as one; callers wanting ≥ 2 check that). */
export function isPowerOfTwo(n: number): boolean {
  return Number.isInteger(n) && n >= 1 && (n & (n - 1)) === 0;
}

export class Fft {
  readonly size: number;

  private readonly levels: number;
  private readonly reversal: Uint32Array;
  private readonly cosTable: Float64Array;
  private readonly sinTable: Float64Array;

  constructor(size: number) {
    if (!isPowerOfTwo(size) || size < 2) {
      throw new RangeError(`FFT size must be a power of two >= 2, got ${size}`);
    }
    this.size = size;
    this.levels = Math.round(Math.log2(size));

    this.reversal = new Uint32Array(size);
    for (let i = 0; i < size; i++) {
      let x = i;
      let r = 0;
      for (let b = 0; b < this.levels; b++) {
        r = (r << 1) | (x & 1);
        x >>>= 1;
      }
      this.reversal[i] = r;
    }

    const half = size / 2;
    this.cosTable = new Float64Array(half);
    this.sinTable = new Float64Array(half);
    for (let i = 0; i < half; i++) {
      this.cosTable[i] = Math.cos((2 * Math.PI * i) / size);
      this.sinTable[i] = Math.sin((2 * Math.PI * i) / size);
    }
  }

  /**
   * In-place forward DFT of the complex vector `(re, im)`. Both arrays must
   * have exactly {@link size} elements and are overwritten with the result.
   */
  transform(re: Float32Array, im: Float32Array): void {
    const n = this.size;
    if (re.length !== n || im.length !== n) {
      throw new RangeError(`transform expects arrays of length ${n}, got ${re.length}/${im.length}`);
    }

    // Bit-reversal permutation.
    for (let i = 0; i < n; i++) {
      const j = this.reversal[i];
      if (j > i) {
        let t = re[i];
        re[i] = re[j];
        re[j] = t;
        t = im[i];
        im[i] = im[j];
        im[j] = t;
      }
    }

    // Butterfly passes, smallest sub-transform first.
    for (let span = 2; span <= n; span *= 2) {
      const half = span >> 1;
      const step = n / span;
      for (let base = 0; base < n; base += span) {
        for (let j = base, k = 0; j < base + half; j++, k += step) {
          const l = j + half;
          const c = this.cosTable[k];
          const s = this.sinTable[k];
          // (re[l] + i·im[l]) · (c − i·s)
          const tre = re[l] * c + im[l] * s;
          const tim = -re[l] * s + im[l] * c;
          re[l] = re[j] - tre;
          im[l] = im[j] - tim;
          re[j] += tre;
          im[j] += tim;
        }
      }
    }
  }
}
