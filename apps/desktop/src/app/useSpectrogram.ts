/**
 * Spectrogram view state + the tile request loop (P1-remainder).
 *
 * Mirrors useSignalView's envelope loop exactly (docs/ipc-contract.md,
 * request discipline): view/window/meta changes bump a state version;
 * requests are coalesced to at most one issue per animation frame; each
 * request carries a monotonically increasing `request_id` (private to this
 * stream — never shared with the envelope counter, so the two loops cannot
 * drop each other's responses); responses whose echoed id is not the current
 * one are dropped on arrival; at most one request beyond the one already in
 * flight is ever issued, always for the latest state.
 *
 * There is no pixel budget to communicate: the backend derives the time-bin
 * count from the requested range and the window size, so a tile is fully
 * described by `(signal, view, windowSize)`.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import { type SignalMeta, getSpectrogramTile } from "../bridge/api";
import { type DecodedSpectrogramTile, type SpectrogramWindow } from "../bridge/protocol";
import type { ViewRange } from "../render/WaveformRenderer";
import { type ErrorState, toErrorState } from "./errors";

/** Tile plus the view/window it was requested for. */
export interface SpectrogramSnapshot {
  tile: DecodedSpectrogramTile;
  view: ViewRange;
  windowSize: SpectrogramWindow;
}

export interface UseSpectrogramOptions {
  /**
   * Whether the spectrogram is the visible view mode. When false the loop
   * issues nothing (no background tile fetches while the waveform shows);
   * flipping it back on re-requests immediately.
   */
  enabled: boolean;
}

export function useSpectrogram(meta: SignalMeta | null, view: ViewRange, options: UseSpectrogramOptions) {
  const [windowSize, setWindowSizeState] = useState<SpectrogramWindow>(1024);
  const [snapshot, setSnapshot] = useState<SpectrogramSnapshot | null>(null);
  const [error, setError] = useState<ErrorState | null>(null);
  const [loading, setLoading] = useState(false);

  // Mirrors for the (rAF-driven, non-React) request loop.
  const metaRef = useRef<SignalMeta | null>(null);
  const viewRef = useRef<ViewRange>({ start: 0, end: 0 });
  const windowRef = useRef<SpectrogramWindow>(1024);
  const enabledRef = useRef(options.enabled);

  const versionRef = useRef(0); // bumped on every request-relevant change
  const issuedRef = useRef(-1); // version the newest issued request was for
  const requestIdRef = useRef(0); // last request_id issued (monotonic, per stream)
  const issuedViewRef = useRef<ViewRange>({ start: 0, end: 0 });
  const outstandingRef = useRef(0);
  const rafIdRef = useRef<number | null>(null);
  const disposedRef = useRef(false);

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
    if (m === null || !enabledRef.current) return;
    if (outstandingRef.current >= 2) return; // one in flight + one beyond, max
    if (issuedRef.current === versionRef.current) return; // nothing new

    const v = viewRef.current;
    const total = m.sample_count;
    if (!(v.start < v.end && v.end <= total)) return;

    const win = windowRef.current;
    const requestId = ++requestIdRef.current;
    issuedRef.current = versionRef.current;
    issuedViewRef.current = { start: v.start, end: v.end };
    outstandingRef.current += 1;
    setLoading(true);

    getSpectrogramTile(m.signal_id, v.start, v.end, win, requestId)
      .then((tile) => {
        if (disposedRef.current) return;
        if (tile.requestId !== requestIdRef.current) return; // stale, dropped
        setSnapshot({ tile, view: issuedViewRef.current, windowSize: win });
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
    viewRef.current = view;
    windowRef.current = windowSize;
    enabledRef.current = options.enabled;
    if (meta === null) setSnapshot(null); // signal closed -> no tile to show
    versionRef.current += 1;
    schedulePump();
  }, [meta, view, windowSize, options.enabled, schedulePump]);

  useEffect(() => {
    // (Re)arm on every setup so a StrictMode-style remount keeps working.
    disposedRef.current = false;
    return () => {
      disposedRef.current = true;
      if (rafIdRef.current !== null) cancelAnimationFrame(rafIdRef.current);
    };
  }, []);

  // --- actions -----------------------------------------------------------------

  /** Choose the STFT window (256/1024/4096); re-requests with a fresh id. */
  const setWindowSize = useCallback((next: SpectrogramWindow): void => {
    setWindowSizeState(next);
  }, []);

  /** Force a re-request with a fresh id (e.g. after a context restore). */
  const refresh = useCallback((): void => {
    versionRef.current += 1;
    schedulePump();
  }, []);

  return { windowSize, setWindowSize, snapshot, error, loading, refresh };
}

export type SpectrogramApi = ReturnType<typeof useSpectrogram>;
