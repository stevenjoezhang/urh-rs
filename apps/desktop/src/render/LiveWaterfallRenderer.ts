/**
 * Canvas2D live waterfall renderer (P3). No React inside — the live view
 * component drains computed u8 columns into append().
 *
 * Each column is one preview batch: `freqBins` u8 values (DC first, the tile
 * payload layout) mapped through the shared {@link spectrogramColor} ramp
 * with the frequency axis flipped so DC lands at the bottom — identical to
 * the offline spectrogram tile view. Columns append at the right edge; a
 * full canvas scrolls left by one device pixel per appended column (the
 * classic self-blit waterfall), so the canvas itself is the history store
 * and no per-column memory accumulates outside it. Resizing clears the
 * history (the backing store is reallocated); the live stream refills it.
 */

import { spectrogramColor } from "./spectrogramImage";

const COLOR_BG = "#14161a";

export class LiveWaterfallRenderer {
  /** false when a 2D context is unavailable. */
  readonly ok: boolean;

  private readonly canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null = null;

  private pxW = 0;
  private pxH = 0;

  /** Offscreen 1×freqBins pixel column, rebuilt when the bin count changes. */
  private columnCanvas: HTMLCanvasElement | null = null;
  private columnCtx: CanvasRenderingContext2D | null = null;
  private columnImage: ImageData | null = null;
  private columnBins = 0;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext("2d", { alpha: false });
    if (ctx === null) {
      this.ok = false;
      return;
    }
    this.ok = true;
    this.ctx = ctx;
  }

  /** Set CSS-size backing store (clears the painted history). */
  resize(cssW: number, cssH: number, dpr: number): void {
    this.pxW = Math.max(0, Math.round(Math.max(0, cssW) * dpr));
    this.pxH = Math.max(0, Math.round(Math.max(0, cssH) * dpr));
    if (this.canvas.width !== this.pxW) this.canvas.width = this.pxW;
    if (this.canvas.height !== this.pxH) this.canvas.height = this.pxH;
    this.clear();
  }

  /** Paint the background (new capture session / resize). */
  clear(): void {
    const ctx = this.ctx;
    if (ctx === null) return;
    ctx.setTransform(1, 0, 0, 1, 0, 0); // device pixels; no CSS transform here
    ctx.fillStyle = COLOR_BG;
    ctx.fillRect(0, 0, this.pxW, this.pxH);
  }

  /**
   * Append one column (`freqBins` u8 dB-mapped values, DC first) at the right
   * edge, scrolling the existing history left by one device pixel.
   */
  append(column: Uint8Array): void {
    const ctx = this.ctx;
    if (ctx === null || this.pxW < 2 || this.pxH < 1) return;
    const freqBins = column.length;
    if (freqBins < 1) return;
    const img = this.ensureColumn(freqBins);
    if (img === null) return;

    // Paint the column into the offscreen pixel (DC at the bottom row, like
    // tilePixels flips the tile's frequency axis).
    const data = img.data;
    for (let f = 0; f < freqBins; f++) {
      const [r, g, b] = spectrogramColor(column[f]);
      const o = (freqBins - 1 - f) * 4;
      data[o] = r;
      data[o + 1] = g;
      data[o + 2] = b;
      data[o + 3] = 255;
    }
    this.columnCtx?.putImageData(img, 0, 0);

    // Identity transform: everything below is in device pixels.
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    // Scroll left by one device pixel. Drawing a canvas onto itself takes
    // the source as a snapshot, per the Canvas2D spec.
    ctx.drawImage(this.canvas, -1, 0);
    // The new column, scaled to the full height.
    ctx.drawImage(this.columnCanvas as HTMLCanvasElement, this.pxW - 1, 0, 1, this.pxH);
  }

  /** (Re)build the offscreen column pixel for `freqBins` bins. */
  private ensureColumn(freqBins: number): ImageData | null {
    if (this.columnBins === freqBins && this.columnImage !== null) {
      return this.columnImage;
    }
    const off = this.columnCanvas ?? document.createElement("canvas");
    off.width = 1;
    off.height = freqBins;
    const octx = off.getContext("2d");
    if (octx === null) {
      this.columnCanvas = null;
      this.columnCtx = null;
      this.columnImage = null;
      this.columnBins = 0;
      return null;
    }
    this.columnCanvas = off;
    this.columnCtx = octx;
    this.columnImage = octx.createImageData(1, freqBins);
    this.columnBins = freqBins;
    return this.columnImage;
  }
}
