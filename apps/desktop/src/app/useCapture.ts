/**
 * Capture view model (P3): device list, parameter fields, the capture state
 * machine (idle → starting → running → stopping) and the live preview models.
 *
 * IPC-thin like useAnalysis: user input is validated by the exported pure
 * {@link parseCaptureFields} (unit-tested), backend responses are parsed in
 * the bridge (also tested); this hook only sequences them and guards against
 * stale events — batches/stats are accepted only while a capture this hook
 * started is active, and settle-after-stop is dropped.
 *
 * The live models (BatchRing, WaterfallColumns, gap counter) live in refs,
 * not React state: batches arrive far faster than the display refreshes, so
 * they are mutated on arrival and a `liveVersion` counter — bumped at most
 * once per animation frame, mirroring useSignalView's request coalescing —
 * tells the live view when to redraw. Columns computed but not yet painted
 * queue up bounded (`MAX_PENDING_COLUMNS`) so a hidden window cannot grow
 * memory without limit (rAF pauses while hidden; the queue drains on resume).
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type CaptureBatch,
  type CaptureConfig,
  type CaptureStats,
  type DeviceInfo,
  captureStats,
  listDevices,
  onCaptureBatch,
  onCaptureStats,
  startCapture,
  stopCapture,
} from "../bridge/api";
import { BatchRing, WaterfallColumns, seqGap, waveformColumns } from "./captureModel";
import { type ErrorState, toErrorState } from "./errors";

/** The one device whose config needs host/port (contract `CaptureConfig`). */
export const RTL_TCP_DEVICE = "rtl-tcp";

/** Editable capture fields as text; numbers are parsed on start. */
export interface CaptureParamFields {
  device: string;
  frequency: string;
  sampleRate: string;
  bandwidth: string;
  gain: string;
  /** rtl-tcp only. */
  host: string;
  /** rtl-tcp only. */
  port: string;
  record: boolean;
}

export const DEFAULT_CAPTURE_FIELDS: CaptureParamFields = {
  device: "",
  frequency: "433.92e6",
  sampleRate: "1e6",
  bandwidth: "1e6",
  gain: "20",
  host: "127.0.0.1",
  port: "1234",
  record: true,
};

/** Text fields of {@link CaptureParamFields} (`record` has its own setter). */
export type CaptureTextField = Exclude<keyof CaptureParamFields, "record">;

export type ParseCaptureResult =
  | { ok: true; config: CaptureConfig }
  | { ok: false; field: keyof CaptureParamFields; message: string };

/** Strip digit separators the rate input also tolerates. */
function parseNumberText(text: string): number {
  return Number.parseFloat(text.replace(/[,\s_]/g, ""));
}

/**
 * Validate the capture fields into a {@link CaptureConfig}. The device must
 * be one of `devices` *and* available (its `detail` becomes the error
 * message); `sample_rate` must be positive, frequency/bandwidth/gain finite
 * and non-negative; host (non-empty) and port (1–65535) are required for the
 * rtl-tcp device only.
 */
export function parseCaptureFields(
  fields: CaptureParamFields,
  devices: readonly DeviceInfo[],
): ParseCaptureResult {
  const device = fields.device.trim();
  if (device === "") {
    return { ok: false, field: "device", message: "no device selected" };
  }
  const info = devices.find((d) => d.name === device);
  if (info === undefined) {
    return { ok: false, field: "device", message: `unknown device "${device}"` };
  }
  if (!info.available) {
    return {
      ok: false,
      field: "device",
      message: info.detail === "" ? `device "${device}" is unavailable` : info.detail,
    };
  }

  const sampleRate = parseNumberText(fields.sampleRate);
  if (!Number.isFinite(sampleRate) || sampleRate <= 0) {
    return { ok: false, field: "sampleRate", message: `"${fields.sampleRate}" is not a positive sample rate` };
  }
  for (const [field, label] of [
    ["frequency", "frequency"],
    ["bandwidth", "bandwidth"],
    ["gain", "gain"],
  ] as const) {
    const v = parseNumberText(fields[field]);
    if (!Number.isFinite(v) || v < 0) {
      return { ok: false, field, message: `"${fields[field]}" is not a non-negative ${label}` };
    }
  }

  let host: string | null = null;
  let port: number | null = null;
  if (device === RTL_TCP_DEVICE) {
    host = fields.host.trim();
    if (host === "") {
      return { ok: false, field: "host", message: "host is required for rtl-tcp" };
    }
    const p = parseNumberText(fields.port);
    if (!Number.isFinite(p) || !Number.isInteger(p) || p < 1 || p > 65535) {
      return { ok: false, field: "port", message: `"${fields.port}" is not a TCP port (1–65535)` };
    }
    port = p;
  }

  return {
    ok: true,
    config: { device, sampleRate, frequency: parseNumberText(fields.frequency), bandwidth: parseNumberText(fields.bandwidth), gain: parseNumberText(fields.gain), record: fields.record, host, port },
  };
}

/** The capture whose events/commands belong to the current session. */
export interface ActiveCapture {
  captureId: string;
  /** `Some("rec-<n>")` iff recording. */
  signalId: string | null;
  device: string;
  sampleRate: number;
  frequency: number;
}

export type CaptureStatus = "idle" | "starting" | "running" | "stopping";

/** Result of a stop: final stats when it succeeded, else the error. */
export interface CaptureStopped {
  stats: CaptureStats | null;
  signalId: string | null;
  error: ErrorState | null;
}

export interface UseCaptureOptions {
  /** Called once a stop settled (App refreshes the signal list here). */
  onStopped: (result: CaptureStopped) => void;
}

/** Waterfall columns computed but not yet painted (bounded; see header). */
const MAX_PENDING_COLUMNS = 512;

export function useCapture(options: UseCaptureOptions) {
  const [fields, setFields] = useState<CaptureParamFields>(DEFAULT_CAPTURE_FIELDS);
  const [devices, setDevices] = useState<DeviceInfo[]>([]);
  const [status, setStatus] = useState<CaptureStatus>("idle");
  const [active, setActive] = useState<ActiveCapture | null>(null);
  const [stats, setStats] = useState<CaptureStats | null>(null);
  const [error, setError] = useState<ErrorState | null>(null);
  const [paramError, setParamError] = useState<string | null>(null);
  const [liveVersion, setLiveVersion] = useState(0);

  // Mirrors for the async/evented paths (always latest values).
  const fieldsRef = useRef(fields);
  const devicesRef = useRef(devices);
  const statusRef = useRef<CaptureStatus>("idle");
  const activeRef = useRef<ActiveCapture | null>(null);
  const onStoppedRef = useRef(options.onStopped);
  /** True between subscribe and stop-settle: the events we accept. */
  const acceptingRef = useRef(false);

  // Live preview models (mutated per batch, rendered per liveVersion).
  const ringRef = useRef(new BatchRing());
  const waterfallRef = useRef(new WaterfallColumns());
  const pendingRef = useRef<Uint8Array[]>([]);
  const droppedRef = useRef(0);
  const lastSeqRef = useRef<number | null>(null);
  const unlistenRef = useRef<{ batch: () => void; stats: () => void } | null>(null);
  const rafIdRef = useRef<number | null>(null);
  const liveVersionRef = useRef(0);

  useEffect(() => {
    fieldsRef.current = fields;
    devicesRef.current = devices;
    onStoppedRef.current = options.onStopped;
  }, [fields, devices, options.onStopped]);

  // Display-only derived value, recomputed on each render (liveVersion and
  // stats drive the re-renders). The backend counter is authoritative; the
  // client-side seq-gap count is its twin — take the max.
  const droppedPreview = Math.max(stats?.droppedPreviewBatches ?? 0, droppedRef.current);

  const scheduleLive = useCallback(() => {
    if (rafIdRef.current !== null) return;
    rafIdRef.current = requestAnimationFrame(() => {
      rafIdRef.current = null;
      liveVersionRef.current += 1;
      setLiveVersion(liveVersionRef.current);
    });
  }, []);

  const handleError = useCallback((e: unknown): void => {
    const next = toErrorState(e);
    // A recurring decode failure must not re-render per batch.
    setError((prev) =>
      prev !== null && prev.code === next.code && prev.message === next.message ? prev : next,
    );
  }, []);

  const handleBatch = useCallback(
    (batch: CaptureBatch): void => {
      if (!acceptingRef.current) return;
      const activeNow = activeRef.current;
      if (activeNow !== null && batch.captureId !== activeNow.captureId) return;
      const gap = seqGap(lastSeqRef.current, batch.seq);
      if (lastSeqRef.current === null || batch.seq > lastSeqRef.current) {
        lastSeqRef.current = batch.seq;
      }
      if (gap > 0) droppedRef.current += gap;
      ringRef.current.push({ seq: batch.seq, iq: batch.iq });
      pendingRef.current.push(waterfallRef.current.push(batch.iq));
      if (pendingRef.current.length > MAX_PENDING_COLUMNS) {
        pendingRef.current.splice(0, pendingRef.current.length - MAX_PENDING_COLUMNS);
      }
      scheduleLive();
    },
    [scheduleLive],
  );

  const handleStats = useCallback((ev: { captureId: string; stats: CaptureStats }): void => {
    if (!acceptingRef.current) return;
    const activeNow = activeRef.current;
    if (activeNow !== null && ev.captureId !== activeNow.captureId) return;
    setStats(ev.stats);
  }, []);

  const teardownListeners = useCallback((): void => {
    const un = unlistenRef.current;
    unlistenRef.current = null;
    acceptingRef.current = false;
    un?.batch();
    un?.stats();
  }, []);

  const resetLiveModels = useCallback((): void => {
    ringRef.current.clear();
    waterfallRef.current.reset();
    pendingRef.current = [];
    droppedRef.current = 0;
    lastSeqRef.current = null;
    liveVersionRef.current = 0;
    setLiveVersion(0);
  }, []);

  // --- actions -----------------------------------------------------------------

  /** (Re)enumerate devices; keeps the selection valid, defaults to available. */
  const refreshDevices = useCallback(async (): Promise<void> => {
    let list: DeviceInfo[];
    try {
      list = await listDevices();
    } catch (e) {
      setError(toErrorState(e));
      return;
    }
    setDevices(list);
    setFields((prev) => {
      if (prev.device !== "" && list.some((d) => d.name === prev.device)) return prev;
      const first = list.find((d) => d.available);
      return first === undefined ? { ...prev, device: "" } : { ...prev, device: first.name };
    });
  }, []);

  useEffect(() => {
    void refreshDevices();
  }, [refreshDevices]);

  /** Update one text field (device select included); clears the last error. */
  const setField = useCallback((field: CaptureTextField, value: string): void => {
    setFields((prev) => ({ ...prev, [field]: value }));
    setParamError(null);
  }, []);

  const setRecord = useCallback((record: boolean): void => {
    setFields((prev) => ({ ...prev, record }));
  }, []);

  /** Start a capture; invalid fields surface as `paramError`, never reach IPC. */
  const start = useCallback(async (): Promise<void> => {
    if (statusRef.current !== "idle") return;
    const parsed = parseCaptureFields(fieldsRef.current, devicesRef.current);
    if (!parsed.ok) {
      setParamError(`${parsed.field}: ${parsed.message}`);
      return;
    }
    setParamError(null);
    setError(null);
    setStats(null);
    resetLiveModels();
    statusRef.current = "starting";
    setStatus("starting");

    // Subscribe before starting so the first batches are not missed; the UI
    // runs one capture at a time, so pre-handle batches are ours.
    try {
      const [unBatch, unStats] = await Promise.all([
        onCaptureBatch(handleBatch, handleError),
        onCaptureStats(handleStats, handleError),
      ]);
      unlistenRef.current = { batch: unBatch, stats: unStats };
      acceptingRef.current = true;
    } catch (e) {
      statusRef.current = "idle";
      setStatus("idle");
      setError(toErrorState(e));
      return;
    }

    try {
      const handle = await startCapture(parsed.config);
      activeRef.current = {
        captureId: handle.captureId,
        signalId: handle.signalId,
        device: parsed.config.device,
        sampleRate: parsed.config.sampleRate,
        frequency: parsed.config.frequency,
      };
      setActive(activeRef.current);
      statusRef.current = "running";
      setStatus("running");
      // Seed the stats line immediately (stats events are only ~1 Hz).
      captureStats(handle.captureId).then(
        (s) => {
          if (activeRef.current?.captureId === handle.captureId) setStats(s);
        },
        () => undefined, // the 1 Hz events cover this; a failed poll is noise
      );
    } catch (e) {
      teardownListeners();
      resetLiveModels();
      statusRef.current = "idle";
      setStatus("idle");
      setError(toErrorState(e));
    }
  }, [handleBatch, handleStats, handleError, resetLiveModels, teardownListeners]);

  /** Stop the running capture; `onStopped` fires once it settled. */
  const stop = useCallback(async (): Promise<void> => {
    const activeNow = activeRef.current;
    if (activeNow === null || statusRef.current === "stopping") return;
    statusRef.current = "stopping";
    setStatus("stopping");

    let finalStats: CaptureStats | null = null;
    let err: ErrorState | null = null;
    try {
      finalStats = await stopCapture(activeNow.captureId);
    } catch (e) {
      err = toErrorState(e);
    }
    teardownListeners();
    activeRef.current = null;
    setActive(null);
    statusRef.current = "idle";
    setStatus("idle");
    if (finalStats !== null) setStats(finalStats);
    if (err !== null) setError(err);
    onStoppedRef.current({ stats: finalStats, signalId: activeNow.signalId, error: err });
  }, [teardownListeners]);

  // Unmount mid-capture: detach and stop best-effort (the backend stops the
  // device; its rejection has nobody left to surface to).
  useEffect(() => {
    return () => {
      if (rafIdRef.current !== null) cancelAnimationFrame(rafIdRef.current);
      const activeNow = activeRef.current;
      teardownListeners();
      if (activeNow !== null) void stopCapture(activeNow.captureId).catch(() => undefined);
    };
  }, [teardownListeners]);

  return {
    // state
    fields,
    devices,
    status,
    active,
    stats,
    error,
    paramError,
    /** Bumped (≤ once per animation frame) per arrived preview batch. */
    liveVersion,
    /** Preview batches dropped so far (backend counter + client seq gaps). */
    droppedPreview,
    // live-preview accessors (render-side; refs underneath)
    /** Frequency bins per waterfall column (DC first). */
    waterfallFreqBins: waterfallRef.current.freqBins,
    /** Min/max per column over the rolling window ([iMin,iMax,qMin,qMax]×n). */
    getWaveformColumns(columns: number): Float32Array {
      return waveformColumns(ringRef.current.batches(), columns);
    },
    /** Take (and clear) the waterfall columns not yet painted. */
    drainWaterfallColumns(): Uint8Array[] {
      const out = pendingRef.current;
      pendingRef.current = [];
      return out;
    },
    // actions
    refreshDevices,
    setField,
    setRecord,
    start,
    stop,
  };
}

export type CaptureApi = ReturnType<typeof useCapture>;
