/**
 * Pure view model for the live capture preview (P3). No React, no DOM — the
 * useCapture hook wires these to the `capture://batch` events and the live
 * renderers consume the results; everything here is unit-tested.
 *
 * Three pieces:
 *
 * - {@link BatchRing}: the rolling preview window (last ~10 batches) that
 *   feeds the live waveform;
 * - {@link seqGap}: client-side dropped-batch accounting from the batch
 *   `seq` counters (the backend's `dropped_preview_batches` is the
 *   authoritative twin of this number);
 * - {@link waveformColumns}: min/max per screen column over the ring, in the
 *   same interleaved `[iMin, iMax, qMin, qMax]` layout as an Iq envelope
 *   payload, so the live renderer consumes it exactly like the offline one;
 * - {@link WaterfallColumns}: one dB-mapped u8 column per batch, using the
 *   *identical* STFT/dB conventions as the backend's spectrogram tiles
 *   (crates/urh-app/src/spectrogram.rs): symmetric Hann window, FFT
 *   normalized by the window size, `db = 20·log10(|X| + 1e-12)`, only bins
 *   `0..size/2` rendered (DC first), byte =
 *   `round(255·clamp((db − db_min)/(db_ref − db_min), 0, 1))` with
 *   `db_min = db_ref − 100` — except that `db_ref` is the *running* max of
 *   the live session instead of the whole-signal max, so the two views look
 *   consistent while a capture runs.
 */

import { Fft } from "./fft";

/** One preview batch as kept by the ring: its seq plus the decoded I/Q. */
export interface LiveBatch {
  seq: number;
  /** Interleaved f32 I/Q (2 floats per complex sample). */
  iq: Float32Array;
}

/** Rolling preview window size (batches), contract: "last ~10 batches". */
export const PREVIEW_BATCH_RING_CAPACITY = 10;

/**
 * Fixed-capacity FIFO of the most recent batches. Pushing onto a full ring
 * evicts the oldest batch (the preview may lose old data — it is display
 * only, exactly like the backend's bounded preview queue).
 */
export class BatchRing {
  private readonly slots: (LiveBatch | null)[];
  /** Index of the oldest live slot. */
  private head = 0;
  private count = 0;

  constructor(readonly capacity: number = PREVIEW_BATCH_RING_CAPACITY) {
    if (!Number.isInteger(capacity) || capacity < 1) {
      throw new RangeError(`ring capacity must be a positive integer, got ${capacity}`);
    }
    this.slots = new Array<LiveBatch | null>(capacity).fill(null);
  }

  /** Number of batches currently held. */
  get length(): number {
    return this.count;
  }

  push(batch: LiveBatch): void {
    const tail = (this.head + this.count) % this.capacity;
    this.slots[tail] = batch;
    if (this.count < this.capacity) {
      this.count += 1;
    } else {
      this.head = (this.head + 1) % this.capacity; // overwrote the oldest
    }
  }

  /** Held batches, oldest → newest. */
  batches(): readonly LiveBatch[] {
    const out: LiveBatch[] = [];
    for (let i = 0; i < this.count; i++) {
      const b = this.slots[(this.head + i) % this.capacity];
      if (b !== null) out.push(b);
    }
    return out;
  }

  /** Total complex samples held. */
  totalSamples(): number {
    let n = 0;
    for (const b of this.batches()) n += b.iq.length >> 1;
    return n;
  }

  clear(): void {
    this.slots.fill(null);
    this.head = 0;
    this.count = 0;
  }
}

/**
 * Batches missing between the previous seq and this one. The first batch
 * establishes the baseline (no gap assumed before it); duplicates or
 * out-of-order seqs are not gaps.
 */
export function seqGap(prevSeq: number | null, seq: number): number {
  if (prevSeq === null || seq <= prevSeq) return 0;
  return seq - prevSeq - 1;
}

/**
 * Min/max per screen column over `batches` (oldest → newest), interleaved
 * `[iMin, iMax, qMin, qMax]` per column — the Iq envelope payload layout.
 * Column `c` covers the complex samples
 * `[floor(c·T/cols), floor((c+1)·T/cols))` of the concatenated window; the
 * single pass computes exact per-column min/max, so peaks are never lost to
 * decimation (same rule as the backend envelopes). Columns that end up with
 * no samples (more columns than samples) keep `±Infinity` and read as empty
 * via `iMin > iMax`.
 */
export function waveformColumns(batches: readonly LiveBatch[], columns: number): Float32Array {
  if (!Number.isInteger(columns) || columns < 1) return new Float32Array(0);
  let total = 0;
  for (const b of batches) total += b.iq.length;
  if (total === 0) return new Float32Array(0);
  const complexTotal = total >> 1;

  const out = new Float32Array(columns * 4);
  for (let c = 0; c < columns; c++) {
    const o = c * 4;
    out[o] = Infinity;
    out[o + 1] = -Infinity;
    out[o + 2] = Infinity;
    out[o + 3] = -Infinity;
  }

  let idx = 0; // complex sample index within the concatenated window
  for (const b of batches) {
    const iq = b.iq;
    for (let s = 0; s < iq.length; s += 2) {
      let col = Math.floor((idx * columns) / complexTotal);
      if (col >= columns) col = columns - 1;
      const o = col * 4;
      const i = iq[s];
      const q = iq[s + 1];
      if (i < out[o]) out[o] = i;
      if (i > out[o + 1]) out[o + 1] = i;
      if (q < out[o + 2]) out[o + 2] = q;
      if (q > out[o + 3]) out[o + 3] = q;
      idx += 1;
    }
  }
  return out;
}

// --- waterfall -------------------------------------------------------------------

/** FFT size for live waterfall columns (freq bins = size / 2, like tiles). */
export const WATERFALL_FFT_SIZE = 1024;

/** dB below the reference that maps to color 0 (contract: 100). */
export const WATERFALL_DB_RANGE = 100;

/**
 * Symmetric Hann window (`np.hanning`, `urh::spectrogram::window`):
 * `0.5 − 0.5·cos(2πi/(m−1))`; size 1 degenerates to [1].
 */
export function hannWindow(size: number): Float32Array {
  if (!Number.isInteger(size) || size < 1) {
    throw new RangeError(`window size must be a positive integer, got ${size}`);
  }
  const w = new Float32Array(size);
  if (size === 1) {
    w[0] = 1;
    return w;
  }
  for (let i = 0; i < size; i++) {
    w[i] = 0.5 - 0.5 * Math.cos((2 * Math.PI * i) / (size - 1));
  }
  return w;
}

/** `db = 20·log10(|X| + 1e-12)` — the tile view's formula. */
export function dbOf(magnitude: number): number {
  return 20 * Math.log10(magnitude + 1e-12);
}

/** dB of a zero magnitude — the floor a fully silent batch sits on. */
const SILENT_DB = dbOf(0);

/**
 * Map a dB value to a display byte (contract):
 * `round(255·clamp((db − db_min)/(db_ref − db_min), 0, 1))` with
 * `db_min = db_ref − 100`. NaN saturates to 0 when stored into a Uint8Array,
 * like the backend's `as u8` cast.
 */
export function dbToByte(db: number, dbRef: number): number {
  const dbMin = dbRef - WATERFALL_DB_RANGE;
  const ratio = Math.min(Math.max((db - dbMin) / (dbRef - dbMin), 0), 1);
  return Math.round(255 * ratio);
}

/**
 * Computes one waterfall column per pushed batch. Owns its FFT scratch and
 * the running dB reference; `push` allocates only the returned column.
 */
export class WaterfallColumns {
  readonly fftSize: number;
  /** Rendered frequency bins per column: `fftSize / 2`, DC first. */
  readonly freqBins: number;

  private dbRef = Number.NEGATIVE_INFINITY;
  private readonly fft: Fft;
  private readonly window: Float32Array;
  private readonly re: Float32Array;
  private readonly im: Float32Array;
  private readonly db: Float64Array;

  constructor(fftSize: number = WATERFALL_FFT_SIZE) {
    this.fft = new Fft(fftSize); // validates the power-of-two/≥2 contract
    this.fftSize = fftSize;
    this.freqBins = fftSize / 2;
    this.window = hannWindow(fftSize);
    this.re = new Float32Array(fftSize);
    this.im = new Float32Array(fftSize);
    this.db = new Float64Array(this.freqBins);
  }

  /** Running max dB of the session (the live analogue of the tiles' db_ref). */
  get referenceDb(): number {
    return this.dbRef;
  }

  /**
   * Window + FFT the batch's first `fftSize` complex samples (zero-padded
   * when the batch is shorter, mirroring the backend's frame padding) and
   * map to `freqBins` u8 values. Updates the running reference *before*
   * mapping, so the loudest bin of the loudest column renders as 255; a
   * fully silent batch renders black (see {@link SILENT_DB}).
   */
  push(iq: Float32Array): Uint8Array {
    const n = Math.min(this.fftSize, iq.length >> 1);
    this.re.fill(0);
    this.im.fill(0);
    for (let s = 0; s < n; s++) {
      const w = this.window[s];
      this.re[s] = iq[s * 2] * w;
      this.im[s] = iq[s * 2 + 1] * w;
    }
    this.fft.transform(this.re, this.im);

    let max = Number.NEGATIVE_INFINITY;
    for (let k = 0; k < this.freqBins; k++) {
      // Tile normalization: the spectrum is divided by the window size.
      const magnitude = Math.hypot(this.re[k], this.im[k]) / this.fftSize;
      const db = dbOf(magnitude);
      this.db[k] = db;
      if (db > max) max = db;
    }
    if (max > this.dbRef) this.dbRef = max;

    const out = new Uint8Array(this.freqBins);
    if (max > SILENT_DB) {
      for (let k = 0; k < this.freqBins; k++) out[k] = dbToByte(this.db[k], this.dbRef);
    }
    // else: every bin sits on the 1e-12 floor — a fully silent batch (or an
    // empty one). The formula would map it to 255 across (a silent column is
    // its own loudest column); render it black instead, like the gap before
    // a signal.
    return out;
  }

  /** Reset the running reference (new capture session). */
  reset(): void {
    this.dbRef = Number.NEGATIVE_INFINITY;
  }
}
