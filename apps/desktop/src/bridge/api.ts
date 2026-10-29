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
import { open } from "@tauri-apps/plugin-dialog";

import { type DecodedEnvelope, WaveformMode, decodeEnvelope } from "./protocol";

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
