/**
 * Canvas2D live waveform renderer (P3). No React inside — the live view
 * component pushes the pure `waveformColumns` output in and calls draw().
 *
 * The input is the same interleaved `[iMin, iMax, qMin, qMax]`-per-column
 * layout as an Iq envelope payload, drawn as per-column min/max vertical
 * line pairs (I blue, Q red) — the Canvas2D analogue of the WebGL
 * WaveformRenderer's instanced GL_LINES path, with its y range, colors and
 * grid. Columns with no samples (more columns than samples in the rolling
 * window) read as `iMin > iMax` and are skipped.
 */

const COLOR_BG = "#14161a";
const COLOR_GRID = "rgba(255, 255, 255, 0.08)";
const COLOR_I = "#4fc3f7";
const COLOR_Q = "#ef9a9a";

/** Fixed y range, like the offline Iq view (hot signals clip — honestly). */
const Y_RANGE: readonly [number, number] = [-1.1, 1.1];

export class LiveWaveformRenderer {
  /** false when a 2D context is unavailable. */
  readonly ok: boolean;

  private readonly canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null = null;

  private cssW = 0;
  private cssH = 0;

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

  /** Set CSS-size backing store. `dpr` = devicePixelRatio. Triggers a draw. */
  resize(cssW: number, cssH: number, dpr: number): void {
    this.cssW = Math.max(0, cssW);
    this.cssH = Math.max(0, cssH);
    const pxW = Math.max(1, Math.round(this.cssW * dpr));
    const pxH = Math.max(1, Math.round(this.cssH * dpr));
    if (this.canvas.width !== pxW) this.canvas.width = pxW;
    if (this.canvas.height !== pxH) this.canvas.height = pxH;
    this.draw(new Float32Array(0));
  }

  /**
   * Redraw from the columns buffer (`nColumns = columns.length / 4`). Cheap
   * enough to call per animation frame: one pass, two fills per column.
   */
  draw(columns: Float32Array): void {
    const ctx = this.ctx;
    if (ctx === null) return;
    if (this.cssW <= 0 || this.cssH <= 0) return;

    const dpr = this.canvas.width / Math.max(1, this.cssW);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    ctx.fillStyle = COLOR_BG;
    ctx.fillRect(0, 0, this.cssW, this.cssH);
    this.drawGrid(ctx);

    const n = columns.length >> 2;
    if (n === 0) return;

    const [yMin, yMax] = Y_RANGE;
    const yOf = (v: number): number => ((v - yMin) / (yMax - yMin)) * this.cssH;
    const clampY = (y: number): number => Math.min(Math.max(y, 0), this.cssH);

    // Two passes (I trace then Q trace), like the GL renderer draws them, so
    // both stay readable where they overlap.
    for (const [color, lo, hi] of [
      [COLOR_I, 0, 1],
      [COLOR_Q, 2, 3],
    ] as const) {
      ctx.fillStyle = color;
      for (let c = 0; c < n; c++) {
        const o = c * 4;
        const vMin = columns[o + lo];
        const vMax = columns[o + hi];
        if (!(vMin <= vMax)) continue; // empty column (±Infinity)
        const x = ((c + 0.5) / n) * this.cssW;
        const y1 = clampY(yOf(vMin));
        let y2 = clampY(yOf(vMax));
        if (y2 - y1 < 1) y2 = y1 + 1; // min == max still shows a 1px tick
        ctx.fillRect(x - 0.5, y1, 1, y2 - y1);
      }
    }
  }

  private drawGrid(ctx: CanvasRenderingContext2D): void {
    const [yMin, yMax] = Y_RANGE;
    ctx.strokeStyle = COLOR_GRID;
    ctx.lineWidth = 1;
    for (const v of [-1, -0.5, 0, 0.5, 1]) {
      const y = Math.round(((v - yMin) / (yMax - yMin)) * this.cssH) + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(this.cssW, y);
      ctx.stroke();
    }
  }
}
