/**
 * App shell: header (open + meta + rate + mode), canvas area (WebGL2
 * waveform + interactions), status bar (cursor/view/selection/zoom/errors).
 *
 * All pixel<->sample math goes through the shared transform in
 * WaveformRenderer.ts (viewSampleToX / viewXToSample) — both via the
 * renderer's own methods and the pure functions — so overlays, hit-testing
 * and traces cannot drift apart.
 */

import {
  type DragEvent as ReactDragEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { pickIQFiles } from "../bridge/api";
import { WaveformMode } from "../bridge/protocol";
import { type ViewRange, WaveformRenderer, viewXToSample } from "../render/WaveformRenderer";
import { type ErrorState, useSignalView } from "./useSignalView";

/** Pan/select drag bookkeeping; `view0` is the view captured at pointer-down. */
interface DragState {
  kind: "pan" | "select";
  x0: number;
  view0: ViewRange;
  anchor: number; // selection anchor sample (select only)
}

const CLICK_SLOP_PX = 3;

function formatCount(n: number): string {
  return Math.floor(n).toLocaleString("en-US");
}

function trimNum(v: number): string {
  return Number(v.toFixed(3)).toString();
}

function formatHz(hz: number): string {
  if (!Number.isFinite(hz) || hz <= 0) return "—";
  if (hz >= 1e9) return `${trimNum(hz / 1e9)} GHz`;
  if (hz >= 1e6) return `${trimNum(hz / 1e6)} MHz`;
  if (hz >= 1e3) return `${trimNum(hz / 1e3)} kHz`;
  return `${trimNum(hz)} Hz`;
}

function formatDuration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return "—";
  if (secs === 0) return "0 s";
  if (secs >= 1) return `${trimNum(secs)} s`;
  if (secs >= 1e-3) return `${trimNum(secs * 1e3)} ms`;
  if (secs >= 1e-6) return `${trimNum(secs * 1e6)} µs`;
  return `${trimNum(secs * 1e9)} ns`;
}

function formatZoom(zoom: number): string {
  if (!Number.isFinite(zoom) || zoom <= 0) return "—";
  if (zoom >= 0.1) return zoom.toFixed(2);
  if (zoom >= 0.001) return zoom.toFixed(4);
  return zoom.toExponential(1);
}

/** Code-specific messages; never parses `message`. */
function errorText(err: ErrorState): string {
  switch (err.code) {
    case "file-not-found":
      return `File not found: ${err.message}`;
    case "unknown-signal":
      return `Signal is no longer open: ${err.message}`;
    case "invalid-range":
      return `View range rejected: ${err.message}`;
    case "invalid-argument":
      return `Invalid argument: ${err.message}`;
    case "io":
      return `Read error: ${err.message}`;
    case "internal":
      return `Internal error: ${err.message}`;
    default:
      return `${err.code}: ${err.message}`;
  }
}

export default function App() {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const rendererRef = useRef<WaveformRenderer | null>(null);
  const dragRef = useRef<DragState | null>(null);
  const sizeRef = useRef({ w: 0, h: 0 });
  const lastBinsWidthRef = useRef(-1);
  const [webglOk, setWebglOk] = useState(true);
  const [rateInput, setRateInput] = useState("");

  const getBins = useCallback(() => Math.max(1, Math.floor(sizeRef.current.w)), []);
  const {
    meta,
    mode,
    view,
    selection,
    cursor,
    snapshot,
    error,
    loading,
    openPath,
    setMode,
    applySampleRate,
    zoomAt,
    setView,
    resetView,
    setSelection,
    clearSelection,
    setCursor,
    refresh,
    reportError,
  } = useSignalView({ getBins });

  // --- open flows ---------------------------------------------------------------

  const openFromDialog = useCallback(async (): Promise<void> => {
    try {
      const paths = await pickIQFiles();
      if (paths.length > 0) await openPath(paths[0]);
    } catch (e) {
      reportError(e);
    }
  }, [openPath, reportError]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        clearSelection();
        return;
      }
      if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "o") {
        e.preventDefault();
        void openFromDialog();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openFromDialog, clearSelection]);

  const onDragOver = (e: ReactDragEvent<HTMLDivElement>): void => {
    e.preventDefault(); // allow the drop
  };

  const onDrop = (e: ReactDragEvent<HTMLDivElement>): void => {
    e.preventDefault();
    const file = e.dataTransfer.files[0];
    if (file === undefined) return;
    // Tauri exposes the absolute path on File.
    const path = (file as File & { path?: unknown }).path;
    if (typeof path === "string" && path.length > 0) {
      void openPath(path);
    } else {
      reportError(new Error("dropped file has no filesystem path"));
    }
  };

  // --- renderer lifecycle + resize -------------------------------------------------

  useEffect(() => {
    const canvas = canvasRef.current;
    const wrap = wrapRef.current;
    if (canvas === null || wrap === null) return;
    const renderer = new WaveformRenderer(canvas);
    rendererRef.current = renderer;
    setWebglOk(renderer.ok);

    const ro = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry === undefined) return;
      const { width, height } = entry.contentRect;
      sizeRef.current = { w: width, h: height };
      renderer.resize(width, height, window.devicePixelRatio || 1);
      const binsWidth = Math.max(1, Math.floor(width));
      if (binsWidth !== lastBinsWidthRef.current) {
        lastBinsWidthRef.current = binsWidth;
        refresh(); // bin budget changed -> fresh id re-request
      }
    });
    ro.observe(wrap);
    return () => {
      ro.disconnect();
      renderer.dispose();
      rendererRef.current = null;
    };
  }, [refresh]);

  // Push state into the renderer (and redraw).
  useEffect(() => {
    const r = rendererRef.current;
    if (r === null) return;
    r.setMode(mode);
    r.setView(view);
    r.setEnvelope(snapshot === null ? null : snapshot.env, snapshot === null ? undefined : snapshot.view);
    r.setSelection(selection);
    r.setCursor(cursor);
    r.draw();
  }, [mode, view, snapshot, selection, cursor]);

  // --- canvas interactions -----------------------------------------------------------

  // Wheel must be non-passive to preventDefault page scroll; React's onWheel
  // is passive at the root, so attach natively.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas === null) return;
    const hasSignal = meta !== null;
    const onWheel = (e: WheelEvent): void => {
      if (!hasSignal) return;
      e.preventDefault();
      const rect = canvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const factor = Math.pow(1.0015, -e.deltaY);
      zoomAt(sizeRef.current.w, x, factor);
    };
    canvas.addEventListener("wheel", onWheel, { passive: false });
    return () => canvas.removeEventListener("wheel", onWheel);
  }, [meta, zoomAt]);

  const onPointerDown = (e: ReactPointerEvent<HTMLCanvasElement>): void => {
    if (e.button !== 0 || meta === null) return;
    const x = e.clientX - e.currentTarget.getBoundingClientRect().left;
    e.currentTarget.setPointerCapture(e.pointerId);
    if (e.shiftKey) {
      const anchor = Math.round(viewXToSample(view, sizeRef.current.w, x));
      dragRef.current = { kind: "select", x0: x, view0: view, anchor };
      setSelection({ selStart: anchor, selEnd: anchor });
    } else {
      dragRef.current = { kind: "pan", x0: x, view0: view, anchor: 0 };
    }
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLCanvasElement>): void => {
    if (meta === null) return;
    const x = e.clientX - e.currentTarget.getBoundingClientRect().left;
    const cssW = sizeRef.current.w;
    const drag = dragRef.current;
    if (drag === null) {
      setCursor(viewXToSample(view, cssW, x));
      return;
    }
    if (drag.kind === "pan") {
      // Keep the grabbed sample under the pointer: anchored on the view
      // captured at pointer-down, measured with the shared transform.
      const offset = viewXToSample(drag.view0, cssW, x) - viewXToSample(drag.view0, cssW, drag.x0);
      const start = drag.view0.start - offset;
      const end = drag.view0.end - offset;
      setView({ start, end });
      setCursor(viewXToSample({ start, end }, cssW, x));
    } else {
      const head = viewXToSample(drag.view0, cssW, x);
      setSelection({ selStart: drag.anchor, selEnd: head });
      setCursor(head);
    }
  };

  const onPointerUp = (e: ReactPointerEvent<HTMLCanvasElement>): void => {
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    const drag = dragRef.current;
    dragRef.current = null;
    if (drag === null) return;
    const x = e.clientX - e.currentTarget.getBoundingClientRect().left;
    if (Math.abs(x - drag.x0) < CLICK_SLOP_PX) {
      clearSelection(); // a click without a drag clears the selection
    }
  };

  const onPointerLeave = (): void => {
    if (dragRef.current === null) setCursor(null);
  };

  // --- sample-rate input --------------------------------------------------------------

  useEffect(() => {
    setRateInput(meta === null ? "" : String(Math.round(meta.sample_rate)));
  }, [meta]);

  const revertRate = useCallback((): void => {
    setRateInput(meta === null ? "" : String(Math.round(meta.sample_rate)));
  }, [meta]);

  const commitRate = useCallback((): void => {
    if (meta === null) return;
    const parsed = Number.parseFloat(rateInput.replace(/[,\s_]/g, ""));
    if (Number.isFinite(parsed) && parsed > 0 && parsed !== meta.sample_rate) {
      void applySampleRate(parsed);
    } else {
      revertRate();
    }
  }, [meta, rateInput, applySampleRate, revertRate]);

  const onRateKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>): void => {
    if (e.key === "Enter") {
      e.currentTarget.blur(); // commits via onBlur
    } else if (e.key === "Escape") {
      e.stopPropagation(); // don't clear the selection too
      revertRate();
      e.currentTarget.blur();
    }
  };

  // --- derived status ------------------------------------------------------------------

  const total = meta === null ? 0 : meta.sample_count;
  const span = view.end - view.start;
  const rate = meta === null ? 0 : meta.sample_rate;
  const cursorText =
    cursor === null || meta === null ? null : `${formatCount(cursor)}${rate > 0 ? ` · ${formatDuration(cursor / rate)}` : ""}`;
  const selText =
    selection === null
      ? null
      : `${formatCount(Math.min(selection.selStart, selection.selEnd))} – ${formatCount(
          Math.max(selection.selStart, selection.selEnd),
        )}`;

  return (
    <div className="app" onDragOver={onDragOver} onDrop={onDrop}>
      <header className="header">
        <span className="app-name">urh</span>
        <button type="button" onClick={() => void openFromDialog()}>
          Open…
        </button>
        {meta !== null && (
          <div className="chips">
            <span className="chip">{meta.format}</span>
            <span className="chip">{formatCount(meta.sample_count)} samples</span>
            <span className="chip">{formatHz(meta.sample_rate)}</span>
            <span className="chip">{formatDuration(meta.duration_secs)}</span>
          </div>
        )}
        <span className="spacer" />
        <input
          className="rate-input"
          type="text"
          inputMode="decimal"
          spellCheck={false}
          aria-label="Sample rate (Hz)"
          placeholder="rate (Hz)"
          value={rateInput}
          disabled={meta === null}
          onChange={(e) => setRateInput(e.target.value)}
          onKeyDown={onRateKeyDown}
          onBlur={commitRate}
        />
        <span className="rate-label">Hz</span>
        <div className="seg" role="group" aria-label="Waveform mode">
          <button
            type="button"
            className={`seg-btn${mode === WaveformMode.Iq ? " active" : ""}`}
            onClick={() => setMode(WaveformMode.Iq)}
          >
            I/Q
          </button>
          <button
            type="button"
            className={`seg-btn${mode === WaveformMode.Amplitude ? " active" : ""}`}
            onClick={() => setMode(WaveformMode.Amplitude)}
          >
            Amplitude
          </button>
        </div>
      </header>

      <main className="canvas-area" ref={wrapRef}>
        <canvas
          ref={canvasRef}
          className="waveform"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerLeave={onPointerLeave}
          onDoubleClick={() => resetView()}
        />
        {meta === null && (
          <div className="overlay-msg">
            <span className="hint">Open a .complex IQ file (⌘/Ctrl+O)</span>
          </div>
        )}
        {!webglOk && (
          <div className="overlay-msg error">
            <span className="hint">WebGL2 is not available — waveform rendering is disabled.</span>
          </div>
        )}
      </main>

      <footer className="status-bar">
        {loading && <span className="loading-dot" aria-label="loading" />}
        <span className="status-item">
          <span className="label">cursor</span>
          <span className="value">{cursorText ?? "—"}</span>
        </span>
        <span className="status-item">
          <span className="label">view</span>
          <span className="value">
            {meta === null ? "—" : `${formatCount(view.start)} – ${formatCount(view.end)} (${formatCount(span)})`}
          </span>
        </span>
        <span className="status-item">
          <span className="label">sel</span>
          <span className="value">{selText ?? "—"}</span>
        </span>
        <span className="status-item">
          <span className="label">zoom</span>
          <span className="value">{total > 0 ? formatZoom(span / total) : "—"}</span>
        </span>
        {error !== null && (
          <span className="status-error" role="alert">
            {errorText(error)}
          </span>
        )}
      </footer>
    </div>
  );
}
