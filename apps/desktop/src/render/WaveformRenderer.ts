/**
 * Raw WebGL2 waveform renderer. No React inside — React pushes state in via
 * setters and calls draw().
 *
 * Envelope data is drawn straight from the decoded frame's interleaved
 * Float32Array using stride/offset vertex attribs; nothing is copied or
 * re-packed into JS arrays per frame.
 *
 * Per-bin min/max rendering uses instanced GL_LINES (2 verts per instance,
 * corner selector 0/1 picks min vs max); when the view is zoomed in far
 * enough that the envelope is degenerate (span / nBins <= 1.5), a
 * GL_LINE_STRIP polyline of the per-sample values is drawn instead —
 * zero-height GL_LINES would be invisible.
 */

import type { DecodedEnvelope } from "../bridge/protocol";
import { WaveformMode } from "../bridge/protocol";
import { createBuffer, createVao, linkProgram } from "./gl-util";

/** Half-open sample interval `[start, end)`. */
export interface ViewRange {
  start: number;
  end: number;
}

/**
 * Half-open sample interval used for *ranges over* the view (message
 * extents, normalized selections) — structurally a {@link ViewRange}, kept
 * distinct so intent is explicit.
 */
export interface Interval {
  start: number;
  end: number;
}

/** Selection in samples; `selStart` is the drag anchor, `selEnd` the head. */
export interface SelectionRange {
  selStart: number;
  selEnd: number;
}

/**
 * THE screen<->sample transform. Pure, shared by rendering (overlays) and by
 * every interaction (pan/zoom/selection hit-testing) so overlays cannot drift
 * from data. `x` is in CSS pixels from the left edge, `sample` may be
 * fractional; the renderer's bin layout puts bin k's center at
 * `(k + 0.5) / nBins * cssW`, which is exactly `viewSampleToX` of the bin's
 * center sample, so traces and overlays agree by construction.
 */
export function viewSampleToX(view: ViewRange, cssW: number, sample: number): number {
  const span = view.end - view.start;
  if (span <= 0 || cssW <= 0) return 0;
  return ((sample - view.start) / span) * cssW;
}

/** Inverse of {@link viewSampleToX}. */
export function viewXToSample(view: ViewRange, cssW: number, x: number): number {
  const span = view.end - view.start;
  if (cssW <= 0) return view.start;
  return view.start + (x / cssW) * span;
}

// --- colors (spec) -----------------------------------------------------------

const COLOR_BG: readonly [number, number, number, number] = [0x14 / 255, 0x16 / 255, 0x1a / 255, 1];
const COLOR_GRID: readonly [number, number, number, number] = [1, 1, 1, 0.08];
const COLOR_I: readonly [number, number, number, number] = [79 / 255, 195 / 255, 247 / 255, 1];
const COLOR_Q: readonly [number, number, number, number] = [239 / 255, 154 / 255, 154 / 255, 1];
const COLOR_MAG: readonly [number, number, number, number] = [129 / 255, 199 / 255, 132 / 255, 1];
const COLOR_SEL_FILL: readonly [number, number, number, number] = [79 / 255, 195 / 255, 247 / 255, 0.15];
const COLOR_SEL_EDGE: readonly [number, number, number, number] = [79 / 255, 195 / 255, 247 / 255, 0.8];
const COLOR_SEL_HANDLE: readonly [number, number, number, number] = [79 / 255, 195 / 255, 247 / 255, 0.3];
const COLOR_MSG_FILL: readonly [number, number, number, number] = [255 / 255, 176 / 255, 0 / 255, 0.1];
const COLOR_MSG_EDGE: readonly [number, number, number, number] = [255 / 255, 176 / 255, 0 / 255, 0.65];
const COLOR_CURSOR: readonly [number, number, number, number] = [1, 1, 1, 0.35];

const Y_RANGE_IQ: readonly [number, number] = [-1.1, 1.1];
const Y_RANGE_MAG: readonly [number, number] = [0, 1.1];

/** Switch to the polyline path at or below this samples-per-bin ratio. */
const DEGENERATE_SAMPLES_PER_BIN = 1.5;

/** Selection edge handles: half-width and cap height, in CSS pixels. */
const SEL_HANDLE_HALF_PX = 4;
const SEL_HANDLE_CAP_PX = 3;

// Fixed WebGL enum values (spec-constant; usable without a live context).
const GL_LINES = 0x0001;
const GL_TRIANGLE_FAN = 0x0006;

// --- shaders -----------------------------------------------------------------
// Bin centers are uniform across the view, so x comes from the vertex/instance
// id; only y travels through the attribute (the envelope floats).

const VS_BINS = `#version 300 es
layout(location = 0) in float aCorner;   // per-vertex: 0 = min, 1 = max
layout(location = 1) in vec2  aMinMax;   // per-instance: one bin's (min, max)
uniform float uNBins;
uniform float uYMin;
uniform float uYMax;
void main() {
  float x = ((float(gl_InstanceID) + 0.5) / uNBins) * 2.0 - 1.0;
  float y = mix(aMinMax.x, aMinMax.y, aCorner);
  gl_Position = vec4(x, (y - uYMin) / (uYMax - uYMin) * 2.0 - 1.0, 0.0, 1.0);
}
`;

const VS_STRIP = `#version 300 es
layout(location = 0) in float aVal;      // per-vertex sample value (min == max)
uniform float uNBins;
uniform float uYMin;
uniform float uYMax;
void main() {
  float x = ((float(gl_VertexID) + 0.5) / uNBins) * 2.0 - 1.0;
  gl_Position = vec4(x, (aVal - uYMin) / (uYMax - uYMin) * 2.0 - 1.0, 0.0, 1.0);
}
`;

const VS_OVERLAY = `#version 300 es
layout(location = 0) in vec2 aPos;       // clip-space, supplied by the app
void main() {
  gl_Position = vec4(aPos, 0.0, 1.0);
}
`;

const FS_SOLID = `#version 300 es
precision mediump float;
uniform vec4 uColor;
out vec4 fragColor;
void main() {
  fragColor = uColor;
}
`;

interface TraceUniforms {
  nBins: WebGLUniformLocation;
  yMin: WebGLUniformLocation;
  yMax: WebGLUniformLocation;
  color: WebGLUniformLocation;
}

interface OverlayUniforms {
  color: WebGLUniformLocation;
}

interface GlResources {
  progBins: WebGLProgram;
  progStrip: WebGLProgram;
  progOverlay: WebGLProgram;
  uBins: TraceUniforms;
  uStrip: TraceUniforms;
  uOverlay: OverlayUniforms;
  envBuf: WebGLBuffer;
  cornerBuf: WebGLBuffer;
  overlayBuf: WebGLBuffer;
  /** instanced min/max lines, one VAO per trace component layout */
  vaoBinsI: WebGLVertexArrayObject;
  vaoBinsQ: WebGLVertexArrayObject;
  vaoBinsMag: WebGLVertexArrayObject;
  /** degenerate polyline, one VAO per trace component layout */
  vaoStripI: WebGLVertexArrayObject;
  vaoStripQ: WebGLVertexArrayObject;
  vaoStripMag: WebGLVertexArrayObject;
  vaoOverlay: WebGLVertexArrayObject;
}

function uniform(gl: WebGL2RenderingContext, program: WebGLProgram, name: string): WebGLUniformLocation {
  const loc = gl.getUniformLocation(program, name);
  if (loc === null) throw new Error(`uniform ${name} not found in program`);
  return loc;
}

function traceUniforms(gl: WebGL2RenderingContext, program: WebGLProgram): TraceUniforms {
  return {
    nBins: uniform(gl, program, "uNBins"),
    yMin: uniform(gl, program, "uYMin"),
    yMax: uniform(gl, program, "uYMax"),
    color: uniform(gl, program, "uColor"),
  };
}

export class WaveformRenderer {
  /** false when WebGL2 is unavailable; transforms still work, draws do not. */
  readonly ok: boolean;

  private readonly canvas: HTMLCanvasElement;
  private gl: WebGL2RenderingContext | null = null;
  private lost = false;
  private res: GlResources | null = null;

  private cssW = 0;
  private cssH = 0;
  private pxW = 0;
  private pxH = 0;

  private env: DecodedEnvelope | null = null;
  /** View the envelope was requested for (degenerate-path threshold). */
  private envView: ViewRange | null = null;
  private view: ViewRange = { start: 0, end: 1 };
  private selection: SelectionRange | null = null;
  /** Protocol-table message extent `[start, end)`, highlighted in amber. */
  private messageHighlight: Interval | null = null;
  private cursor: number | null = null;
  private mode: WaveformMode = WaveformMode.Iq;

  private readonly onContextLost = (e: Event): void => {
    e.preventDefault(); // required, or the context never restores
    this.lost = true;
  };

  private readonly onContextRestored = (): void => {
    this.lost = false;
    if (this.gl === null) return;
    this.initResources();
    this.uploadEnvelope();
    this.draw();
  };

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const gl = canvas.getContext("webgl2", { alpha: false, antialias: true, depth: false });
    if (gl === null) {
      this.ok = false;
      return;
    }
    this.ok = true;
    this.gl = gl;
    this.initResources();
    canvas.addEventListener("webglcontextlost", this.onContextLost);
    canvas.addEventListener("webglcontextrestored", this.onContextRestored);
  }

  dispose(): void {
    this.canvas.removeEventListener("webglcontextlost", this.onContextLost);
    this.canvas.removeEventListener("webglcontextrestored", this.onContextRestored);
  }

  // --- state in ---------------------------------------------------------------

  /** Set CSS-size backing store. `dpr` = devicePixelRatio. Triggers a draw. */
  resize(cssW: number, cssH: number, dpr: number): void {
    this.cssW = Math.max(0, cssW);
    this.cssH = Math.max(0, cssH);
    this.pxW = Math.max(1, Math.round(this.cssW * dpr));
    this.pxH = Math.max(1, Math.round(this.cssH * dpr));
    if (this.canvas.width !== this.pxW) this.canvas.width = this.pxW;
    if (this.canvas.height !== this.pxH) this.canvas.height = this.pxH;
    this.draw();
  }

  /**
   * Upload the decoded envelope. `sourceView` is the view the envelope was
   * requested for; it decides the degenerate-polyline threshold so a stale
   * envelope from a wider view is never drawn as a per-sample polyline.
   */
  setEnvelope(env: DecodedEnvelope | null, sourceView?: ViewRange): void {
    this.env = env;
    this.envView = env === null ? null : (sourceView ?? this.view);
    this.uploadEnvelope();
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

  /** UI mode; only used for the y range / grid while no envelope is loaded. */
  setMode(mode: WaveformMode): void {
    this.mode = mode;
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

  /** CSS pixel -> (fractional) sample, via the shared pure transform. */
  xToSample(x: number): number {
    return viewXToSample(this.view, this.cssW, x);
  }

  // --- drawing -----------------------------------------------------------------

  draw(): void {
    const gl = this.gl;
    if (gl === null || this.lost || this.res === null) return;
    if (this.cssW <= 0 || this.cssH <= 0) return;

    gl.viewport(0, 0, this.pxW, this.pxH);
    gl.disable(gl.SCISSOR_TEST);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);

    gl.clearColor(...COLOR_BG);
    gl.clear(gl.COLOR_BUFFER_BIT);

    const env = this.env;
    const mode = env === null ? this.mode : env.mode;
    const [yMin, yMax] = mode === WaveformMode.Iq ? Y_RANGE_IQ : Y_RANGE_MAG;

    this.drawGrid(mode, yMin, yMax);

    if (env !== null && env.nBins > 0 && env.data.length > 0) {
      const srcSpan = this.envView === null ? 0 : this.envView.end - this.envView.start;
      if (srcSpan / env.nBins <= DEGENERATE_SAMPLES_PER_BIN) {
        this.drawStrip(env, yMin, yMax);
      } else {
        this.drawBins(env, yMin, yMax);
      }
    }

    if (this.messageHighlight !== null) this.drawMessageHighlight();
    if (this.selection !== null) this.drawSelection();
    if (this.cursor !== null) this.drawCursor();
  }

  private drawGrid(mode: WaveformMode, yMin: number, yMax: number): void {
    const lines = mode === WaveformMode.Iq ? [-1, -0.5, 0, 0.5, 1] : [0, 0.25, 0.5, 0.75, 1];
    this.resetOverlay();
    for (const v of lines) {
      const y = ((v - yMin) / (yMax - yMin)) * 2 - 1;
      this.pushOverlayVert(-1, y);
      this.pushOverlayVert(1, y);
    }
    this.flushOverlay(GL_LINES, COLOR_GRID);
  }

  private drawBins(env: DecodedEnvelope, yMin: number, yMax: number): void {
    const gl = this.gl;
    const res = this.res;
    if (gl === null || res === null) return;
    gl.useProgram(res.progBins);
    gl.uniform1f(res.uBins.nBins, env.nBins);
    gl.uniform1f(res.uBins.yMin, yMin);
    gl.uniform1f(res.uBins.yMax, yMax);
    if (env.mode === WaveformMode.Iq) {
      gl.uniform4f(res.uBins.color, ...COLOR_I);
      gl.bindVertexArray(res.vaoBinsI);
      gl.drawArraysInstanced(gl.LINES, 0, 2, env.nBins);
      gl.uniform4f(res.uBins.color, ...COLOR_Q);
      gl.bindVertexArray(res.vaoBinsQ);
      gl.drawArraysInstanced(gl.LINES, 0, 2, env.nBins);
    } else {
      gl.uniform4f(res.uBins.color, ...COLOR_MAG);
      gl.bindVertexArray(res.vaoBinsMag);
      gl.drawArraysInstanced(gl.LINES, 0, 2, env.nBins);
    }
    gl.bindVertexArray(null);
  }

  private drawStrip(env: DecodedEnvelope, yMin: number, yMax: number): void {
    const gl = this.gl;
    const res = this.res;
    if (gl === null || res === null) return;
    gl.useProgram(res.progStrip);
    gl.uniform1f(res.uStrip.nBins, env.nBins);
    gl.uniform1f(res.uStrip.yMin, yMin);
    gl.uniform1f(res.uStrip.yMax, yMax);
    if (env.mode === WaveformMode.Iq) {
      gl.uniform4f(res.uStrip.color, ...COLOR_I);
      gl.bindVertexArray(res.vaoStripI);
      gl.drawArrays(gl.LINE_STRIP, 0, env.nBins);
      gl.uniform4f(res.uStrip.color, ...COLOR_Q);
      gl.bindVertexArray(res.vaoStripQ);
      gl.drawArrays(gl.LINE_STRIP, 0, env.nBins);
    } else {
      gl.uniform4f(res.uStrip.color, ...COLOR_MAG);
      gl.bindVertexArray(res.vaoStripMag);
      gl.drawArrays(gl.LINE_STRIP, 0, env.nBins);
    }
    gl.bindVertexArray(null);
  }

  private drawSelection(): void {
    const sel = this.selection;
    if (sel === null) return;
    // selStart is the drag anchor and may be > selEnd; order for drawing.
    const x1 = this.clipX(this.sampleToX(Math.min(sel.selStart, sel.selEnd)));
    const x2 = this.clipX(this.sampleToX(Math.max(sel.selStart, sel.selEnd)));
    this.resetOverlay();
    this.pushOverlayVert(x1, -1);
    this.pushOverlayVert(x2, -1);
    this.pushOverlayVert(x2, 1);
    this.pushOverlayVert(x1, 1);
    this.flushOverlay(GL_TRIANGLE_FAN, COLOR_SEL_FILL);
    this.resetOverlay();
    this.pushOverlayVert(x1, -1);
    this.pushOverlayVert(x1, 1);
    this.pushOverlayVert(x2, -1);
    this.pushOverlayVert(x2, 1);
    this.flushOverlay(GL_LINES, COLOR_SEL_EDGE);
    this.drawEdgeHandles(x1, x2);
  }

  /**
   * Grab handles at the selection edges: a wider translucent bar plus cap
   * ticks top and bottom, sized in CSS pixels so they stay grabbable at any
   * zoom level (hit-testing in the app uses the same CSS-pixel tolerance).
   */
  private drawEdgeHandles(x1: number, x2: number): void {
    const half = this.pxToClipX(SEL_HANDLE_HALF_PX);
    const cap = this.pxToClipY(SEL_HANDLE_CAP_PX);
    for (const x of [x1, x2]) {
      const l = Math.max(-1, x - half);
      const r = Math.min(1, x + half);
      if (r <= -1 || l >= 1) continue; // fully off-canvas
      this.resetOverlay();
      this.pushOverlayVert(l, -1);
      this.pushOverlayVert(r, -1);
      this.pushOverlayVert(r, 1);
      this.pushOverlayVert(l, 1);
      this.flushOverlay(GL_TRIANGLE_FAN, COLOR_SEL_HANDLE);
      this.resetOverlay();
      this.pushOverlayVert(l, -1);
      this.pushOverlayVert(r, -1);
      this.pushOverlayVert(l, -1 + cap);
      this.pushOverlayVert(r, -1 + cap);
      this.pushOverlayVert(l, 1 - cap);
      this.pushOverlayVert(r, 1 - cap);
      this.pushOverlayVert(l, 1);
      this.pushOverlayVert(r, 1);
      this.flushOverlay(GL_LINES, COLOR_SEL_EDGE);
    }
  }

  /** Amber band + solid end bars; visually distinct from the blue selection. */
  private drawMessageHighlight(): void {
    const m = this.messageHighlight;
    if (m === null || !(m.start < m.end)) return;
    const x1 = this.clipX(this.sampleToX(m.start));
    const x2 = this.clipX(this.sampleToX(m.end));
    this.resetOverlay();
    this.pushOverlayVert(x1, -1);
    this.pushOverlayVert(x2, -1);
    this.pushOverlayVert(x2, 1);
    this.pushOverlayVert(x1, 1);
    this.flushOverlay(GL_TRIANGLE_FAN, COLOR_MSG_FILL);
    const bar = this.pxToClipX(2);
    for (const x of [x1, x2]) {
      const l = Math.max(-1, x - bar);
      const r = Math.min(1, x + bar);
      if (r <= -1 || l >= 1) continue;
      this.resetOverlay();
      this.pushOverlayVert(l, -1);
      this.pushOverlayVert(r, -1);
      this.pushOverlayVert(r, 1);
      this.pushOverlayVert(l, 1);
      this.flushOverlay(GL_TRIANGLE_FAN, COLOR_MSG_EDGE);
    }
  }

  private drawCursor(): void {
    if (this.cursor === null) return;
    const x = this.clipX(this.sampleToX(this.cursor));
    this.resetOverlay();
    this.pushOverlayVert(x, -1);
    this.pushOverlayVert(x, 1);
    this.flushOverlay(GL_LINES, COLOR_CURSOR);
  }

  /** CSS x -> clip-space x. */
  private clipX(xCss: number): number {
    return this.cssW <= 0 ? -1 : (xCss / this.cssW) * 2 - 1;
  }

  /** A CSS-pixel width -> clip-space width (clamped to the canvas). */
  private pxToClipX(px: number): number {
    return this.cssW <= 0 ? 0 : Math.min((px / this.cssW) * 2, 2);
  }

  /** A CSS-pixel height -> clip-space height (clamped to the canvas). */
  private pxToClipY(px: number): number {
    return this.cssH <= 0 ? 0 : Math.min((px / this.cssH) * 2, 2);
  }

  /** Reusable overlay vertex scratch; rebuilt/grown on demand, never per-bin. */
  private overlayData = new Float32Array(64);
  private overlayVertCount = 0;

  private resetOverlay(): void {
    this.overlayVertCount = 0;
  }

  private pushOverlayVert(x: number, y: number): void {
    if (this.overlayVertCount * 2 + 2 > this.overlayData.length) {
      const bigger = new Float32Array(this.overlayData.length * 2);
      bigger.set(this.overlayData);
      this.overlayData = bigger;
    }
    this.overlayData[this.overlayVertCount * 2] = x;
    this.overlayData[this.overlayVertCount * 2 + 1] = y;
    this.overlayVertCount++;
  }

  private flushOverlay(mode: number, color: readonly [number, number, number, number]): void {
    const gl = this.gl;
    const res = this.res;
    if (gl === null || res === null || this.overlayVertCount === 0) return;
    gl.useProgram(res.progOverlay);
    gl.uniform4f(res.uOverlay.color, ...color);
    gl.bindBuffer(gl.ARRAY_BUFFER, res.overlayBuf);
    gl.bufferData(
      gl.ARRAY_BUFFER,
      this.overlayData.subarray(0, this.overlayVertCount * 2),
      gl.DYNAMIC_DRAW,
    );
    gl.bindVertexArray(res.vaoOverlay);
    gl.drawArrays(mode, 0, this.overlayVertCount);
    gl.bindVertexArray(null);
  }

  // --- GL plumbing --------------------------------------------------------------

  private initResources(): void {
    const gl = this.gl;
    if (gl === null) return;

    const progBins = linkProgram(gl, VS_BINS, FS_SOLID);
    const progStrip = linkProgram(gl, VS_STRIP, FS_SOLID);
    const progOverlay = linkProgram(gl, VS_OVERLAY, FS_SOLID);

    const envBuf = createBuffer(gl);
    const cornerBuf = createBuffer(gl);
    gl.bindBuffer(gl.ARRAY_BUFFER, cornerBuf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 1]), gl.STATIC_DRAW);
    const overlayBuf = createBuffer(gl);

    const IQ_STRIDE = 16; // bytes per bin: 4 x f32
    const MAG_STRIDE = 8; // bytes per bin: 2 x f32

    // Instanced min/max lines: location 0 = corner selector (per vertex),
    // location 1 = (min, max) pair (per instance) out of the envelope buffer.
    const makeBinsVao = (stride: number, byteOffset: number): WebGLVertexArrayObject => {
      const vao = createVao(gl);
      gl.bindVertexArray(vao);
      gl.bindBuffer(gl.ARRAY_BUFFER, cornerBuf);
      gl.enableVertexAttribArray(0);
      gl.vertexAttribPointer(0, 1, gl.FLOAT, false, 0, 0);
      gl.bindBuffer(gl.ARRAY_BUFFER, envBuf);
      gl.enableVertexAttribArray(1);
      gl.vertexAttribPointer(1, 2, gl.FLOAT, false, stride, byteOffset);
      gl.vertexAttribDivisor(1, 1);
      gl.bindVertexArray(null);
      return vao;
    };

    // Degenerate polyline: one f32 per vertex out of the envelope buffer.
    const makeStripVao = (stride: number, byteOffset: number): WebGLVertexArrayObject => {
      const vao = createVao(gl);
      gl.bindVertexArray(vao);
      gl.bindBuffer(gl.ARRAY_BUFFER, envBuf);
      gl.enableVertexAttribArray(0);
      gl.vertexAttribPointer(0, 1, gl.FLOAT, false, stride, byteOffset);
      gl.bindVertexArray(null);
      return vao;
    };

    const vaoOverlay = createVao(gl);
    gl.bindVertexArray(vaoOverlay);
    gl.bindBuffer(gl.ARRAY_BUFFER, overlayBuf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    this.res = {
      progBins,
      progStrip,
      progOverlay,
      uBins: traceUniforms(gl, progBins),
      uStrip: traceUniforms(gl, progStrip),
      uOverlay: { color: uniform(gl, progOverlay, "uColor") },
      envBuf,
      cornerBuf,
      overlayBuf,
      vaoBinsI: makeBinsVao(IQ_STRIDE, 0),
      vaoBinsQ: makeBinsVao(IQ_STRIDE, 8),
      vaoBinsMag: makeBinsVao(MAG_STRIDE, 0),
      vaoStripI: makeStripVao(IQ_STRIDE, 0),
      vaoStripQ: makeStripVao(IQ_STRIDE, 8),
      vaoStripMag: makeStripVao(MAG_STRIDE, 0),
      vaoOverlay,
    };
  }

  private uploadEnvelope(): void {
    const gl = this.gl;
    const res = this.res;
    if (gl === null || res === null || this.lost) return;
    gl.bindBuffer(gl.ARRAY_BUFFER, res.envBuf);
    if (this.env === null) {
      gl.bufferData(gl.ARRAY_BUFFER, 0, gl.STATIC_DRAW);
    } else {
      gl.bufferData(gl.ARRAY_BUFFER, this.env.data, gl.STATIC_DRAW);
    }
    gl.bindBuffer(gl.ARRAY_BUFFER, null);
  }
}
