/**
 * Raw Canvas2D spectrogram renderer. No React inside — React pushes state in
 * via setters and calls draw().
 *
 * The decoded tile is converted once (per tile) into an offscreen canvas of
 * exactly `nTimeBins x freqBins` pixels by the pure {@link tilePixels} mapping
 * (DC at the bottom row); draw() then only re-scales that offscreen canvas
 * into the viewport rectangle the tile's sample range maps to, using the same
 * shared screen<->sample transform as the waveform. Panning and zooming are
 * therefore pure view changes: the x positions come from `viewSampleToX`
 * against the *tile's* requested range, so a stale tile stays glued to its
 * samples while the newer one is in flight — the same discipline as the
 * waveform's envelope.
 *
 * Selection / message-highlight / cursor overlays use the identical colors
 * and transform as the WebGL waveform so both view modes read as one view.
 */

import { type DecodedSpectrogramTile, hopForWindow } from "../bridge/protocol";
import { type Interval, type SelectionRange, type ViewRange, viewSampleToX } from "./WaveformRenderer";
import { tilePixels } from "./spectrogramImage";

const COLOR_BG = "#14161a";
const COLOR_SEL_FILL = "rgba(79, 195, 247, 0.15)";
const COLOR_SEL_EDGE = "rgba(79, 195, 247, 0.8)";
const COLOR_SEL_HANDLE = "rgba(79, 195, 247, 0.3)";
const COLOR_MSG_FILL = "rgba(255, 176, 0, 0.10)";
const COLOR_MSG_EDGE = "rgba(255, 176, 0, 0.65)";
const COLOR_CURSOR = "rgba(255, 255, 255, 0.35)";

/** Selection edge handles: half-width and cap height, in CSS pixels. */
const SEL_HANDLE_HALF_PX = 4;
const SEL_HANDLE_CAP_PX = 3;
/** Message-highlight end-bar half-width, in CSS pixels. */
const MSG_EDGE_HALF_PX = 2;

export class SpectrogramRenderer {
  /** false when a 2D context is unavailable; transforms still work. */
  readonly ok: boolean;

  private readonly canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null = null;

  private cssW = 0;
  private cssH = 0;

  private tile: DecodedSpectrogramTile | null = null;
  /** View (sample range) the tile was requested for. */
  private tileView: ViewRange | null = null;
  private offscreen: HTMLCanvasElement | null = null;

  private view: ViewRange = { start: 0, end: 1 };
  private selection: SelectionRange | null = null;
  private messageHighlight: Interval | null = null;
  private cursor: number | null = null;

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

  // --- state in ---------------------------------------------------------------

  /** Set CSS-size backing store. `dpr` = devicePixelRatio. Triggers a draw. */
  resize(cssW: number, cssH: number, dpr: number): void {
    this.cssW = Math.max(0, cssW);
    this.cssH = Math.max(0, cssH);
    const pxW = Math.max(1, Math.round(this.cssW * dpr));
    const pxH = Math.max(1, Math.round(this.cssH * dpr));
    if (this.canvas.width !== pxW) this.canvas.width = pxW;
    if (this.canvas.height !== pxH) this.canvas.height = pxH;
    this.draw();
  }

  /**
   * Upload the decoded tile. `sourceView` is the view the tile was requested
   * for; the tile is drawn at the x positions that range maps to, so stale
   * tiles never slide around with the live view.
   */
  setTile(tile: DecodedSpectrogramTile | null, sourceView: ViewRange | null): void {
    this.tile = tile;
    this.tileView = tile === null ? null : sourceView;
    this.uploadTile();
    this.draw();
  }

  setView(view: ViewRange): void {
    this.view = view;
    this.draw();
  }

  setSelection(sel: SelectionRange | null): void {
    this.selection = sel;
    this.draw();
  }

  /** Highlight a protocol message's `[start, end)` extent (table row click). */
  setMessageHighlight(m: Interval | null): void {
    this.messageHighlight = m;
    this.draw();
  }

  setCursor(sample: number | null): void {
    this.cursor = sample;
    this.draw();
  }

  // --- shared transform (rendering + hit-testing) ------------------------------

  get cssWidth(): number {
    return this.cssW;
  }

  /** Sample -> CSS pixel (left edge), via the shared pure transform. */
  sampleToX(sample: number): number {
    return viewSampleToX(this.view, this.cssW, sample);
  }

  // --- drawing -----------------------------------------------------------------

  draw(): void {
    const ctx = this.ctx;
    if (ctx === null) return;
    if (this.cssW <= 0 || this.cssH <= 0) return;

    const dpr = this.canvas.width / Math.max(1, this.cssW);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    ctx.fillStyle = COLOR_BG;
    ctx.fillRect(0, 0, this.cssW, this.cssH);

    this.drawTile(ctx);
    if (this.messageHighlight !== null) this.drawMessageHighlight(ctx);
    if (this.selection !== null) this.drawSelection(ctx);
    if (this.cursor !== null) this.drawCursor(ctx);
  }

  private drawTile(ctx: CanvasRenderingContext2D): void {
    const tile = this.tile;
    const tileView = this.tileView;
    const off = this.offscreen;
    if (tile === null || tileView === null || off === null) return;
    if (tile.nTimeBins === 0 || tile.freqBins === 0) return;

    // Time bin t covers [tileView.start + t*hop, ... + (t+1)*hop); uniform in
    // sample space, so the whole bitmap maps to one contiguous x range.
    const hop = hopForWindow(tile.windowSize);
    const x0 = viewSampleToX(this.view, this.cssW, tileView.start);
    const x1 = viewSampleToX(this.view, this.cssW, tileView.start + tile.nTimeBins * hop);
    if (!(x1 > x0)) return;

    // Minifying (many time bins per pixel) -> bilinear; magnifying -> crisp
    // blocks, which reads better for the discrete STFT grid.
    ctx.imageSmoothingEnabled = x1 - x0 <= tile.nTimeBins;
    ctx.drawImage(off, x0, 0, x1 - x0, this.cssH);
  }

  private drawSelection(ctx: CanvasRenderingContext2D): void {
    const sel = this.selection;
    if (sel === null) return;
    const x1 = this.sampleToX(Math.min(sel.selStart, sel.selEnd));
    const x2 = this.sampleToX(Math.max(sel.selStart, sel.selEnd));
    ctx.fillStyle = COLOR_SEL_FILL;
    ctx.fillRect(x1, 0, x2 - x1, this.cssH);
    ctx.strokeStyle = COLOR_SEL_EDGE;
    ctx.lineWidth = 1;
    for (const x of [x1, x2]) {
      ctx.beginPath();
      ctx.moveTo(x + 0.5, 0);
      ctx.lineTo(x + 0.5, this.cssH);
      ctx.stroke();
    }
    for (const x of [x1, x2]) {
      ctx.fillStyle = COLOR_SEL_HANDLE;
      ctx.fillRect(x - SEL_HANDLE_HALF_PX, 0, SEL_HANDLE_HALF_PX * 2, this.cssH);
      ctx.fillStyle = COLOR_SEL_EDGE;
      ctx.fillRect(x - SEL_HANDLE_HALF_PX, 0, SEL_HANDLE_HALF_PX * 2, SEL_HANDLE_CAP_PX);
      ctx.fillRect(x - SEL_HANDLE_HALF_PX, this.cssH - SEL_HANDLE_CAP_PX, SEL_HANDLE_HALF_PX * 2, SEL_HANDLE_CAP_PX);
    }
  }

  private drawMessageHighlight(ctx: CanvasRenderingContext2D): void {
    const m = this.messageHighlight;
    if (m === null || !(m.start < m.end)) return;
    const x1 = this.sampleToX(m.start);
    const x2 = this.sampleToX(m.end);
    ctx.fillStyle = COLOR_MSG_FILL;
    ctx.fillRect(x1, 0, x2 - x1, this.cssH);
    ctx.fillStyle = COLOR_MSG_EDGE;
    for (const x of [x1, x2]) {
      ctx.fillRect(x - MSG_EDGE_HALF_PX, 0, MSG_EDGE_HALF_PX * 2, this.cssH);
    }
  }

  private drawCursor(ctx: CanvasRenderingContext2D): void {
    if (this.cursor === null) return;
    const x = Math.round(this.sampleToX(this.cursor)) + 0.5;
    ctx.strokeStyle = COLOR_CURSOR;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, this.cssH);
    ctx.stroke();
  }

  // --- offscreen bitmap ----------------------------------------------------------

  /** (Re)build the offscreen bitmap when a new tile arrives. */
  private uploadTile(): void {
    const tile = this.tile;
    if (tile === null) {
      this.offscreen = null;
      return;
    }
    const { width, height, rgba } = tilePixels(tile);
    if (width === 0 || height === 0) {
      this.offscreen = null; // empty tile (range shorter than one hop)
      return;
    }
    const off = this.offscreen ?? document.createElement("canvas");
    if (off.width !== width || off.height !== height) {
      off.width = width;
      off.height = height;
    }
    const octx = off.getContext("2d");
    if (octx === null) {
      this.offscreen = null;
      return;
    }
    octx.putImageData(new ImageData(rgba, width, height), 0, 0);
    this.offscreen = off;
  }
}
