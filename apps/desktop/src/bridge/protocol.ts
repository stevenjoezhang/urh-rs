/**
 * Binary frame decoders for the raw IPC responses.
 *
 * Waveform envelope frame layout (docs/ipc-contract.md, all fields
 * little-endian, payload at offset 24 so every f32 is 4-byte aligned):
 *
 * | offset | size | field                                   |
 * |--------|------|-----------------------------------------|
 * | 0      | 4    | magic b"URH1"                           |
 * | 4      | 2    | version = 1                             |
 * | 6      | 2    | dtype = 1 (IEEE-754 f32)                |
 * | 8      | 4    | request_id (u32, echo)                  |
 * | 12     | 4    | n_bins (u32)                            |
 * | 16     | 4    | mode (u32: 1 = Iq, 2 = Amplitude)       |
 * | 20     | 4    | flags = 0 (reserved)                    |
 * | 24     | 16n  | Iq payload: [min_i, max_i, min_q, max_q] per bin |
 * | 24     | 8n   | Amplitude payload: [min_mag, max_mag] per bin     |
 *
 * Spectrogram tile frames (v2) reuse the same header with dtype = 2 (u8) and
 * mode = 3; `n_bins` is the *time*-bin count and the payload is
 * `n_bins x freq_bins` raw u8 (see {@link decodeSpectrogramTile}).
 */

/** Wire values of `WaveformMode` (3 = Phase, 4 = Frequency are reserved). */
export enum WaveformMode {
  Iq = 1,
  Amplitude = 2,
}

/** Header size in bytes; the payload starts here (4-byte aligned). */
export const FRAME_HEADER_BYTES = 24;

/** f32 values per bin for each mode (4 for Iq, 2 for Amplitude). */
export function floatsPerBin(mode: WaveformMode): number {
  return mode === WaveformMode.Iq ? 4 : 2;
}

/**
 * Decoded envelope. `data` is a single interleaved Float32Array that is a
 * view into the frame buffer (no copy):
 *
 * - Iq mode:        4 floats per bin `[min_i, max_i, min_q, max_q]`
 * - Amplitude mode: 2 floats per bin `[min_mag, max_mag]`
 *
 * The renderer consumes this buffer directly with stride/offset vertex
 * attribs; use {@link binValue} for named per-component access from JS.
 */
export interface DecodedEnvelope {
  readonly requestId: number;
  readonly mode: WaveformMode;
  readonly nBins: number;
  readonly data: Float32Array;
}

/** Named per-bin components (indices within one bin's slice of `data`). */
export type BinComponent = "iMin" | "iMax" | "qMin" | "qMax" | "magMin" | "magMax";

const COMPONENT_INDEX: Readonly<Record<BinComponent, number>> = {
  iMin: 0,
  iMax: 1,
  qMin: 2,
  qMax: 3,
  magMin: 0,
  magMax: 1,
};

/**
 * Read one named component of one bin. Strided access into the interleaved
 * buffer — this is the copy-free equivalent of `env.iMin[bin]`.
 */
export function binValue(env: DecodedEnvelope, component: BinComponent, bin: number): number {
  if (bin < 0 || bin >= env.nBins) {
    throw new RangeError(`bin ${bin} out of range 0..${env.nBins - 1}`);
  }
  return env.data[bin * floatsPerBin(env.mode) + COMPONENT_INDEX[component]];
}

/** Why a frame was rejected. */
export type ProtocolErrorReason =
  | "buffer" // wrong input type, shorter than the header, or misaligned
  | "magic"
  | "version"
  | "dtype"
  | "mode"
  | "n-bins"
  | "truncated-payload" // fewer payload bytes than n_bins requires
  | "payload-size" // extra bytes beyond n_bins x component size
  | "batch-size" // capture batch body: zero length or not a multiple of 8
  | "batch-body"; // capture batch body: not base64 / not a u8 byte array

/** Thrown by {@link decodeEnvelope} on any contract violation. */
export class ProtocolError extends Error {
  readonly reason: ProtocolErrorReason;

  constructor(reason: ProtocolErrorReason, message: string) {
    super(message);
    this.name = "ProtocolError";
    this.reason = reason;
  }
}

const MAGIC = 0x31524855; // b"URH1" read as a little-endian u32
const VERSION = 1;
const DTYPE_F32 = 1;

/**
 * Validate and decode one envelope frame.
 *
 * Accepts the `ArrayBuffer` the Tauri invoke resolves to (per contract) and,
 * defensively, a `Uint8Array` view over the same bytes. All returned data is
 * a view into the input buffer; nothing is copied.
 */
export function decodeEnvelope(frame: ArrayBuffer | Uint8Array): DecodedEnvelope {
  let buffer: ArrayBuffer;
  let base: number;
  let byteLength: number;
  if (frame instanceof Uint8Array) {
    buffer = frame.buffer;
    base = frame.byteOffset;
    byteLength = frame.byteLength;
  } else if (frame instanceof ArrayBuffer) {
    buffer = frame;
    base = 0;
    byteLength = frame.byteLength;
  } else {
    throw new ProtocolError("buffer", `frame must be ArrayBuffer or Uint8Array, got ${typeof frame}`);
  }

  if (byteLength < FRAME_HEADER_BYTES) {
    throw new ProtocolError(
      "buffer",
      `frame is ${byteLength} bytes, shorter than the ${FRAME_HEADER_BYTES}-byte header`,
    );
  }
  if ((base + FRAME_HEADER_BYTES) % 4 !== 0) {
    throw new ProtocolError("buffer", "payload would not be 4-byte aligned");
  }

  const dv = new DataView(buffer, base, byteLength);

  if (dv.getUint32(0, true) !== MAGIC) {
    throw new ProtocolError("magic", "bad magic; expected b\"URH1\"");
  }
  const version = dv.getUint16(4, true);
  if (version !== VERSION) {
    throw new ProtocolError("version", `expected version ${VERSION}, got ${version}`);
  }
  const dtype = dv.getUint16(6, true);
  if (dtype !== DTYPE_F32) {
    throw new ProtocolError("dtype", `expected dtype 1 (f32), got ${dtype}`);
  }

  const requestId = dv.getUint32(8, true);
  const nBins = dv.getUint32(12, true);
  const modeRaw = dv.getUint32(16, true);
  if (modeRaw !== WaveformMode.Iq && modeRaw !== WaveformMode.Amplitude) {
    throw new ProtocolError("mode", `mode ${modeRaw} is not 1 (Iq) or 2 (Amplitude)`);
  }
  const mode: WaveformMode = modeRaw;
  // flags at offset 20 are reserved (0 in v1); no v1 semantics, ignored.

  if (nBins === 0) {
    throw new ProtocolError("n-bins", "n_bins is 0");
  }

  const fpb = floatsPerBin(mode);
  const payloadBytes = fpb * 4 * nBins;
  const expected = FRAME_HEADER_BYTES + payloadBytes;
  if (byteLength < expected) {
    throw new ProtocolError(
      "truncated-payload",
      `payload is ${byteLength - FRAME_HEADER_BYTES} bytes, but ${nBins} bins of mode ${modeRaw} need ${payloadBytes}`,
    );
  }
  if (byteLength > expected) {
    throw new ProtocolError(
      "payload-size",
      `frame is ${byteLength} bytes, but ${nBins} bins of mode ${modeRaw} imply ${expected}`,
    );
  }

  const data = new Float32Array(buffer, base + FRAME_HEADER_BYTES, fpb * nBins);
  return { requestId, mode, nBins, data };
}

// --- capture preview batch bodies (v2, P3) --------------------------------------
//
// `capture://batch` events carry `{ capture_id, seq }` plus a raw body of
// interleaved LE f32 I/Q samples (8 bytes per complex sample). Tauri events
// are JSON-serialized, so depending on how the emitter wires the bytes the
// body arrives as an ArrayBuffer / Uint8Array (raw, ideal), a base64 string
// or a JSON array of u8 bytes — all four decode here. A JSON array of *float
// sample values* is rejected on purpose: it is indistinguishable from a byte
// array with even length, and mis-decoding it would silently corrupt the
// preview (see docs/ipc-contract.md "Device capture").

/** Bytes per complex f32 I/Q sample on the wire. */
export const CAPTURE_SAMPLE_BYTES = 8;

function base64ToBytes(s: string): Uint8Array {
  try {
    const bin = atob(s);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return bytes;
  } catch {
    throw new ProtocolError("batch-body", "capture batch body string is not valid base64");
  }
}

function byteArrayToBytes(values: readonly unknown[]): Uint8Array {
  const bytes = new Uint8Array(values.length);
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    if (typeof v !== "number" || !Number.isInteger(v) || v < 0 || v > 255) {
      throw new ProtocolError(
        "batch-body",
        `capture batch body array must contain u8 bytes (0..255), got ${String(v)} at [${i}] — ` +
          "float sample values are not a supported event encoding",
      );
    }
    bytes[i] = v;
  }
  return bytes;
}

/**
 * Validate and decode a capture batch body into its interleaved f32 I/Q
 * values (2 floats per complex sample). The byte length must be a non-zero
 * multiple of 8. Aligned binary inputs decode to a view into the original
 * buffer (no copy); misaligned views, base64 strings and byte arrays copy.
 */
export function decodeCaptureBatchBody(body: unknown): Float32Array {
  let bytes: Uint8Array;
  if (body instanceof Uint8Array) {
    bytes = body;
  } else if (body instanceof ArrayBuffer) {
    bytes = new Uint8Array(body);
  } else if (typeof body === "string") {
    bytes = base64ToBytes(body);
  } else if (Array.isArray(body)) {
    bytes = byteArrayToBytes(body);
  } else {
    throw new ProtocolError(
      "buffer",
      `capture batch body must be ArrayBuffer, Uint8Array, base64 string or byte array, got ${typeof body}`,
    );
  }

  if (bytes.length === 0 || bytes.length % CAPTURE_SAMPLE_BYTES !== 0) {
    throw new ProtocolError(
      "batch-size",
      `capture batch body is ${bytes.length} bytes; must be a non-zero multiple of ${CAPTURE_SAMPLE_BYTES} (f32 I/Q pairs)`,
    );
  }

  if (bytes.byteOffset % 4 === 0) {
    return new Float32Array(bytes.buffer, bytes.byteOffset, bytes.length / 4);
  }
  // Unaligned view: Float32Array needs 4-byte alignment, so copy once.
  const copy = new Uint8Array(bytes.length);
  copy.set(bytes);
  return new Float32Array(copy.buffer);
}

// --- spectrogram tiles (v2) ---------------------------------------------------

/** Allowed STFT window sizes (contract v2: {256, 1024, 4096}). */
export const SPECTROGRAM_WINDOWS = [256, 1024, 4096] as const;

/** STFT window size; hop is fixed at `window_size / 2` (contract v2). */
export type SpectrogramWindow = (typeof SPECTROGRAM_WINDOWS)[number];

/** Type guard for {@link SPECTROGRAM_WINDOWS} members. */
export function isSpectrogramWindow(w: number): w is SpectrogramWindow {
  return (SPECTROGRAM_WINDOWS as readonly number[]).includes(w);
}

/** Frequency bins per tile row; the contract couples this to the window. */
export function freqBinsForWindow(windowSize: SpectrogramWindow): number {
  return windowSize / 2;
}

/** STFT hop in samples; fixed at half the window (contract v2). */
export function hopForWindow(windowSize: SpectrogramWindow): number {
  return windowSize / 2;
}

/** Header `dtype` for spectrogram tiles (u8 payload). */
export const DTYPE_U8 = 2;
/** Header `mode` for spectrogram tiles. */
export const SPECTROGRAM_FRAME_MODE = 3;

/**
 * Decoded spectrogram tile. `data` is a view into the frame buffer (no copy):
 * `data[t * freqBins + f]` is the dB-mapped byte of time bin `t`, frequency
 * bin `f`, with **DC first** (low frequencies at row start — renderers flip
 * as needed). Time bin `t` covers samples
 * `[start + t * hop, start + (t + 1) * hop)` of the requested range.
 */
export interface DecodedSpectrogramTile {
  readonly requestId: number;
  readonly windowSize: SpectrogramWindow;
  readonly freqBins: number;
  readonly nTimeBins: number;
  readonly data: Uint8Array;
}

/**
 * Validate and decode one spectrogram tile frame.
 *
 * Same header as envelopes, but dtype = 2 (u8) and mode = 3; `n_bins` is the
 * time-bin count and *may be 0* (range shorter than one hop → empty payload,
 * which is valid). `freq_bins` is not transmitted — the contract couples it
 * to `window_size`, which the caller therefore must supply. Unlike envelopes,
 * the u8 payload has no alignment requirement, so unaligned `Uint8Array`
 * views are accepted.
 */
export function decodeSpectrogramTile(
  frame: ArrayBuffer | Uint8Array,
  windowSize: SpectrogramWindow,
): DecodedSpectrogramTile {
  let buffer: ArrayBuffer;
  let base: number;
  let byteLength: number;
  if (frame instanceof Uint8Array) {
    buffer = frame.buffer;
    base = frame.byteOffset;
    byteLength = frame.byteLength;
  } else if (frame instanceof ArrayBuffer) {
    buffer = frame;
    base = 0;
    byteLength = frame.byteLength;
  } else {
    throw new ProtocolError("buffer", `frame must be ArrayBuffer or Uint8Array, got ${typeof frame}`);
  }

  if (byteLength < FRAME_HEADER_BYTES) {
    throw new ProtocolError(
      "buffer",
      `frame is ${byteLength} bytes, shorter than the ${FRAME_HEADER_BYTES}-byte header`,
    );
  }

  const dv = new DataView(buffer, base, byteLength);

  if (dv.getUint32(0, true) !== MAGIC) {
    throw new ProtocolError("magic", "bad magic; expected b\"URH1\"");
  }
  const version = dv.getUint16(4, true);
  if (version !== VERSION) {
    throw new ProtocolError("version", `expected version ${VERSION}, got ${version}`);
  }
  const dtype = dv.getUint16(6, true);
  if (dtype !== DTYPE_U8) {
    throw new ProtocolError("dtype", `expected dtype 2 (u8), got ${dtype}`);
  }

  const requestId = dv.getUint32(8, true);
  const nTimeBins = dv.getUint32(12, true);
  const modeRaw = dv.getUint32(16, true);
  if (modeRaw !== SPECTROGRAM_FRAME_MODE) {
    throw new ProtocolError("mode", `mode ${modeRaw} is not 3 (spectrogram)`);
  }
  // flags at offset 20 are reserved (0 in v1/v2); no semantics, ignored.

  const freqBins = freqBinsForWindow(windowSize);
  const payloadBytes = freqBins * nTimeBins;
  const expected = FRAME_HEADER_BYTES + payloadBytes;
  if (byteLength < expected) {
    throw new ProtocolError(
      "truncated-payload",
      `payload is ${byteLength - FRAME_HEADER_BYTES} bytes, but ${nTimeBins} time bins x ${freqBins} freq bins need ${payloadBytes}`,
    );
  }
  if (byteLength > expected) {
    throw new ProtocolError(
      "payload-size",
      `frame is ${byteLength} bytes, but ${nTimeBins} time bins x ${freqBins} freq bins imply ${expected}`,
    );
  }

  const data = new Uint8Array(buffer, base + FRAME_HEADER_BYTES, payloadBytes);
  return { requestId, windowSize, freqBins, nTimeBins, data };
}
