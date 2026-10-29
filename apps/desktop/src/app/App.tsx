/**
 * App shell: header (open + project open/save + signal list + meta + rate +
 * view mode + zoom-to-selection + analysis toggle + capture/generator/
 * simulator rail toggle), workspace (collapsible left rail with the Device/
 * Generator/Simulator panels; canvas area with WebGL2 waveform OR Canvas2D
 * spectrogram — replaced by the live capture view while a capture runs;
 * analysis side panel), project message banners, status bar.
 *
 * All pixel<->sample math goes through the shared transform in
 * WaveformRenderer.ts (viewSampleToX / viewXToSample) — both via the
 * renderers' own methods and the pure functions — so overlays, hit-testing
 * and traces cannot drift apart. Both canvases are kept mounted and toggled
 * via CSS so the GL/2D contexts (and their uploaded data) survive switches;
 * the live view likewise overlays them without unmounting.
 *
 * Interaction model: left-drag selects (edge handles are grabbable),
 * middle/right-drag pans, wheel zooms about the cursor, double-click zooms
 * to the selection when inside it (else resets the view), click-empty and
 * Esc clear the selection. The selection is the single source of truth the
 * analysis panel and the protocol table highlight read; the protocol table
 * conversely reports clicked messages back as the amber highlight.
 *
 * Capture (P3): stopping a recording refreshes the signal list and activates
 * the registered rec-<n> signal; Generate (P4) does the same for gen-<n>.
 * Project (P4): Open/Save Project… write and reopen .urhproj files (warnings
 * for per-signal reopen failures, errors as banners).
 */

import {
  type DragEvent as ReactDragEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import {
  type AnalysisParams,
  type SignalMeta,
  listSignals,
  pickIQFiles,
} from "../bridge/api";
import { SPECTROGRAM_WINDOWS, type SpectrogramWindow, WaveformMode } from "../bridge/protocol";
import { type Interval, type ViewRange, WaveformRenderer, viewSampleToX, viewXToSample } from "../render/WaveformRenderer";
import { SpectrogramRenderer } from "../render/SpectrogramRenderer";
import { AnalysisPanel } from "../components/AnalysisPanel";
import { ProtocolTable } from "../components/ProtocolTable";
import { CaptureLiveView } from "../components/CaptureLiveView";
import { DevicePanel } from "../components/DevicePanel";
import { GeneratorPanel } from "../components/GeneratorPanel";
import { SimulatorPanel } from "../components/SimulatorPanel";
import { errorText } from "./errors";
import { formatCount, formatDuration, formatHz, formatZoom } from "./format";
import { type SelectionEdge, selectionEdgeAt } from "./selection";
import { useAnalysis, parseParamFields } from "./useAnalysis";
import { type CaptureStopped, useCapture } from "./useCapture";
import { useGenerator } from "./useGenerator";
import { type ProjectSaveInput, type SignalViewState } from "./projectModel";
import { useProject } from "./useProject";
import { useSelection } from "./useSelection";
import { useSignalView } from "./useSignalView";
import { useSimulator } from "./useSimulator";
import { useSpectrogram } from "./useSpectrogram";
import { viewForMessage } from "./protocolTable";

/** Which canvas view is active. */
type ViewMode = "waveform" | "spectrogram";

/** Left-rail tool panels. */
type RailTab = "capture" | "generator" | "simulator";

/** Pan/select/edge drag bookkeeping; `view0` is the view captured at pointer-down. */
interface DragState {
  kind: "pan" | "select" | "edge";
  x0: number;
  view0: ViewRange;
  anchor: number; // selection anchor sample (select only)
  edge: SelectionEdge; // which handle (edge only)
  /** True once the pointer moved beyond the click slop (click vs drag). */
  moved: boolean;
}

const CLICK_SLOP_PX = 3;
/** Selection-edge hit tolerance in CSS px (handles are 8 px wide). */
const EDGE_HIT_PX = 6;

export default function App() {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const spectroCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const rendererRef = useRef<WaveformRenderer | null>(null);
  const spectroRendererRef = useRef<SpectrogramRenderer | null>(null);
  const dragRef = useRef<DragState | null>(null);
  const sizeRef = useRef({ w: 0, h: 0 });
  const lastBinsWidthRef = useRef(-1);
  const [webglOk, setWebglOk] = useState(true);
  const [spectroOk, setSpectroOk] = useState(true);
  const [rateInput, setRateInput] = useState("");
  const [viewMode, setViewMode] = useState<ViewMode>("waveform");
  const [panelOpen, setPanelOpen] = useState(true);
  const [selectedRow, setSelectedRow] = useState<number | null>(null);
  const [railTab, setRailTab] = useState<RailTab>("capture");
  const [railOpen, setRailOpen] = useState(false);
  const [signals, setSignals] = useState<SignalMeta[]>([]);

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
    activateSignal,
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

  const spectro = useSpectrogram(meta, view, { enabled: viewMode === "spectrogram" });
  const selectionApi = useSelection({ selection, setSelection, clearSelection, setView });
  const analysis = useAnalysis(meta);

  // --- signal list + capture/generator panels (P3/P4) --------------------------------

  /** Re-read the open-signal list (files, recordings, generated signals). */
  const refreshSignals = useCallback(async (): Promise<SignalMeta[]> => {
    try {
      const list = await listSignals();
      setSignals(list);
      return list;
    } catch (e) {
      reportError(e);
      return [];
    }
  }, [reportError]);

  useEffect(() => {
    void refreshSignals();
  }, [refreshSignals]);

  /** A stopped recording settled: refresh the list, activate the rec-<n> result. */
  const onCaptureStopped = useCallback(
    (result: CaptureStopped): void => {
      void refreshSignals().then((list) => {
        if (result.signalId === null) return;
        const m = list.find((s) => s.signal_id === result.signalId);
        if (m !== undefined) activateSignal(m);
      });
    },
    [refreshSignals, activateSignal],
  );

  const capture = useCapture({ onStopped: onCaptureStopped });

  /** Generate produced a gen-<n> signal: refresh the list, activate it. */
  const onGenerated = useCallback(
    (generated: SignalMeta): void => {
      void refreshSignals().then((list) => {
        const m = list.find((s) => s.signal_id === generated.signal_id);
        if (m !== undefined) activateSignal(m);
      });
    },
    [refreshSignals, activateSignal],
  );

  const generator = useGenerator({
    getActiveSignalId: () => meta?.signal_id ?? null,
    onGenerated,
  });

  const simulator = useSimulator();

  // --- project save/load (P4) --------------------------------------------------------

  /** Snapshot for save: file-backed signals, active id, the active signal's
   *  analysis params (when its fields parse) and its waveform view+mode. */
  const getSaveInput = useCallback((): ProjectSaveInput => {
    const activeId = meta === null ? null : meta.signal_id;
    const analysisBySignal: Record<string, AnalysisParams> = {};
    if (activeId !== null) {
      const parsed = parseParamFields(analysis.fields);
      if (parsed.ok) analysisBySignal[activeId] = parsed.params; // invalid fields are not saved
    }
    const viewBySignal: Record<string, SignalViewState> = {};
    if (activeId !== null) {
      viewBySignal[activeId] = { start: view.start, end: view.end, mode };
    }
    return { signals, activeSignalId: activeId, analysisBySignal, viewBySignal };
  }, [meta, signals, analysis.fields, view, mode]);

  const project = useProject({
    getSaveInput,
    refreshSignals,
    activateSignal,
    applyAnalysisFields: analysis.applyFields,
  });

  /** Rail toggle: clicking the active tab collapses the rail. */
  const toggleRail = useCallback(
    (tab: RailTab): void => {
      setRailOpen(railTab === tab ? !railOpen : true);
      setRailTab(tab);
    },
    [railTab, railOpen],
  );

  /** The name shown for the signal "Save as .complex" would write. */
  const saveTargetName = meta === null ? null : meta.display_name === "" ? meta.signal_id : meta.display_name;

  // A new result invalidates the clicked-row highlight (row indices are new).
  useEffect(() => {
    setSelectedRow(null);
  }, [analysis.result]);

  // --- protocol table wiring (before the renderer effects: they depend on it) ----

  /** The clicked message's extent, highlighted on the waveform/spectrogram. */
  const messageHighlight: Interval | null = useMemo(() => {
    if (selectedRow === null || analysis.result === null) return null;
    const m = analysis.result.messages.find((msg) => msg.index === selectedRow);
    return m === undefined ? null : { start: m.startSample, end: m.endSample };
  }, [selectedRow, analysis.result]);

  const onRowClick = useCallback(
    (index: number): void => {
      if (selectedRow === index) {
        setSelectedRow(null); // clicking the selected row clears the highlight
        return;
      }
      setSelectedRow(index);
      const m = analysis.result?.messages.find((msg) => msg.index === index);
      if (m !== undefined) setView(viewForMessage(m)); // scrolls it into view
    },
    [selectedRow, analysis.result, setView],
  );

  // --- open flows -----------------------------------------------------------------

  const openFromDialog = useCallback(async (): Promise<void> => {
    try {
      const paths = await pickIQFiles();
      if (paths.length > 0) {
        await openPath(paths[0]);
        await refreshSignals(); // the opened file registers a signal
      }
    } catch (e) {
      reportError(e);
    }
  }, [openPath, refreshSignals, reportError]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        clearSelection();
        setSelectedRow(null); // also clears the message highlight
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
      void openPath(path).then(() => void refreshSignals());
    } else {
      reportError(new Error("dropped file has no filesystem path"));
    }
  };

  // --- renderer lifecycle + resize -------------------------------------------------

  useEffect(() => {
    const canvas = canvasRef.current;
    const spectroCanvas = spectroCanvasRef.current;
    const wrap = wrapRef.current;
    if (canvas === null || spectroCanvas === null || wrap === null) return;
    const renderer = new WaveformRenderer(canvas);
    rendererRef.current = renderer;
    setWebglOk(renderer.ok);
    const spectroRenderer = new SpectrogramRenderer(spectroCanvas);
    spectroRendererRef.current = spectroRenderer;
    setSpectroOk(spectroRenderer.ok);

    const ro = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry === undefined) return;
      const { width, height } = entry.contentRect;
      sizeRef.current = { w: width, h: height };
      renderer.resize(width, height, window.devicePixelRatio || 1);
      spectroRenderer.resize(width, height, window.devicePixelRatio || 1);
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
      spectroRendererRef.current = null;
    };
  }, [refresh]);

  // Push state into the renderers (and redraw).
  useEffect(() => {
    const r = rendererRef.current;
    if (r === null) return;
    r.setMode(mode);
    r.setView(view);
    r.setEnvelope(snapshot === null ? null : snapshot.env, snapshot === null ? undefined : snapshot.view);
    r.setSelection(selection);
    r.setMessageHighlight(messageHighlight);
    r.setCursor(cursor);
    r.draw();
  }, [mode, view, snapshot, selection, cursor, messageHighlight]);

  useEffect(() => {
    const r = spectroRendererRef.current;
    if (r === null) return;
    r.setView(view);
    r.setTile(spectro.snapshot === null ? null : spectro.snapshot.tile, spectro.snapshot === null ? null : spectro.snapshot.view);
    r.setSelection(selection);
    r.setMessageHighlight(messageHighlight);
    r.setCursor(cursor);
    r.draw();
  }, [view, spectro.snapshot, selection, cursor, messageHighlight]);

  // Toggling the visible canvas changes no state; redraw the now-shown one
  // explicitly (a hidden WebGL buffer is not guaranteed to survive).
  useEffect(() => {
    rendererRef.current?.draw();
    spectroRendererRef.current?.draw();
  }, [viewMode]);

  // --- canvas interactions (shared by both canvases) ------------------------------

  // Wheel must be non-passive to preventDefault page scroll; React's onWheel
  // is passive at the root, so attach natively — to both canvases.
  useEffect(() => {
    const canvases = [canvasRef.current, spectroCanvasRef.current];
    const hasSignal = meta !== null;
    const onWheel = (e: WheelEvent): void => {
      if (!hasSignal) return;
      e.preventDefault();
      const rect = (e.currentTarget as HTMLCanvasElement).getBoundingClientRect();
      const x = e.clientX - rect.left;
      const factor = Math.pow(1.0015, -e.deltaY);
      zoomAt(sizeRef.current.w, x, factor);
    };
    for (const c of canvases) {
      c?.addEventListener("wheel", onWheel, { passive: false });
    }
    return () => {
      for (const c of canvases) {
        c?.removeEventListener("wheel", onWheel);
      }
    };
  }, [meta, zoomAt]);

  const onPointerDown = (e: ReactPointerEvent<HTMLCanvasElement>): void => {
    if (meta === null) return;
    if (e.button !== 0 && e.button !== 1 && e.button !== 2) return;
    const x = e.clientX - e.currentTarget.getBoundingClientRect().left;
    const cssW = sizeRef.current.w;
    e.currentTarget.setPointerCapture(e.pointerId);
    if (e.button === 0) {
      // Near a selection handle? Grab the edge. Otherwise start a new
      // selection (a plain left-drag); pan lives on middle/right-drag.
      const edge = selectionEdgeAt(selection, view, cssW, x, EDGE_HIT_PX);
      if (edge !== null && !e.shiftKey) {
        dragRef.current = { kind: "edge", x0: x, view0: view, anchor: 0, edge, moved: false };
        return;
      }
      const anchor = Math.round(viewXToSample(view, cssW, x));
      // The pending selection is NOT committed yet — click vs drag is only
      // decided after the slop threshold, so a click inside the current
      // selection can keep it (and double-click can zoom to it).
      dragRef.current = { kind: "select", x0: x, view0: view, anchor, edge: "start", moved: false };
    } else {
      e.preventDefault(); // suppress middle-click autoscroll / selection menu
      dragRef.current = { kind: "pan", x0: x, view0: view, anchor: 0, edge: "start", moved: false };
    }
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLCanvasElement>): void => {
    if (meta === null) return;
    const x = e.clientX - e.currentTarget.getBoundingClientRect().left;
    const cssW = sizeRef.current.w;
    const drag = dragRef.current;
    if (drag === null) {
      setCursor(viewXToSample(view, cssW, x));
      // Affordance: the resize cursor over a selection handle.
      const edge = selectionEdgeAt(selection, view, cssW, x, EDGE_HIT_PX);
      e.currentTarget.style.cursor = edge !== null ? "ew-resize" : "crosshair";
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
    } else if (drag.kind === "select") {
      if (!drag.moved && Math.abs(x - drag.x0) < CLICK_SLOP_PX) return; // still a click
      drag.moved = true;
      const head = viewXToSample(drag.view0, cssW, x);
      setSelection({ selStart: drag.anchor, selEnd: head });
      setCursor(head);
    } else {
      drag.moved = true;
      const sample = viewXToSample(drag.view0, cssW, x);
      selectionApi.moveEdge(drag.edge, sample);
      setCursor(sample);
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
    if (!drag.moved && drag.kind === "select") {
      // A click (no drag): inside the selection keeps it — so a double-click
      // inside the band can still zoom to it — anywhere else clears it.
      const sample = viewXToSample(drag.view0, sizeRef.current.w, x);
      const iv = selectionApi.interval;
      if (iv === null || sample < iv.start || sample >= iv.end) clearSelection();
    }
  };

  const onPointerLeave = (): void => {
    if (dragRef.current === null) setCursor(null);
  };

  const onContextMenu = (e: ReactMouseEvent<HTMLCanvasElement>): void => {
    e.preventDefault(); // right-drag pans; no menu on the canvases
  };

  /** Double-click: zoom to the selection when inside it, else reset the view. */
  const onDoubleClick = (e: ReactMouseEvent<HTMLCanvasElement>): void => {
    const x = e.clientX - e.currentTarget.getBoundingClientRect().left;
    const cssW = sizeRef.current.w;
    if (selectionApi.hasSelection && selection !== null) {
      const x1 = viewSampleToX(view, cssW, Math.min(selection.selStart, selection.selEnd));
      const x2 = viewSampleToX(view, cssW, Math.max(selection.selStart, selection.selEnd));
      if (x >= x1 && x <= x2) {
        selectionApi.zoomToSelection();
        return;
      }
    }
    resetView();
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

  // --- derived status --------------------------------------------------------------------

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
  const activeError = error ?? (viewMode === "spectrogram" ? spectro.error : null);
  const showWaveformControls = viewMode === "waveform";
  /** The live capture view replaces the offline canvases while capturing. */
  const captureLive = capture.status !== "idle";

  return (
    <div className="app" onDragOver={onDragOver} onDrop={onDrop}>
      <header className="header">
        <span className="app-name">urh</span>
        <button type="button" onClick={() => void openFromDialog()}>
          Open…
        </button>
        <button
          type="button"
          onClick={() => void project.load()}
          disabled={project.busy !== "idle"}
          title="Open a .urhproj — reopens its signals and restores analysis/view state"
        >
          Open Project…
        </button>
        <button
          type="button"
          onClick={() => void project.save()}
          disabled={signals.length === 0 || project.busy !== "idle"}
          title={
            signals.some((s) => s.path !== "")
              ? "Save the file-backed signals, the active signal and its analysis/view state as .urhproj"
              : "No file-backed signals to save (recordings and generated signals cannot be reopened)"
          }
        >
          Save Project…
        </button>
        {signals.length > 0 && (
          <select
            className="signal-select"
            aria-label="Open signals"
            title="Open signals (opened files, recordings, generated)"
            value={meta === null ? "" : meta.signal_id}
            onChange={(e) => {
              const m = signals.find((s) => s.signal_id === e.target.value);
              if (m !== undefined) activateSignal(m);
            }}
          >
            {meta === null && <option value="">signals…</option>}
            {meta !== null && !signals.some((s) => s.signal_id === meta.signal_id) && (
              // list not refreshed yet — keep the active signal selectable
              <option value={meta.signal_id}>{meta.signal_id}</option>
            )}
            {signals.map((s) => (
              <option key={s.signal_id} value={s.signal_id}>
                {s.display_name === "" ? s.signal_id : `${s.display_name} · ${s.signal_id}`}
              </option>
            ))}
          </select>
        )}
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
        <button
          type="button"
          onClick={selectionApi.zoomToSelection}
          disabled={!selectionApi.hasSelection}
          title="Zoom the view to the selection (or double-click inside it)"
        >
          Zoom to selection
        </button>
        <div className="seg" role="group" aria-label="View mode">
          <button
            type="button"
            className={`seg-btn${viewMode === "waveform" ? " active" : ""}`}
            onClick={() => setViewMode("waveform")}
          >
            Waveform
          </button>
          <button
            type="button"
            className={`seg-btn${viewMode === "spectrogram" ? " active" : ""}`}
            onClick={() => setViewMode("spectrogram")}
          >
            Spectrogram
          </button>
        </div>
        {showWaveformControls ? (
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
        ) : (
          <label className="window-select" title="STFT window size (hop = window / 2)">
            <span>window</span>
            <select
              aria-label="Spectrogram window size"
              value={String(spectro.windowSize)}
              disabled={meta === null}
              onChange={(e) => spectro.setWindowSize(Number(e.target.value) as SpectrogramWindow)}
            >
              {SPECTROGRAM_WINDOWS.map((w) => (
                <option key={w} value={w}>
                  {w}
                </option>
              ))}
            </select>
          </label>
        )}
        <div className="seg" role="group" aria-label="Tools">
          <button
            type="button"
            className={`seg-btn${railOpen && railTab === "capture" ? " active" : ""}`}
            onClick={() => toggleRail("capture")}
            title="Device / capture panel"
          >
            Capture
          </button>
          <button
            type="button"
            className={`seg-btn${railOpen && railTab === "generator" ? " active" : ""}`}
            onClick={() => toggleRail("generator")}
            title="Signal generator panel"
          >
            Generator
          </button>
          <button
            type="button"
            className={`seg-btn${railOpen && railTab === "simulator" ? " active" : ""}`}
            onClick={() => toggleRail("simulator")}
            title="Simulator panel"
          >
            Simulator
          </button>
        </div>
        <button
          type="button"
          className={`panel-toggle${panelOpen ? " active" : ""}`}
          aria-pressed={panelOpen}
          onClick={() => setPanelOpen((v) => !v)}
        >
          Analysis
        </button>
      </header>

      {(project.error !== null || project.warnings !== null) && (
        <div className="banner-strip" aria-label="Project messages">
          {project.error !== null && (
            <div className="error-banner project-banner" role="alert">
              <span>{errorText(project.error)}</span>
              <button type="button" className="link-btn" onClick={project.dismissError}>
                dismiss
              </button>
            </div>
          )}
          {project.warnings !== null && (
            <div className="warn-banner project-banner" role="alert">
              <span>
                {project.warnings.length === 1
                  ? "1 signal could not be re-opened — the rest of the project loaded:"
                  : `${project.warnings.length} signals could not be re-opened — the rest of the project loaded:`}
              </span>
              <ul className="project-warnings">
                {project.warnings.map((w, i) => (
                  <li key={i}>{w}</li>
                ))}
              </ul>
              <button type="button" className="link-btn" onClick={project.dismissWarnings}>
                dismiss
              </button>
            </div>
          )}
        </div>
      )}

      <main className="workspace">
        {railOpen && (
          <aside className="left-rail" aria-label="Capture, generator and simulator tools">
            {railTab === "capture" ? (
              <DevicePanel capture={capture} />
            ) : railTab === "generator" ? (
              <GeneratorPanel generator={generator} canSave={meta !== null} saveTargetName={saveTargetName} />
            ) : (
              <SimulatorPanel simulator={simulator} />
            )}
          </aside>
        )}
        <div className="canvas-area" ref={wrapRef}>
          <canvas
            ref={canvasRef}
            className="waveform"
            style={{ display: viewMode === "waveform" && !captureLive ? "block" : "none" }}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerLeave={onPointerLeave}
            onContextMenu={onContextMenu}
            onDoubleClick={onDoubleClick}
          />
          <canvas
            ref={spectroCanvasRef}
            className="waveform spectro"
            style={{ display: viewMode === "spectrogram" && !captureLive ? "block" : "none" }}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerLeave={onPointerLeave}
            onContextMenu={onContextMenu}
            onDoubleClick={onDoubleClick}
          />
          {captureLive && <CaptureLiveView capture={capture} />}
          {meta === null && !captureLive && (
            <div className="overlay-msg">
              <span className="hint">Open a .complex IQ file (⌘/Ctrl+O)</span>
            </div>
          )}
          {!webglOk && viewMode === "waveform" && !captureLive && (
            <div className="overlay-msg error">
              <span className="hint">WebGL2 is not available — waveform rendering is disabled.</span>
            </div>
          )}
          {!spectroOk && viewMode === "spectrogram" && !captureLive && (
            <div className="overlay-msg error">
              <span className="hint">Canvas2D is not available — spectrogram rendering is disabled.</span>
            </div>
          )}
        </div>

        {panelOpen && (
          <aside className="side-panel" aria-label="Protocol analysis">
            <AnalysisPanel analysis={analysis} selectionInterval={selectionApi.interval} enabled={meta !== null} />
            <ProtocolTable
              result={analysis.result}
              labels={analysis.labels}
              selectionInterval={selectionApi.interval}
              selectedRow={selectedRow}
              onRowClick={onRowClick}
            />
          </aside>
        )}
      </main>

      <footer className="status-bar">
        {loading && <span className="loading-dot" aria-label="loading" />}
        {viewMode === "spectrogram" && spectro.loading && <span className="loading-dot" aria-label="loading" />}
        {project.busy !== "idle" && <span className="loading-dot" aria-label="loading" />}
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
        {project.savedPath !== null && (
          <span className="status-item" title={project.savedPath}>
            <span className="label">project</span>
            <span className="value">saved</span>
          </span>
        )}
        {activeError !== null && (
          <span className="status-error" role="alert">
            {errorText(activeError)}
          </span>
        )}
      </footer>
    </div>
  );
}
