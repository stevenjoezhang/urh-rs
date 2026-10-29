/**
 * Typed wrappers around the Tauri commands defined in docs/ipc-contract.md.
 *
 * Two deliberate boundary decisions (both narrowable if the contract is
 * tightened):
 *
 * 1. Invoke-argument casing: the contract spells multi-word parameters in
 *    snake_case (`sample_rate`, `signal_id`, `request_id`), but Tauri v2 by
 *    default binds Rust snake_case parameters to camelCase JS keys unless
 *    the command opts into `rename_all = "snake_case"`. We send *both*
 *    spellings; Tauri ignores payload keys it does not bind, so this works
 *    either way without inventing a third shape.
 *
 * 2. `SignalMeta` keys: the contract's Rust struct is serde-derived with no
 *    rename, so snake_case keys are canonical; the camelCase spellings are
 *    accepted defensively by {@link parseMeta}.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";

import {
  type DecodedEnvelope,
  type DecodedSpectrogramTile,
  type SpectrogramWindow,
  WaveformMode,
  decodeCaptureBatchBody,
  decodeEnvelope,
  decodeSpectrogramTile,
  freqBinsForWindow,
} from "./protocol";

export interface SignalMeta {
  signal_id: string;
  path: string;
  display_name: string;
  format: string;
  sample_count: number;
  sample_rate: number;
  duration_secs: number;
}

/** The error shape every command rejects with (contract: `AppError`). */
export interface AppErrorShape {
  code: string;
  message: string;
}

/** Normalized rejection: either the backend's `AppError` or a wrapped unknown. */
export class IpcError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "IpcError";
    this.code = code;
  }
}

function toIpcError(e: unknown): IpcError {
  if (e instanceof IpcError) return e;
  if (typeof e === "object" && e !== null && "code" in e && "message" in e) {
    const code = (e as { code: unknown }).code;
    const message = (e as { message: unknown }).message;
    if (typeof code === "string" && typeof message === "string") {
      return new IpcError(code, message);
    }
  }
  return new IpcError("internal", e instanceof Error ? e.message : String(e));
}

/** Send both snake_case and camelCase spellings of every multi-word key. */
function invokeArgs(args: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(args)) {
    out[key] = value;
    const camel = key.replace(/_([a-z])/g, (_s, c: string) => c.toUpperCase());
    if (camel !== key) out[camel] = value;
  }
  return out;
}

async function invokeMeta(command: string, args: Record<string, unknown>): Promise<SignalMeta> {
  try {
    return parseMeta(await invoke<unknown>(command, invokeArgs(args)));
  } catch (e) {
    throw toIpcError(e);
  }
}

function readString(raw: object, snake: string, camel: string): string {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (typeof v !== "string") throw new IpcError("internal", `SignalMeta.${snake} is missing or not a string`);
  return v;
}

function readNumber(raw: object, snake: string, camel: string): number {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (typeof v !== "number" || !Number.isFinite(v)) {
    throw new IpcError("internal", `SignalMeta.${snake} is missing or not a finite number`);
  }
  return v;
}

/** Narrow an untyped IPC payload into {@link SignalMeta}. */
export function parseMeta(raw: unknown): SignalMeta {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "open_signal returned a non-object");
  }
  return {
    signal_id: readString(raw, "signal_id", "signalId"),
    path: readString(raw, "path", "path"),
    display_name: readString(raw, "display_name", "displayName"),
    format: readString(raw, "format", "format"),
    sample_count: readNumber(raw, "sample_count", "sampleCount"),
    sample_rate: readNumber(raw, "sample_rate", "sampleRate"),
    duration_secs: readNumber(raw, "duration_secs", "durationSecs"),
  };
}

export async function openSignal(path: string, sampleRate: number | null): Promise<SignalMeta> {
  return invokeMeta("open_signal", { path, sample_rate: sampleRate });
}

export async function closeSignal(signalId: string): Promise<void> {
  try {
    await invoke<unknown>("close_signal", invokeArgs({ signal_id: signalId }));
  } catch (e) {
    throw toIpcError(e);
  }
}

export async function listSignals(): Promise<SignalMeta[]> {
  try {
    const raw = await invoke<unknown>("list_signals");
    if (!Array.isArray(raw)) throw new IpcError("internal", "list_signals returned a non-array");
    return raw.map(parseMeta);
  } catch (e) {
    throw toIpcError(e);
  }
}

export async function setSampleRate(signalId: string, sampleRate: number): Promise<SignalMeta> {
  return invokeMeta("set_sample_rate", { signal_id: signalId, sample_rate: sampleRate });
}

/**
 * Fetch and decode one envelope frame. `start`/`end` are the half-open view
 * range, `bins` the pixel-column budget, `requestId` the staleness token.
 */
export async function getWaveformEnvelope(
  signalId: string,
  start: number,
  end: number,
  bins: number,
  mode: WaveformMode,
  requestId: number,
): Promise<DecodedEnvelope> {
  let frame: unknown;
  try {
    frame = await invoke<unknown>("get_waveform_envelope", invokeArgs({
      signal_id: signalId,
      start,
      end,
      bins,
      mode,
      request_id: requestId,
    }));
  } catch (e) {
    throw toIpcError(e);
  }
  if (!(frame instanceof ArrayBuffer) && !(frame instanceof Uint8Array)) {
    throw new IpcError(
      "internal",
      `get_waveform_envelope returned ${typeof frame}, expected raw frame bytes`,
    );
  }
  return decodeEnvelope(frame);
}

const IQ_EXTENSIONS = [
  "complex",
  "complex16s",
  "complex16u",
  "complex32s",
  "complex32u",
  "cs8",
  "cu8",
  "cs16",
  "cu16",
];

/** Native file picker; returns the chosen paths ([] if cancelled). */
export async function pickIQFiles(): Promise<string[]> {
  const picked = await open({
    multiple: true,
    filters: [{ name: "IQ", extensions: IQ_EXTENSIONS }],
  });
  if (picked === null) return [];
  return Array.isArray(picked) ? picked : [picked];
}

// --- v2: spectrogram tiles -----------------------------------------------------

/** Ordered half-open sample interval `[start, end)` (analysis/tile ranges). */
export interface SampleRange {
  start: number;
  end: number;
}

/**
 * Fetch and decode one spectrogram tile covering `[start, end)`. `freq_bins`
 * is not a parameter: the contract couples it to `window_size` (always
 * `window_size / 2`), so it is derived and sent as such. `requestId` is the
 * staleness token echoed in the frame header.
 */
export async function getSpectrogramTile(
  signalId: string,
  start: number,
  end: number,
  windowSize: SpectrogramWindow,
  requestId: number,
): Promise<DecodedSpectrogramTile> {
  let frame: unknown;
  try {
    frame = await invoke<unknown>("get_spectrogram_tile", invokeArgs({
      signal_id: signalId,
      start,
      end,
      freq_bins: freqBinsForWindow(windowSize),
      window_size: windowSize,
      request_id: requestId,
    }));
  } catch (e) {
    throw toIpcError(e);
  }
  if (!(frame instanceof ArrayBuffer) && !(frame instanceof Uint8Array)) {
    throw new IpcError(
      "internal",
      `get_spectrogram_tile returned ${typeof frame}, expected raw frame bytes`,
    );
  }
  return decodeSpectrogramTile(frame, windowSize);
}

// --- v2: protocol analysis -----------------------------------------------------

/**
 * Analysis parameters (contract v2). Every field is optional; `undefined`
 * means "auto" (the backend estimates it). `null` is never stored here —
 * the wire encoding maps `undefined` to an explicit JSON `null`.
 */
export interface AnalysisParams {
  /** One of {@link MODULATIONS}; `undefined` = auto-detect. */
  modulation?: string;
  /** Samples per bit; `undefined` = auto. */
  bitLength?: number;
  /** Decision threshold; `undefined` = auto. */
  center?: number;
  /** Noise threshold; `undefined` = auto. */
  noiseThreshold?: number;
  /** Bit tolerance in samples; `undefined` = auto. */
  tolerance?: number;
}

/** Modulations the backend accepts (contract v2). */
export const MODULATIONS = ["ASK", "FSK", "PSK", "GFSK", "OQPSK"] as const;

/** The effective parameters an analysis actually used (all fields resolved). */
export interface EstimatedParams {
  modulation: string;
  bitLength: number;
  center: number;
  noiseThreshold: number;
  tolerance: number;
}

/** One decoded message (contract v2; sample extents are absolute). */
export interface MessageInfo {
  index: number;
  bitCount: number;
  /** Pause AFTER the message, in samples. */
  pause: number;
  /** Undecoded bits, `"0101…"`. */
  plainBits: string;
  /** Absolute sample index of the first bit. */
  startSample: number;
  /** Exclusive end: last bit position + pause. */
  endSample: number;
}

export interface AnalysisResult {
  params: EstimatedParams;
  messages: MessageInfo[];
}

/** An AWRE field label over a message's bits (contract v2). */
export interface FieldLabel {
  messageIndex: number;
  name: string;
  /** Bit index within the message. */
  bitStart: number;
  /** Exclusive. */
  bitEnd: number;
  colorIndex: number;
}

/**
 * Encode {@link AnalysisParams} for the wire: snake_case keys (serde default
 * for the backend struct) plus the camelCase spellings, `null` for "auto" —
 * mirroring the top-level {@link invokeArgs} dual-casing decision.
 */
export function encodeAnalysisParams(params: AnalysisParams): Record<string, unknown> {
  return invokeArgs({
    modulation: params.modulation ?? null,
    bit_length: params.bitLength ?? null,
    center: params.center ?? null,
    noise_threshold: params.noiseThreshold ?? null,
    tolerance: params.tolerance ?? null,
  });
}

// Field readers shared by the v2 result parsers. Same defensive dual-key
// (snake_case canonical, camelCase accepted) style as parseMeta.

function fieldString(ctx: string, raw: object, snake: string, camel = snake): string {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (typeof v !== "string") throw new IpcError("internal", `${ctx}.${snake} is missing or not a string`);
  return v;
}

function fieldNumber(ctx: string, raw: object, snake: string, camel = snake): number {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (typeof v !== "number" || !Number.isFinite(v)) {
    throw new IpcError("internal", `${ctx}.${snake} is missing or not a finite number`);
  }
  return v;
}

/** Unsigned integer field (u32/u64/usize on the wire; exact up to 2^53). */
function fieldUint(ctx: string, raw: object, snake: string, camel = snake): number {
  const v = fieldNumber(ctx, raw, snake, camel);
  if (!Number.isInteger(v) || v < 0) {
    throw new IpcError("internal", `${ctx}.${snake} must be a non-negative integer, got ${v}`);
  }
  return v;
}

/** Narrow an untyped IPC payload into {@link EstimatedParams}. */
export function parseEstimatedParams(raw: unknown): EstimatedParams {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "AnalysisResult.params is not an object");
  }
  const modulation = fieldString("AnalysisResult.params", raw, "modulation");
  if (modulation.length === 0) {
    throw new IpcError("internal", "AnalysisResult.params.modulation is empty");
  }
  const bitLength = fieldUint("AnalysisResult.params", raw, "bit_length", "bitLength");
  if (bitLength === 0) {
    throw new IpcError("internal", "AnalysisResult.params.bit_length must be > 0");
  }
  return {
    modulation,
    bitLength,
    center: fieldNumber("AnalysisResult.params", raw, "center"),
    noiseThreshold: fieldNumber("AnalysisResult.params", raw, "noise_threshold", "noiseThreshold"),
    tolerance: fieldUint("AnalysisResult.params", raw, "tolerance"),
  };
}

/** Narrow an untyped IPC payload into one {@link MessageInfo}. */
export function parseMessageInfo(raw: unknown): MessageInfo {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "AnalysisResult.messages[] is not an object");
  }
  const index = fieldUint("MessageInfo", raw, "index");
  const bitCount = fieldUint("MessageInfo", raw, "bit_count", "bitCount");
  const pause = fieldUint("MessageInfo", raw, "pause");
  const plainBits = fieldString("MessageInfo", raw, "plain_bits", "plainBits");
  if (plainBits.length > 0 && !/^[01]+$/.test(plainBits)) {
    throw new IpcError("internal", "MessageInfo.plain_bits contains characters other than 0/1");
  }
  if (plainBits.length !== bitCount) {
    throw new IpcError(
      "internal",
      `MessageInfo.bit_count (${bitCount}) does not match plain_bits length (${plainBits.length})`,
    );
  }
  const startSample = fieldUint("MessageInfo", raw, "start_sample", "startSample");
  const endSample = fieldUint("MessageInfo", raw, "end_sample", "endSample");
  if (endSample < startSample) {
    throw new IpcError(
      "internal",
      `MessageInfo.end_sample (${endSample}) is before start_sample (${startSample})`,
    );
  }
  return { index, bitCount, pause, plainBits, startSample, endSample };
}

/** Narrow an untyped IPC payload into {@link AnalysisResult}. */
export function parseAnalysisResult(raw: unknown): AnalysisResult {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "analyze_signal returned a non-object");
  }
  const messagesRaw = (raw as Record<string, unknown>)["messages"];
  if (!Array.isArray(messagesRaw)) {
    throw new IpcError("internal", "AnalysisResult.messages is missing or not an array");
  }
  return {
    params: parseEstimatedParams((raw as Record<string, unknown>)["params"]),
    messages: messagesRaw.map(parseMessageInfo),
  };
}

/** Narrow an untyped IPC payload into one {@link FieldLabel}. */
export function parseFieldLabel(raw: unknown): FieldLabel {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "run_awre returned a non-object entry");
  }
  const bitStart = fieldUint("FieldLabel", raw, "bit_start", "bitStart");
  const bitEnd = fieldUint("FieldLabel", raw, "bit_end", "bitEnd");
  if (bitEnd < bitStart) {
    throw new IpcError("internal", `FieldLabel.bit_end (${bitEnd}) is before bit_start (${bitStart})`);
  }
  return {
    messageIndex: fieldUint("FieldLabel", raw, "message_index", "messageIndex"),
    name: fieldString("FieldLabel", raw, "name"),
    bitStart,
    bitEnd,
    colorIndex: fieldUint("FieldLabel", raw, "color_index", "colorIndex"),
  };
}

function parseArray(command: string, raw: unknown): unknown[] {
  if (!Array.isArray(raw)) {
    throw new IpcError("internal", `${command} returned a non-array`);
  }
  return raw;
}

/**
 * Run a protocol analysis. `range` restricts analysis to a half-open interval
 * (`null` = whole signal); `jobId` identifies the job for cancellation. The
 * promise settles when the job finishes — `cancel_analysis` makes it reject
 * with code `"cancelled"`.
 */
export async function analyzeSignal(
  signalId: string,
  range: SampleRange | null,
  params: AnalysisParams,
  jobId: number,
): Promise<AnalysisResult> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("analyze_signal", invokeArgs({
      signal_id: signalId,
      range_start: range === null ? null : range.start,
      range_end: range === null ? null : range.end,
      params: encodeAnalysisParams(params),
      job_id: jobId,
    }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseAnalysisResult(raw);
}

/** Ask the backend to cancel a running analysis job. Returns job-existed. */
export async function cancelAnalysis(jobId: number): Promise<boolean> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("cancel_analysis", invokeArgs({ job_id: jobId }));
  } catch (e) {
    throw toIpcError(e);
  }
  if (typeof raw !== "boolean") {
    throw new IpcError("internal", `cancel_analysis returned ${typeof raw}, expected a boolean`);
  }
  return raw;
}

/** Run AWRE field detection over the analysis messages of a signal/range. */
export async function runAwre(
  signalId: string,
  range: SampleRange | null,
  params: AnalysisParams,
): Promise<FieldLabel[]> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("run_awre", invokeArgs({
      signal_id: signalId,
      range_start: range === null ? null : range.start,
      range_end: range === null ? null : range.end,
      params: encodeAnalysisParams(params),
    }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseArray("run_awre", raw).map(parseFieldLabel);
}

// --- v2: device capture (P3) -----------------------------------------------------

/** One capture-capable device (contract v2 `DeviceInfo`). */
export interface DeviceInfo {
  /** Device name; the `device` key of a {@link CaptureConfig}. */
  name: string;
  /** Backend kind: "native-dlopen" | "cpal" | "network" | "mock" | … */
  backend: string;
  /** Driver lib loadable / device openable. */
  available: boolean;
  /** "" or the reason the device is unavailable. */
  detail: string;
}

/** Capture parameters (contract v2 `CaptureConfig`). */
export interface CaptureConfig {
  device: string;
  sampleRate: number;
  frequency: number;
  bandwidth: number;
  gain: number;
  /** Append samples into a repository signal (`rec-<n>`). */
  record: boolean;
  /** rtl-tcp only. */
  host: string | null;
  /** rtl-tcp only. */
  port: number | null;
  /** Native backends only: identifier (serial/index) of the specific
   * hardware to open; null opens the first device. */
  identifier: string | null;
}

/** A started capture (contract v2 `CaptureHandle`). */
export interface CaptureHandle {
  captureId: string;
  /** `Some("rec-<n>")` iff the capture records. */
  signalId: string | null;
}

/** Capture counters (contract v2 `CaptureStats`). */
export interface CaptureStats {
  droppedPreviewBatches: number;
  recordedSamples: number;
  /** Record queue overflowed — recorded data was lost, capture stopped. */
  overflowDetected: boolean;
}

/**
 * Encode a {@link CaptureConfig} for the wire: snake_case keys (serde default
 * for the backend struct) plus the camelCase spellings, Options as explicit
 * `null` — the same dual-casing decision as {@link encodeAnalysisParams}.
 */
export function encodeCaptureConfig(config: CaptureConfig): Record<string, unknown> {
  return invokeArgs({
    device: config.device,
    sample_rate: config.sampleRate,
    frequency: config.frequency,
    bandwidth: config.bandwidth,
    gain: config.gain,
    record: config.record,
    host: config.host ?? null,
    port: config.port ?? null,
    identifier: config.identifier ?? null,
  });
}

function fieldBool(ctx: string, raw: object, snake: string, camel = snake): boolean {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (typeof v !== "boolean") {
    throw new IpcError("internal", `${ctx}.${snake} is missing or not a boolean`);
  }
  return v;
}

/** Narrow an untyped IPC payload into one {@link DeviceInfo}. */
export function parseDeviceInfo(raw: unknown): DeviceInfo {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "list_devices returned a non-object entry");
  }
  const name = fieldString("DeviceInfo", raw, "name");
  if (name.length === 0) {
    throw new IpcError("internal", "DeviceInfo.name is empty");
  }
  return {
    name,
    backend: fieldString("DeviceInfo", raw, "backend"),
    available: fieldBool("DeviceInfo", raw, "available"),
    detail: fieldString("DeviceInfo", raw, "detail"),
  };
}

/** Narrow an untyped IPC payload into a {@link CaptureHandle}. */
export function parseCaptureHandle(raw: unknown): CaptureHandle {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "start_capture returned a non-object");
  }
  const signalId = (raw as Record<string, unknown>)["signal_id"] ?? (raw as Record<string, unknown>)["signalId"];
  if (signalId !== null && signalId !== undefined && typeof signalId !== "string") {
    throw new IpcError("internal", "CaptureHandle.signal_id is not a string or null");
  }
  return {
    captureId: fieldString("CaptureHandle", raw, "capture_id", "captureId"),
    signalId: signalId === undefined || signalId === null ? null : signalId,
  };
}

/** Narrow an untyped IPC payload into {@link CaptureStats}. */
export function parseCaptureStats(raw: unknown): CaptureStats {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "CaptureStats is not an object");
  }
  return {
    droppedPreviewBatches: fieldUint("CaptureStats", raw, "dropped_preview_batches", "droppedPreviewBatches"),
    recordedSamples: fieldUint("CaptureStats", raw, "recorded_samples", "recordedSamples"),
    overflowDetected: fieldBool("CaptureStats", raw, "overflow_detected", "overflowDetected"),
  };
}

/** Enumerate capture devices; unavailable entries carry their reason. */
export async function listDevices(): Promise<DeviceInfo[]> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("list_devices");
  } catch (e) {
    throw toIpcError(e);
  }
  return parseArray("list_devices", raw).map(parseDeviceInfo);
}

/** Enumerate the connected physical devices (identifiers) of one backend. */
export async function listDeviceIds(device: string): Promise<string[]> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("list_device_ids", { device });
  } catch (e) {
    throw toIpcError(e);
  }
  return parseArray("list_device_ids", raw).map((v) => {
    if (typeof v !== "string") {
      throw new IpcError("internal", "list_device_ids entry is not a string");
    }
    return v;
  });
}

/** Start a capture; resolves once the device is open and streaming. */
export async function startCapture(config: CaptureConfig): Promise<CaptureHandle> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("start_capture", { config: encodeCaptureConfig(config) });
  } catch (e) {
    throw toIpcError(e);
  }
  return parseCaptureHandle(raw);
}

/** Stop a capture; resolves with the final stats (recording is registered now). */
export async function stopCapture(captureId: string): Promise<CaptureStats> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("stop_capture", invokeArgs({ capture_id: captureId }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseCaptureStats(raw);
}

/** Poll a running (or just-stopped) capture's counters. */
export async function captureStats(captureId: string): Promise<CaptureStats> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("capture_stats", invokeArgs({ capture_id: captureId }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseCaptureStats(raw);
}

// --- v2: capture events (P3) -----------------------------------------------------

/** Tauri event names (contract v2 "Device capture"). */
export const CAPTURE_BATCH_EVENT = "capture://batch";
export const CAPTURE_STATS_EVENT = "capture://stats";

/** One decoded preview batch: interleaved f32 I/Q plus its sequence number. */
export interface CaptureBatch {
  captureId: string;
  seq: number;
  /** Interleaved I/Q f32 values (2 per complex sample). */
  iq: Float32Array;
}

/** A `capture://stats` event: which capture plus its counters. */
export interface CaptureStatsEvent {
  captureId: string;
  stats: CaptureStats;
}

/**
 * Narrow a `capture://batch` payload: `{ capture_id, seq, body }` (snake_case
 * canonical, camelCase accepted). The body is returned untyped —
 * {@link decodeCaptureBatchBody} owns the binary validation.
 */
function parseCaptureBatchEvent(raw: unknown): { captureId: string; seq: number; body: unknown } {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "capture://batch payload is not an object");
  }
  const body = (raw as Record<string, unknown>)["body"];
  if (body === undefined || body === null) {
    throw new IpcError("internal", "capture://batch payload has no body");
  }
  return {
    captureId: fieldString("capture://batch", raw, "capture_id", "captureId"),
    seq: fieldUint("capture://batch", raw, "seq"),
    body,
  };
}

/**
 * Narrow a `capture://stats` payload: `{ capture_id, …CaptureStats }` with the
 * counters either flattened into the payload or nested under `stats`.
 */
function parseCaptureStatsEvent(raw: unknown): CaptureStatsEvent {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "capture://stats payload is not an object");
  }
  const nested = (raw as Record<string, unknown>)["stats"];
  const statsSource =
    typeof nested === "object" && nested !== null ? nested : raw;
  return {
    captureId: fieldString("capture://stats", raw, "capture_id", "captureId"),
    stats: parseCaptureStats(statsSource),
  };
}

/**
 * Subscribe to preview batches. The callback receives decoded batches;
 * malformed events (bad body, bad shape) go to `onError` instead — a bad
 * batch must never kill the subscription. Unlisten to detach.
 */
export async function onCaptureBatch(
  cb: (batch: CaptureBatch) => void,
  onError?: (e: unknown) => void,
): Promise<UnlistenFn> {
  return listen<unknown>(CAPTURE_BATCH_EVENT, (event) => {
    try {
      const { captureId, seq, body } = parseCaptureBatchEvent(event.payload);
      cb({ captureId, seq, iq: decodeCaptureBatchBody(body) });
    } catch (e) {
      onError?.(e);
    }
  });
}

/** Subscribe to 1 Hz capture stats events. Unlisten to detach. */
export async function onCaptureStats(
  cb: (ev: CaptureStatsEvent) => void,
  onError?: (e: unknown) => void,
): Promise<UnlistenFn> {
  return listen<unknown>(CAPTURE_STATS_EVENT, (event) => {
    try {
      cb(parseCaptureStatsEvent(event.payload));
    } catch (e) {
      onError?.(e);
    }
  });
}

// --- v2: generator (P4) ----------------------------------------------------------

/** Generator parameters (contract v2 `GeneratorParams`). */
export interface GeneratorParams {
  modulation: string;
  /** `"0101…"`. */
  bits: string;
  samplesPerBit: number;
  pauseSamples: number;
  center: number;
}

/** Encode {@link GeneratorParams} for the wire (dual casing, like the others). */
export function encodeGeneratorParams(params: GeneratorParams): Record<string, unknown> {
  return invokeArgs({
    modulation: params.modulation,
    bits: params.bits,
    samples_per_bit: params.samplesPerBit,
    pause_samples: params.pauseSamples,
    center: params.center,
  });
}

/** Generate a transient `gen-<n>` signal and register it in the repository. */
export async function generate(params: GeneratorParams): Promise<SignalMeta> {
  return invokeMeta("generate", { params: encodeGeneratorParams(params) });
}

/** Save a signal as .complex (f32) at `path`. */
export async function saveSignalAs(signalId: string, path: string): Promise<void> {
  try {
    await invoke<unknown>("save_signal_as", invokeArgs({ signal_id: signalId, path }));
  } catch (e) {
    throw toIpcError(e);
  }
}

/** Native save dialog for .complex files; null when cancelled. */
export async function pickSaveComplexPath(defaultName = "signal.complex"): Promise<string | null> {
  const path = await save({
    defaultPath: defaultName,
    filters: [{ name: "Complex IQ (f32)", extensions: ["complex"] }],
  });
  return path;
}

// --- v2: project (P4) -------------------------------------------------------------

/** Project-format version this frontend reads and writes (contract v2). */
export const PROJECT_VERSION = 2;

/** One reopenable signal of a project (contract v2 `ProjectSignal`). */
export interface ProjectSignal {
  /** Absolute path as opened; never empty (in-memory signals are not saved). */
  path: string;
  /** Hz; handed to `open_signal` when the project is loaded. */
  sampleRate: number;
}

/** A project `analysis` entry: the params the signal's panel should restore. */
export interface ProjectAnalysisEntry {
  signalId: string;
  params: AnalysisParams;
}

/** A project `view` entry: the waveform view + mode the signal should restore. */
export interface ProjectViewEntry {
  signalId: string;
  /** Half-open `[start, end)` sample interval. */
  start: number;
  end: number;
  mode: WaveformMode;
}

/** A saved project (contract v2 `ProjectState`). */
export interface ProjectState {
  version: number;
  signals: ProjectSignal[];
  activeSignalId: string | null;
  analysis: ProjectAnalysisEntry[];
  view: ProjectViewEntry[];
}

/**
 * Encode a {@link ProjectState} for the wire. The backend struct is
 * snake_case serde; the camelCase spellings ride along like everywhere else.
 * The tuple fields serialize as JSON 2-/4-element arrays (`Vec<(…)>` semantics).
 */
export function encodeProjectState(state: ProjectState): Record<string, unknown> {
  return invokeArgs({
    version: state.version,
    signals: state.signals.map((s) => invokeArgs({ path: s.path, sample_rate: s.sampleRate })),
    active_signal_id: state.activeSignalId,
    analysis: state.analysis.map((e) => [e.signalId, encodeAnalysisParams(e.params)]),
    view: state.view.map((e) => [e.signalId, e.start, e.end, e.mode]),
  });
}

// Optional-field readers for the all-Option AnalysisParams and the project's
// Option fields: absent or null means "not set", anything else must be the
// right type (same defensive dual-key style as the required-field readers).

function optString(ctx: string, raw: object, snake: string, camel = snake): string | null {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (v === undefined || v === null) return null;
  if (typeof v !== "string") throw new IpcError("internal", `${ctx}.${snake} is not a string or null`);
  return v;
}

function optNumber(ctx: string, raw: object, snake: string, camel = snake): number | null {
  const v = (raw as Record<string, unknown>)[snake] ?? (raw as Record<string, unknown>)[camel];
  if (v === undefined || v === null) return null;
  if (typeof v !== "number" || !Number.isFinite(v)) {
    throw new IpcError("internal", `${ctx}.${snake} is not a finite number or null`);
  }
  return v;
}

function optUint(ctx: string, raw: object, snake: string, camel = snake): number | null {
  const v = optNumber(ctx, raw, snake, camel);
  if (v !== null && (!Number.isInteger(v) || v < 0)) {
    throw new IpcError("internal", `${ctx}.${snake} must be a non-negative integer or null, got ${v}`);
  }
  return v;
}

/**
 * Narrow a wire `AnalysisParams` (every field an Option: explicit `null` or a
 * missing key means "auto"). The project's per-signal entries use this shape;
 * `analyze_signal` *results* carry the resolved {@link EstimatedParams}.
 */
export function parseAnalysisParams(raw: unknown): AnalysisParams {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "AnalysisParams is not an object");
  }
  const params: AnalysisParams = {};
  const modulation = optString("AnalysisParams", raw, "modulation");
  if (modulation !== null) {
    if (modulation.length === 0) {
      throw new IpcError("internal", "AnalysisParams.modulation is empty");
    }
    params.modulation = modulation;
  }
  const bitLength = optUint("AnalysisParams", raw, "bit_length", "bitLength");
  if (bitLength !== null) {
    if (bitLength === 0) {
      throw new IpcError("internal", "AnalysisParams.bit_length must be > 0, got 0");
    }
    params.bitLength = bitLength;
  }
  const center = optNumber("AnalysisParams", raw, "center");
  if (center !== null) params.center = center;
  const noiseThreshold = optNumber("AnalysisParams", raw, "noise_threshold", "noiseThreshold");
  if (noiseThreshold !== null) params.noiseThreshold = noiseThreshold;
  const tolerance = optUint("AnalysisParams", raw, "tolerance");
  if (tolerance !== null) params.tolerance = tolerance;
  return params;
}

/** Narrow one `{path, sample_rate}` project signal. */
export function parseProjectSignal(raw: unknown): ProjectSignal {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "ProjectState.signals[] is not an object");
  }
  const path = fieldString("ProjectSignal", raw, "path");
  if (path.length === 0) {
    throw new IpcError("internal", "ProjectSignal.path is empty");
  }
  const sampleRate = fieldNumber("ProjectSignal", raw, "sample_rate", "sampleRate");
  if (sampleRate <= 0) {
    throw new IpcError("internal", `ProjectSignal.sample_rate must be > 0, got ${sampleRate}`);
  }
  return { path, sampleRate };
}

/** A fixed-length JSON tuple (serde `Vec<(…)>` element). */
function tuple(ctx: string, raw: unknown, length: number): unknown[] {
  if (!Array.isArray(raw) || raw.length !== length) {
    throw new IpcError("internal", `${ctx} is not a ${length}-element tuple`);
  }
  return raw;
}

function tupleId(ctx: string, raw: unknown): string {
  if (typeof raw !== "string" || raw.length === 0) {
    throw new IpcError("internal", `${ctx} signal id is not a non-empty string`);
  }
  return raw;
}

/** Narrow one `analysis` entry: `[signal_id, AnalysisParams]`. */
export function parseProjectAnalysisEntry(raw: unknown): ProjectAnalysisEntry {
  const t = tuple("ProjectState.analysis[]", raw, 2);
  return { signalId: tupleId("ProjectState.analysis[]", t[0]), params: parseAnalysisParams(t[1]) };
}

/** Narrow one `view` entry: `[signal_id, start, end, mode]`. */
export function parseProjectViewEntry(raw: unknown): ProjectViewEntry {
  const ctx = "ProjectState.view[]";
  const t = tuple(ctx, raw, 4);
  const start = t[1];
  const end = t[2];
  const mode = t[3];
  if (typeof start !== "number" || !Number.isInteger(start) || start < 0) {
    throw new IpcError("internal", `${ctx} start is not a non-negative integer`);
  }
  if (typeof end !== "number" || !Number.isInteger(end) || end < start) {
    throw new IpcError("internal", `${ctx} end is not an integer >= start`);
  }
  if (mode !== WaveformMode.Iq && mode !== WaveformMode.Amplitude) {
    throw new IpcError("internal", `${ctx} mode must be 1 (Iq) or 2 (Amplitude), got ${String(mode)}`);
  }
  return { signalId: tupleId(ctx, t[0]), start, end, mode };
}

/**
 * Narrow an untyped IPC payload into {@link ProjectState}. The version must
 * equal {@link PROJECT_VERSION} — anything else (older or newer) is a clean
 * `internal` rejection so the UI can surface "unsupported project version"
 * instead of misapplying fields.
 */
export function parseProjectState(raw: unknown): ProjectState {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "load_project returned a non-object");
  }
  const version = fieldUint("ProjectState", raw, "version");
  if (version !== PROJECT_VERSION) {
    throw new IpcError("internal", `unsupported project version ${version} (expected ${PROJECT_VERSION})`);
  }
  const signalsRaw = (raw as Record<string, unknown>)["signals"];
  if (!Array.isArray(signalsRaw)) {
    throw new IpcError("internal", "ProjectState.signals is missing or not an array");
  }
  const activeRaw =
    (raw as Record<string, unknown>)["active_signal_id"] ?? (raw as Record<string, unknown>)["activeSignalId"];
  if (activeRaw !== undefined && activeRaw !== null && typeof activeRaw !== "string") {
    throw new IpcError("internal", "ProjectState.active_signal_id is not a string or null");
  }
  const analysisRaw = (raw as Record<string, unknown>)["analysis"];
  if (!Array.isArray(analysisRaw)) {
    throw new IpcError("internal", "ProjectState.analysis is missing or not an array");
  }
  const viewRaw = (raw as Record<string, unknown>)["view"];
  if (!Array.isArray(viewRaw)) {
    throw new IpcError("internal", "ProjectState.view is missing or not an array");
  }
  return {
    version,
    signals: signalsRaw.map(parseProjectSignal),
    activeSignalId: activeRaw ?? null,
    analysis: analysisRaw.map(parseProjectAnalysisEntry),
    view: viewRaw.map(parseProjectViewEntry),
  };
}

/** Write a project file (pretty JSON is the backend's business). */
export async function saveProject(path: string, state: ProjectState): Promise<void> {
  try {
    await invoke<unknown>("save_project", invokeArgs({ path, state: encodeProjectState(state) }));
  } catch (e) {
    throw toIpcError(e);
  }
}

/** Read + parse a project file (the backend only parses; reopening is ours). */
export async function loadProject(path: string): Promise<ProjectState> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("load_project", invokeArgs({ path }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseProjectState(raw);
}

/** Native open dialog for .urhproj files; null when cancelled. */
export async function pickOpenProjectPath(): Promise<string | null> {
  const picked = await open({
    multiple: false,
    filters: [{ name: "URH project", extensions: ["urhproj"] }],
  });
  if (picked === null) return null;
  return Array.isArray(picked) ? (picked[0] ?? null) : picked;
}

/** Native save dialog for .urhproj files; null when cancelled. */
export async function pickSaveProjectPath(defaultName = "project.urhproj"): Promise<string | null> {
  return save({
    defaultPath: defaultName,
    filters: [{ name: "URH project", extensions: ["urhproj"] }],
  });
}

// --- v2: simulator (P5) -----------------------------------------------------------

/** One rule of a loaded .simulator config (contract v2 `SimRuleInfo`). */
export interface SimRuleInfo {
  index: number;
  /** Rule display name. */
  name: string;
  /** "rule" | "action" | … as in the engine. */
  kind: string;
  /** Human-readable one-liner from the config. */
  summary: string;
}

/** One transcript line of a simulation run (contract v2 `SimTranscriptEntry`). */
export interface SimTranscriptEntry {
  /** Seconds (sim clock). */
  timestamp: number;
  source: string;
  destination: string;
  /** Transferred bits, `"0101…"` (may be empty for e.g. log actions). */
  bits: string;
  /** e.g. "send", "log". */
  action: string;
}

/** Narrow an untyped IPC payload into one {@link SimRuleInfo}. */
export function parseSimRuleInfo(raw: unknown): SimRuleInfo {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "load_simulator returned a non-object entry");
  }
  return {
    index: fieldUint("SimRuleInfo", raw, "index"),
    name: fieldString("SimRuleInfo", raw, "name"),
    kind: fieldString("SimRuleInfo", raw, "kind"),
    summary: fieldString("SimRuleInfo", raw, "summary"),
  };
}

/** Narrow an untyped IPC payload into one {@link SimTranscriptEntry}. */
export function parseSimTranscriptEntry(raw: unknown): SimTranscriptEntry {
  if (typeof raw !== "object" || raw === null) {
    throw new IpcError("internal", "run_simulation returned a non-object entry");
  }
  const timestamp = fieldNumber("SimTranscriptEntry", raw, "timestamp");
  if (timestamp < 0) {
    throw new IpcError("internal", `SimTranscriptEntry.timestamp must be >= 0, got ${timestamp}`);
  }
  return {
    timestamp,
    source: fieldString("SimTranscriptEntry", raw, "source"),
    destination: fieldString("SimTranscriptEntry", raw, "destination"),
    bits: fieldString("SimTranscriptEntry", raw, "bits"),
    action: fieldString("SimTranscriptEntry", raw, "action"),
  };
}

/** Load a .simulator config and list its rules (read-only; no editing in v2). */
export async function loadSimulator(path: string): Promise<SimRuleInfo[]> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("load_simulator", invokeArgs({ path }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseArray("load_simulator", raw).map(parseSimRuleInfo);
}

/** Run the loaded config against its configured source, with a hard timeout. */
export async function runSimulation(path: string, timeoutSecs: number): Promise<SimTranscriptEntry[]> {
  let raw: unknown;
  try {
    raw = await invoke<unknown>("run_simulation", invokeArgs({ path, timeout_secs: timeoutSecs }));
  } catch (e) {
    throw toIpcError(e);
  }
  return parseArray("run_simulation", raw).map(parseSimTranscriptEntry);
}

/** Native open dialog for .simulator files; null when cancelled. */
export async function pickSimulatorPath(): Promise<string | null> {
  const picked = await open({
    multiple: false,
    filters: [{ name: "URH simulator", extensions: ["simulator"] }],
  });
  if (picked === null) return null;
  return Array.isArray(picked) ? (picked[0] ?? null) : picked;
}
