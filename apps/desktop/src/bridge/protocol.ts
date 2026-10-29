/**
 * Binary frame decoder for `get_waveform_envelope` responses.
 *
 * Frame layout (docs/ipc-contract.md, all fields little-endian, payload at
 * offset 24 so every f32 is 4-byte aligned):
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
  | "payload-size"; // extra bytes beyond n_bins x component size

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
