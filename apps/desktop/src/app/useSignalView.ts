/**
 * Signal/session state + view state + the envelope request loop.
 *
 * Request discipline (docs/ipc-contract.md):
 * - view/mode/meta changes bump a state version; requests are coalesced to at
 *   most one issue per animation frame;
 * - each request carries a monotonically increasing request_id (never reset,
 *   so ids are also unique across signals);
 * - responses whose echoed id is not the current one are dropped on arrival;
 * - at most one request beyond the one already in flight is ever issued, and
 *   it is always for the latest state — intermediate states are never queued.
 */

import { type Dispatch, type SetStateAction, useCallback, useEffect, useRef, useState } from "react";

import { type SignalMeta, getWaveformEnvelope, openSignal, setSampleRate } from "../bridge/api";
import { type DecodedEnvelope, WaveformMode } from "../bridge/protocol";
import { type SelectionRange, type ViewRange, viewXToSample } from "../render/WaveformRenderer";

/** Contract bounds for `bins`. */
const MIN_BINS = 1;
const MAX_BINS = 65536;
/** Contract zoom floor: a view never spans fewer samples than this. */
export const MIN_SPAN = 16;

export interface ErrorState {
  code: string;
  message: string;
}

/** Envelope plus the view it was requested for (degenerate-path threshold). */
export interface EnvelopeSnapshot {
  env: DecodedEnvelope;
  view: ViewRange;
}

export interface UseSignalViewOptions {
  /** Current bin budget, e.g. `() => Math.max(1, Math.floor(cssWidth))`. */
  getBins: () => number;
}

function toErrorState(e: unknown): ErrorState {
  if (e instanceof Error && "code" in e && typeof (e as { code: unknown }).code === "string") {
    return { code: (e as { code: string }).code, message: e.message };
  }
  return { code: "internal", message: e instanceof Error ? e.message : String(e) };
}

/** Clamp a fractional view into `[0, total]` with integer bounds and span. */
export function clampView(v: ViewRange, total: number): ViewRange {
  if (total <= 0) return { start: 0, end: 0 };
  let span = v.end - v.start;
  if (total <= MIN_SPAN) {
    span = total;
  } else {
    span = Math.min(Math.max(span, MIN_SPAN), total);
    span = Math.max(MIN_SPAN, Math.round(span));
  }
  let start = Math.round(v.start);
  start = Math.min(Math.max(start, 0), total - span);
  return { start, end: start + span };
}

function clampSample(s: number, total: number): number {
  return Math.min(Math.max(Math.round(s), 0), total);
}

export function useSignalView(options: UseSignalViewOptions) {
  const [meta, setMeta] = useState<SignalMeta | null>(null);
  const [mode, setModeState] = useState<WaveformMode>(WaveformMode.Iq);
  const [view, setViewState] = useState<ViewRange>({ start: 0, end: 0 });
  const [selection, setSelectionState] = useState<SelectionRange | null>(null);
  const [cursor, setCursorState] = useState<number | null>(null);
  const [snapshot, setSnapshot] = useState<EnvelopeSnapshot | null>(null);
  const [error, setError] = useState<ErrorState | null>(null);
  const [loading, setLoading] = useState(false);

  // Mirrors for the (rAF-driven, non-React) request loop.
  const metaRef = useRef<SignalMeta | null>(null);
  const modeRef = useRef<WaveformMode>(WaveformMode.Iq);
  const viewRef = useRef<ViewRange>({ start: 0, end: 0 });
  const getBinsRef = useRef(options.getBins);

  const versionRef = useRef(0); // bumped on every request-relevant change
  const issuedRef = useRef(-1); // version the newest issued request was for
  const requestIdRef = useRef(0); // last request_id issued (monotonic, per app)
  const issuedViewRef = useRef<ViewRange>({ start: 0, end: 0 });
  const outstandingRef = useRef(0);
  const rafIdRef = useRef<number | null>(null);
  const disposedRef = useRef(false);

  useEffect(() => {
    getBinsRef.current = options.getBins;
  }, [options.getBins]);

  const schedulePump = useCallback(() => {
    if (disposedRef.current || rafIdRef.current !== null) return;
    rafIdRef.current = requestAnimationFrame(() => {
      rafIdRef.current = null;
      pump();
    });
  }, []);

  const pump = useCallback(() => {
    if (disposedRef.current) return;
    const m = metaRef.current;
    if (m === null) return;
    if (outstandingRef.current >= 2) return; // one in flight + one beyond, max
    if (issuedRef.current === versionRef.current) return; // nothing new

    const v = viewRef.current;
    const total = m.sample_count;
    if (!(v.start < v.end && v.end <= total)) return;
    const rawBins = Math.floor(getBinsRef.current());
    if (!Number.isFinite(rawBins) || rawBins < MIN_BINS) return;
    const bins = Math.min(MAX_BINS, rawBins);

    const requestId = ++requestIdRef.current;
    issuedRef.current = versionRef.current;
    issuedViewRef.current = { start: v.start, end: v.end };
    outstandingRef.current += 1;
    setLoading(true);

    getWaveformEnvelope(m.signal_id, v.start, v.end, bins, modeRef.current, requestId)
      .then((env) => {
        if (disposedRef.current) return;
        if (env.requestId !== requestIdRef.current) return; // stale, dropped
        setSnapshot({ env, view: issuedViewRef.current });
        setError(null);
      })
      .catch((e: unknown) => {
        if (disposedRef.current) return;
        if (requestId !== requestIdRef.current) return; // stale failure, dropped
        setError(toErrorState(e));
      })
      .finally(() => {
        outstandingRef.current -= 1;
        if (!disposedRef.current) {
          setLoading(outstandingRef.current > 0);
          if (versionRef.current > issuedRef.current) schedulePump();
        }
      });
  }, [schedulePump]);

  // Mirror state into refs and mark the request loop dirty.
  useEffect(() => {
    metaRef.current = meta;
    modeRef.current = mode;
    viewRef.current = view;
    versionRef.current += 1;
    schedulePump();
  }, [meta, mode, view, schedulePump]);

  useEffect(() => {
    // (Re)arm on every setup so a StrictMode-style remount keeps working.
    disposedRef.current = false;
    return () => {
      disposedRef.current = true;
      if (rafIdRef.current !== null) cancelAnimationFrame(rafIdRef.current);
    };
  }, []);

  // --- actions -----------------------------------------------------------------

  /** Open one signal (first picked path); resets view/envelope state. */
  const openPath = useCallback(async (path: string): Promise<void> => {
    setError(null);
    try {
      const m = await openSignal(path, null);
      setMeta(m);
      setViewState(clampView({ start: 0, end: m.sample_count }, m.sample_count));
      setSelectionState(null);
      setCursorState(null);
      setSnapshot(null);
    } catch (e) {
      setError(toErrorState(e));
    }
  }, []);

  /**
   * Make an already-open repository signal the active one (signal-list
   * switch; freshly registered `rec-<n>`/`gen-<n>` results). Same reset as
   * {@link openPath}, minus the open call — the backend already knows it.
   *
   * `restore` (project load) instead applies a saved view+mode — clamped
   * against the *new* signal's extent, which a follow-up setView could not
   * do: the mirror refs only sync in an effect, so the same-tick clamp would
   * use the previous signal's sample count.
   */
  const activateSignal = useCallback(
    (m: SignalMeta, restore?: { view: ViewRange; mode: WaveformMode }): void => {
      setError(null);
      setMeta(m);
      setViewState(clampView(restore === undefined ? { start: 0, end: m.sample_count } : restore.view, m.sample_count));
      if (restore !== undefined) setModeState(restore.mode);
      setSelectionState(null);
      setCursorState(null);
      setSnapshot(null);
    },
    [],
  );

  const setMode = useCallback((next: WaveformMode): void => {
    setModeState(next);
  }, []);

  /** Commit a new sample rate; refreshes meta (and re-requests with a fresh id). */
  const applySampleRate = useCallback(async (rate: number): Promise<void> => {
    const m = metaRef.current;
    if (m === null) return;
    try {
      const updated = await setSampleRate(m.signal_id, rate);
      setMeta(updated);
      setError(null);
    } catch (e) {
      setError(toErrorState(e));
    }
  }, []);

  /**
   * Zoom about the sample under the cursor. `factor` > 1 zooms in. Uses the
   * shared pure transform against the previous view, so consecutive wheel
   * events compose correctly even when batched.
   */
  const zoomAt = useCallback((cssW: number, xCss: number, factor: number): void => {
    const total = metaRef.current?.sample_count ?? 0;
    setViewState((prev) => {
      const span = prev.end - prev.start;
      const anchor = viewXToSample(prev, cssW, xCss);
      const frac = span > 0 ? (anchor - prev.start) / span : 0;
      let newSpan = span * factor;
      if (total <= MIN_SPAN) newSpan = total;
      else newSpan = Math.min(Math.max(newSpan, MIN_SPAN), total);
      return clampView({ start: anchor - frac * newSpan, end: anchor - frac * newSpan + newSpan }, total);
    });
  }, []);

  /** Absolute view set (pan target computed by the caller), clamped. */
  const setView: Dispatch<SetStateAction<ViewRange>> = useCallback((action) => {
    const total = metaRef.current?.sample_count ?? 0;
    setViewState((prev) => {
      const next = typeof action === "function" ? action(prev) : action;
      return clampView(next, total);
    });
  }, []);

  const resetView = useCallback((): void => {
    const total = metaRef.current?.sample_count ?? 0;
    setViewState(clampView({ start: 0, end: total }, total));
  }, []);

  const setSelection = useCallback((sel: SelectionRange | null): void => {
    const total = metaRef.current?.sample_count ?? 0;
    setSelectionState(
      sel === null
        ? null
        : { selStart: clampSample(sel.selStart, total), selEnd: clampSample(sel.selEnd, total) },
    );
  }, []);

  const clearSelection = useCallback((): void => {
    setSelectionState(null);
  }, []);

  /**
   * Cursor position in samples. Kept fractional so the cursor line tracks
   * the pointer exactly; the status bar floors it for display.
   */
  const setCursor = useCallback((sample: number | null): void => {
    const total = metaRef.current?.sample_count ?? 0;
    setCursorState(sample === null ? null : Math.min(Math.max(sample, 0), total));
  }, []);

  /** Force a re-request with a fresh id (e.g. the bin budget changed). */
  const refresh = useCallback((): void => {
    versionRef.current += 1;
    schedulePump();
  }, []);

  /** Surface a non-IPC failure (dialog, drag-drop, ...) in the status bar. */
  const reportError = useCallback((e: unknown): void => {
    setError(toErrorState(e));
  }, []);

  return {
    // state
    meta,
    mode,
    view,
    selection,
    cursor,
    snapshot,
    error,
    loading,
    // actions
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
  };
}

export type SignalViewApi = ReturnType<typeof useSignalView>;
