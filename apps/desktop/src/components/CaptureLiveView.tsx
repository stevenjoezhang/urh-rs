/**
 * Live capture view (P3): the center-area content while a capture runs.
 *
 * A slim status strip (device, recording state, dropped-batches warning, the
 * stats line), the live waveform (min/max per column over the rolling batch
 * window) above, and the scrolling FFT waterfall below — "live waveform above
 * the waterfall", per the P3 layout. The component is thin DOM/canvas glue:
 * all math (ring buffer, seq gaps, columns, FFT/dB mapping) is the pure
 * captureModel, unit-tested elsewhere.
 *
 * Drawing discipline: the waveform is *recomputed and redrawn* whenever
 * `liveVersion` bumps (≤ once per animation frame); waterfall columns are
 * computed once per batch by the hook and *drained* here exactly once — the
 * drain clears the queue, so a double-invoked effect drains nothing the
 * second time and appends stay idempotent.
 */

import { useEffect, useRef } from "react";

import type { CaptureApi } from "../app/useCapture";
import { errorText } from "../app/errors";
import { formatCount, formatDuration, formatHz } from "../app/format";
import { LiveWaterfallRenderer } from "../render/LiveWaterfallRenderer";
import { LiveWaveformRenderer } from "../render/LiveWaveformRenderer";

interface CaptureLiveViewProps {
  capture: CaptureApi;
}

export function CaptureLiveView(props: CaptureLiveViewProps): JSX.Element {
  const { capture } = props;
  const waveWrapRef = useRef<HTMLDivElement | null>(null);
  const fallWrapRef = useRef<HTMLDivElement | null>(null);
  const waveCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const fallCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const waveRendererRef = useRef<LiveWaveformRenderer | null>(null);
  const fallRendererRef = useRef<LiveWaterfallRenderer | null>(null);
  const waveWidthRef = useRef(0);
  // Latest-ref so the liveVersion-keyed effect always reads the current api.
  const captureRef = useRef(capture);
  captureRef.current = capture;

  // Renderer lifecycle + resize (the wave1 canvas pattern).
  useEffect(() => {
    const waveCanvas = waveCanvasRef.current;
    const fallCanvas = fallCanvasRef.current;
    const waveWrap = waveWrapRef.current;
    const fallWrap = fallWrapRef.current;
    if (waveCanvas === null || fallCanvas === null || waveWrap === null || fallWrap === null) return;
    const wave = new LiveWaveformRenderer(waveCanvas);
    const fall = new LiveWaterfallRenderer(fallCanvas);
    waveRendererRef.current = wave;
    fallRendererRef.current = fall;

    const dpr = () => window.devicePixelRatio || 1;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        if (entry.target === waveWrap) {
          waveWidthRef.current = width;
          wave.resize(width, height, dpr());
        } else {
          fall.resize(width, height, dpr());
        }
      }
    });
    ro.observe(waveWrap);
    ro.observe(fallWrap);
    return () => {
      ro.disconnect();
      waveRendererRef.current = null;
      fallRendererRef.current = null;
    };
  }, []);

  // Redraw the waveform + drain new waterfall columns, per arrived batch.
  useEffect(() => {
    const wave = waveRendererRef.current;
    const fall = fallRendererRef.current;
    if (wave === null || fall === null) return;
    const api = captureRef.current;
    wave.draw(api.getWaveformColumns(Math.max(1, Math.floor(waveWidthRef.current))));
    for (const column of api.drainWaterfallColumns()) fall.append(column);
  }, [capture.liveVersion]);

  // A new capture session starts a fresh waterfall history.
  const captureId = capture.active?.captureId ?? null;
  useEffect(() => {
    fallRendererRef.current?.clear();
  }, [captureId]);

  const active = capture.active;
  const stats = capture.stats;
  const recording = active?.signalId != null;
  const duration =
    active !== null && stats !== null && active.sampleRate > 0
      ? formatDuration(stats.recordedSamples / active.sampleRate)
      : null;

  return (
    <div className="live-view" aria-label="Live capture preview">
      <div className="live-status">
        <span className={`live-dot${recording ? " rec" : ""}`} aria-hidden="true" />
        <span className="live-device">
          {active === null ? "capture…" : `${active.device} @ ${formatHz(active.frequency)}`}
        </span>
        {stats !== null && (
          <span className="live-stats">
            recorded {formatCount(stats.recordedSamples)} samples{duration === null ? "" : ` (${duration})`}
          </span>
        )}
        <span className="spacer" />
        {capture.droppedPreview > 0 && (
          <span className="drop-warn" role="status">
            dropped {formatCount(capture.droppedPreview)} preview batches
          </span>
        )}
      </div>
      <div className="live-wave-wrap" ref={waveWrapRef}>
        <canvas ref={waveCanvasRef} className="live-canvas" />
      </div>
      <div className="live-fall-wrap" ref={fallWrapRef}>
        <canvas ref={fallCanvasRef} className="live-canvas" />
      </div>
      {capture.liveVersion === 0 && (
        <div className="overlay-msg">
          <span className="hint">waiting for preview batches…</span>
        </div>
      )}
      {capture.error !== null && (
        <div className="error-banner live-error" role="alert">
          {errorText(capture.error)}
        </div>
      )}
    </div>
  );
}
