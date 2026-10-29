/**
 * Bridge wrapper tests for the v2 commands (get_spectrogram_tile,
 * analyze_signal, cancel_analysis, run_awre; P3/P4: list_devices,
 * start/stop_capture, capture_stats, generate, save_signal_as) and the
 * capture://batch and capture://stats event subscriptions: invoke-argument
 * shapes (snake_case + camelCase dual keys, Option handling, nested params),
 * response parsing/validation, and IpcError normalization. The Tauri core,
 * dialog and event modules are mocked at the module boundary — nothing here
 * needs a webview.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";

import {
  CAPTURE_BATCH_EVENT,
  CAPTURE_STATS_EVENT,
  IpcError,
  PROJECT_VERSION,
  analyzeSignal,
  cancelAnalysis,
  captureStats,
  encodeAnalysisParams,
  encodeCaptureConfig,
  encodeGeneratorParams,
  encodeProjectState,
  generate,
  getSpectrogramTile,
  listDevices,
  loadProject,
  loadSimulator,
  onCaptureBatch,
  onCaptureStats,
  pickOpenProjectPath,
  pickSaveComplexPath,
  pickSaveProjectPath,
  pickSimulatorPath,
  runAwre,
  runSimulation,
  saveProject,
  saveSignalAs,
  startCapture,
  stopCapture,
} from "./api";
import { parseAnalysisResult, parseFieldLabel, parseProjectState, parseSimTranscriptEntry } from "./api";
import { WaveformMode, ProtocolError } from "./protocol";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(), save: vi.fn() }));

const invokeMock = vi.mocked(invoke);
const listenMock = vi.mocked(listen);
const saveMock = vi.mocked(save);
const openMock = vi.mocked(open);

beforeEach(() => {
  invokeMock.mockReset();
  listenMock.mockReset();
  saveMock.mockReset();
  openMock.mockReset();
});

/** Minimal valid header + payload for a spectrogram tile frame. */
function tileFrameBytes(nTimeBins: number, windowSize: 256 | 1024 | 4096, requestId: number): ArrayBuffer {
  const freqBins = windowSize / 2;
  const buf = new ArrayBuffer(24 + nTimeBins * freqBins);
  const dv = new DataView(buf);
  dv.setUint32(0, 0x31524855, true); // b"URH1"
  dv.setUint16(4, 1, true); // version
  dv.setUint16(6, 2, true); // dtype = u8
  dv.setUint32(8, requestId, true);
  dv.setUint32(12, nTimeBins, true);
  dv.setUint32(16, 3, true); // mode = spectrogram
  dv.setUint32(20, 0, true);
  const payload = new Uint8Array(buf, 24);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 13) & 0xff;
  return buf;
}

describe("encodeAnalysisParams", () => {
  it("sends both key spellings and null for auto fields", () => {
    const encoded = encodeAnalysisParams({});
    expect(encoded).toEqual({
      modulation: null,
      bit_length: null,
      bitLength: null,
      center: null,
      noise_threshold: null,
      noiseThreshold: null,
      tolerance: null,
    });
  });

  it("passes through explicit values in both spellings", () => {
    const encoded = encodeAnalysisParams({ modulation: "FSK", bitLength: 100, tolerance: 5 });
    expect(encoded.modulation).toBe("FSK");
    expect(encoded.bit_length).toBe(100);
    expect(encoded.bitLength).toBe(100);
    expect(encoded.tolerance).toBe(5);
    expect(encoded.center).toBeNull();
  });
});

describe("getSpectrogramTile", () => {
  it("invokes with dual-cased args and derives freq_bins from the window", async () => {
    invokeMock.mockResolvedValue(tileFrameBytes(4, 256, 9));
    const tile = await getSpectrogramTile("sig-1", 100, 200, 256, 9);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("get_spectrogram_tile", {
      signal_id: "sig-1",
      signalId: "sig-1",
      start: 100,
      end: 200,
      freq_bins: 128,
      freqBins: 128,
      window_size: 256,
      windowSize: 256,
      request_id: 9,
      requestId: 9,
    });
    expect(tile.nTimeBins).toBe(4);
    expect(tile.freqBins).toBe(128);
    expect(tile.requestId).toBe(9);
  });

  it("normalizes a rejection into IpcError with the backend code", async () => {
    invokeMock.mockRejectedValue({ code: "invalid-argument", message: "window_size not allowed" });
    await expect(getSpectrogramTile("sig-1", 0, 10, 999 as never, 1)).rejects.toMatchObject({
      name: "IpcError",
      code: "invalid-argument",
    });
  });

  it("rejects a non-binary response as internal", async () => {
    invokeMock.mockResolvedValue([1, 2, 3]);
    await expect(getSpectrogramTile("sig-1", 0, 10, 256, 1)).rejects.toMatchObject({
      name: "IpcError",
      code: "internal",
    });
  });

  it("rejects a malformed frame as a protocol error", async () => {
    const bad = tileFrameBytes(2, 256, 1);
    new DataView(bad).setUint16(6, 1, true); // dtype = f32
    invokeMock.mockResolvedValue(bad);
    await expect(getSpectrogramTile("sig-1", 0, 10, 256, 1)).rejects.toMatchObject({ name: "ProtocolError" });
  });
});

/** One well-formed message payload in the backend's snake_case JSON. */
function messageRaw(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    index: 0,
    bit_count: 8,
    pause: 1000,
    plain_bits: "01011010",
    start_sample: 42,
    end_sample: 42 + 800 + 1000,
    ...overrides,
  };
}

function resultRaw(): Record<string, unknown> {
  return {
    params: {
      modulation: "ASK",
      bit_length: 100,
      center: 0.01,
      noise_threshold: 0.001,
      tolerance: 5,
    },
    messages: [messageRaw({ index: 0 }), messageRaw({ index: 1, plain_bits: "11", bit_count: 2, start_sample: 5000, end_sample: 5200 })],
  };
}

describe("analyzeSignal", () => {
  it("invokes with range/options as null and dual-cased params", async () => {
    invokeMock.mockResolvedValue(resultRaw());
    await analyzeSignal("sig-2", null, { modulation: "ASK" }, 7);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("analyze_signal", {
      signal_id: "sig-2",
      signalId: "sig-2",
      range_start: null,
      rangeStart: null,
      range_end: null,
      rangeEnd: null,
      params: {
        modulation: "ASK",
        bit_length: null,
        bitLength: null,
        center: null,
        noise_threshold: null,
        noiseThreshold: null,
        tolerance: null,
      },
      job_id: 7,
      jobId: 7,
    });
  });

  it("passes a selection range through as numbers", async () => {
    invokeMock.mockResolvedValue(resultRaw());
    await analyzeSignal("sig-2", { start: 100, end: 200 }, {}, 8);
    const args = invokeMock.mock.calls[0]?.[1] as Record<string, unknown>;
    expect(args.range_start).toBe(100);
    expect(args.range_end).toBe(200);
    expect(args.rangeStart).toBe(100);
    expect(args.rangeEnd).toBe(200);
  });

  it("parses a snake_case result", async () => {
    invokeMock.mockResolvedValue(resultRaw());
    const result = await analyzeSignal("sig-2", null, {}, 1);
    expect(result.params.modulation).toBe("ASK");
    expect(result.params.bitLength).toBe(100);
    expect(result.messages).toHaveLength(2);
    expect(result.messages[0]).toEqual({
      index: 0,
      bitCount: 8,
      pause: 1000,
      plainBits: "01011010",
      startSample: 42,
      endSample: 1842,
    });
  });

  it("accepts camelCase result keys defensively", async () => {
    invokeMock.mockResolvedValue({
      params: { modulation: "FSK", bitLength: 10, center: 0, noiseThreshold: 0, tolerance: 0 },
      messages: [
        { index: 3, bitCount: 1, pause: 0, plainBits: "1", startSample: 5, endSample: 15 },
      ],
    });
    const result = await analyzeSignal("sig-2", null, {}, 1);
    expect(result.messages[0]?.plainBits).toBe("1");
    expect(result.messages[0]?.bitCount).toBe(1);
  });

  it("surfaces an AppError rejection as IpcError with its code", async () => {
    invokeMock.mockRejectedValue({ code: "cancelled", message: "job cancelled" });
    const err = await analyzeSignal("sig-2", null, {}, 1).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(IpcError);
    expect((err as IpcError).code).toBe("cancelled");
  });
});

describe("parseAnalysisResult validation", () => {
  it("rejects a bit_count that disagrees with plain_bits", () => {
    expect(() => parseAnalysisResult(resultRawWith({ bit_count: 3 }))).toThrowError(/bit_count/);
  });

  it("rejects plain_bits containing non-bit characters", () => {
    expect(() => parseAnalysisResult(resultRawWith({ plain_bits: "0101AB" }))).toThrowError(/0\/1/);
  });

  it("rejects end_sample before start_sample", () => {
    expect(() => parseAnalysisResult(resultRawWith({ end_sample: 10 }))).toThrowError(/end_sample/);
  });

  it("rejects a negative pause", () => {
    expect(() => parseAnalysisResult(resultRawWith({ pause: -1 }))).toThrowError(/pause/);
  });

  it("rejects a non-integer bit_length", () => {
    const raw = resultRaw();
    (raw.params as Record<string, unknown>)["bit_length"] = 1.5;
    expect(() => parseAnalysisResult(raw)).toThrowError(/bit_length/);
  });

  it("rejects a missing messages array", () => {
    expect(() => parseAnalysisResult({ params: resultRaw().params })).toThrowError(/messages/);
  });

  it("rejects a non-object payload", () => {
    expect(() => parseAnalysisResult(42)).toThrowError(/non-object/);
  });
});

/** resultRaw with the first message's fields overridden. */
function resultRawWith(overrides: Record<string, unknown>): Record<string, unknown> {
  const raw = resultRaw();
  (raw.messages as Record<string, unknown>[])[0] = messageRaw(overrides);
  return raw;
}

describe("cancelAnalysis", () => {
  it("returns the boolean result", async () => {
    invokeMock.mockResolvedValue(true);
    await expect(cancelAnalysis(7)).resolves.toBe(true);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("cancel_analysis", { job_id: 7, jobId: 7 });
  });

  it("rejects a non-boolean response", async () => {
    invokeMock.mockResolvedValue("yes");
    await expect(cancelAnalysis(7)).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });
});

describe("runAwre", () => {
  it("invokes with the range/params and parses labels", async () => {
    invokeMock.mockResolvedValue([
      { message_index: 0, name: "preamble", bit_start: 0, bit_end: 8, color_index: 0 },
      { message_index: 0, name: "sync", bit_start: 8, bit_end: 16, color_index: 1 },
    ]);
    const labels = await runAwre("sig-3", { start: 0, end: 1000 }, {});
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("run_awre", {
      signal_id: "sig-3",
      signalId: "sig-3",
      range_start: 0,
      rangeStart: 0,
      range_end: 1000,
      rangeEnd: 1000,
      params: expect.objectContaining({ modulation: null }),
    });
    expect(labels).toEqual([
      { messageIndex: 0, name: "preamble", bitStart: 0, bitEnd: 8, colorIndex: 0 },
      { messageIndex: 0, name: "sync", bitStart: 8, bitEnd: 16, colorIndex: 1 },
    ]);
  });

  it("rejects labels with inverted bit ranges", () => {
    expect(() =>
      parseFieldLabel({ message_index: 0, name: "x", bit_start: 5, bit_end: 2, color_index: 0 }),
    ).toThrowError(/bit_end/);
  });

  it("rejects a non-array response", async () => {
    invokeMock.mockResolvedValue({});
    await expect(runAwre("sig-3", null, {})).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });
});

// --- v2 additions: device capture (P3) ---------------------------------------------

describe("encodeCaptureConfig", () => {
  it("sends both key spellings with Options as explicit null", () => {
    const encoded = encodeCaptureConfig({
      device: "mock",
      sampleRate: 1e6,
      frequency: 433.92e6,
      bandwidth: 1e6,
      gain: 20,
      record: true,
      host: null,
      port: null,
    });
    expect(encoded).toEqual({
      device: "mock",
      sample_rate: 1e6,
      sampleRate: 1e6,
      frequency: 433.92e6,
      bandwidth: 1e6,
      gain: 20,
      record: true,
      host: null,
      port: null,
    });
  });

  it("passes rtl-tcp host/port through", () => {
    const encoded = encodeCaptureConfig({
      device: "rtl-tcp",
      sampleRate: 2.4e6,
      frequency: 100e6,
      bandwidth: 2e6,
      gain: 0,
      record: false,
      host: "127.0.0.1",
      port: 1234,
    });
    expect(encoded.host).toBe("127.0.0.1");
    expect(encoded.port).toBe(1234);
    expect(encoded.record).toBe(false);
  });
});

describe("listDevices", () => {
  it("parses the device entries", async () => {
    invokeMock.mockResolvedValue([
      { name: "mock", backend: "mock", available: true, detail: "" },
      { name: "rtl-tcp", backend: "rtltcp", available: false, detail: "connection refused" },
    ]);
    const devices = await listDevices();
    expect(invokeMock).toHaveBeenCalledWith("list_devices");
    expect(devices).toEqual([
      { name: "mock", backend: "mock", available: true, detail: "" },
      { name: "rtl-tcp", backend: "rtltcp", available: false, detail: "connection refused" },
    ]);
  });

  it("rejects an entry with an empty name", async () => {
    invokeMock.mockResolvedValue([{ name: "", backend: "x", available: true, detail: "" }]);
    await expect(listDevices()).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });

  it("normalizes a backend rejection into IpcError", async () => {
    invokeMock.mockRejectedValue({ code: "device-open-failed", message: "no device" });
    await expect(listDevices()).rejects.toMatchObject({ name: "IpcError", code: "device-open-failed" });
  });
});

describe("startCapture", () => {
  it("invokes with the nested dual-cased config and parses the handle", async () => {
    invokeMock.mockResolvedValue({ capture_id: "cap-3", signal_id: "rec-7" });
    const handle = await startCapture({
      device: "mock",
      sampleRate: 1e6,
      frequency: 433.92e6,
      bandwidth: 1e6,
      gain: 20,
      record: true,
      host: null,
      port: null,
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("start_capture", {
      config: {
        device: "mock",
        sample_rate: 1e6,
        sampleRate: 1e6,
        frequency: 433.92e6,
        bandwidth: 1e6,
        gain: 20,
        record: true,
        host: null,
        port: null,
      },
    });
    expect(handle).toEqual({ captureId: "cap-3", signalId: "rec-7" });
  });

  it("accepts a null signal_id for preview-only captures (and camelCase keys)", async () => {
    invokeMock.mockResolvedValue({ captureId: "cap-1", signalId: null });
    const handle = await startCapture({
      device: "mock",
      sampleRate: 1,
      frequency: 0,
      bandwidth: 0,
      gain: 0,
      record: false,
      host: null,
      port: null,
    });
    expect(handle).toEqual({ captureId: "cap-1", signalId: null });
  });

  it("rejects a handle without a capture_id", async () => {
    invokeMock.mockResolvedValue({ signal_id: "rec-1" });
    await expect(
      startCapture({ device: "m", sampleRate: 1, frequency: 0, bandwidth: 0, gain: 0, record: false, host: null, port: null }),
    ).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });
});

describe("stopCapture / captureStats", () => {
  const statsRaw = { dropped_preview_batches: 2, recorded_samples: 123456, overflow_detected: false };
  const stats = { droppedPreviewBatches: 2, recordedSamples: 123456, overflowDetected: false };

  it("stop_capture returns the final stats", async () => {
    invokeMock.mockResolvedValue(statsRaw);
    await expect(stopCapture("cap-3")).resolves.toEqual(stats);
    expect(invokeMock).toHaveBeenCalledWith("stop_capture", { capture_id: "cap-3", captureId: "cap-3" });
  });

  it("capture_stats polls the counters", async () => {
    invokeMock.mockResolvedValue({ droppedPreviewBatches: 0, recordedSamples: 10, overflowDetected: true });
    await expect(captureStats("cap-3")).resolves.toEqual({
      droppedPreviewBatches: 0,
      recordedSamples: 10,
      overflowDetected: true,
    });
    expect(invokeMock).toHaveBeenCalledWith("capture_stats", { capture_id: "cap-3", captureId: "cap-3" });
  });

  it("reject a payload with a negative counter", async () => {
    invokeMock.mockResolvedValue({ dropped_preview_batches: -1, recorded_samples: 0, overflow_detected: false });
    await expect(captureStats("cap-3")).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });

  it("normalize backend rejections", async () => {
    invokeMock.mockRejectedValue({ code: "unknown-capture", message: "no such capture" });
    await expect(stopCapture("cap-9")).rejects.toMatchObject({ name: "IpcError", code: "unknown-capture" });
  });
});

describe("capture events", () => {
  /** Wire a listen mock that records the handler; returns (trigger, unlisten). */
  function armListen(): { trigger: (payload: unknown) => void; unlisten: () => void } {
    let handler: ((event: { payload: unknown }) => void) | null = null;
    let unlistened = false;
    listenMock.mockImplementation(async (_event, h) => {
      handler = h as (event: { payload: unknown }) => void;
      return () => {
        unlistened = true;
      };
    });
    return {
      trigger: (payload: unknown) => {
        if (handler !== null) handler({ payload });
      },
      unlisten: () => {
        expect(unlistened).toBe(true);
      },
    };
  }

  /** Four LE f32 bytes per value, little-endian. */
  function f32Bytes(values: number[]): ArrayBuffer {
    const buf = new ArrayBuffer(values.length * 4);
    const dv = new DataView(buf);
    values.forEach((v, i) => dv.setFloat32(i * 4, v, true));
    return buf;
  }

  it("subscribe to capture://batch and decode the body", async () => {
    const armed = armListen();
    const batches: { captureId: string; seq: number; iq: Float32Array }[] = [];
    const unlisten = await onCaptureBatch((b) => batches.push(b));
    expect(listenMock).toHaveBeenCalledTimes(1);
    expect(listenMock.mock.calls[0]?.[0]).toBe(CAPTURE_BATCH_EVENT);
    expect(CAPTURE_BATCH_EVENT).toBe("capture://batch");

    armed.trigger({ capture_id: "cap-1", seq: 4, body: f32Bytes([1.5, -2.25, 0, 0.5]) });
    expect(batches).toHaveLength(1);
    expect(batches[0]?.captureId).toBe("cap-1");
    expect(batches[0]?.seq).toBe(4);
    expect(Array.from(batches[0]?.iq ?? [])).toEqual([1.5, -2.25, 0, 0.5]);

    unlisten();
    armed.unlisten();
  });

  it("accepts camelCase payload keys and a base64 body", async () => {
    const armed = armListen();
    const seen: number[] = [];
    await onCaptureBatch((b) => seen.push(b.seq));
    const bytes = new Uint8Array(f32Bytes([0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25]));
    armed.trigger({ captureId: "cap-1", seq: 0, body: btoa(String.fromCharCode(...bytes)) });
    expect(seen).toEqual([0]);
  });

  it("routes malformed batches to onError without killing the subscription", async () => {
    const armed = armListen();
    const good: number[] = [];
    const errors: unknown[] = [];
    await onCaptureBatch((b) => good.push(b.seq), (e) => errors.push(e));
    armed.trigger({ capture_id: "cap-1", seq: 1, body: "!!!not base64!!!" }); // bad body -> ProtocolError
    armed.trigger({ capture_id: "cap-1", seq: 2 }); // no body at all -> IpcError
    armed.trigger({ capture_id: "cap-1", seq: 3, body: f32Bytes([1, 2, 3, 4]) }); // 16 bytes, ok
    expect(good).toEqual([3]);
    expect(errors).toHaveLength(2);
    expect(errors[0]).toBeInstanceOf(ProtocolError);
    expect(errors[1]).toBeInstanceOf(IpcError);
  });

  it("subscribe to capture://stats, flattened or nested", async () => {
    const armed = armListen();
    const events: { captureId: string; stats: { recordedSamples: number } }[] = [];
    const unlisten = await onCaptureStats((ev) => events.push(ev));
    expect(listenMock.mock.calls[0]?.[0]).toBe(CAPTURE_STATS_EVENT);
    expect(CAPTURE_STATS_EVENT).toBe("capture://stats");

    armed.trigger({ capture_id: "cap-1", dropped_preview_batches: 1, recorded_samples: 99, overflow_detected: false });
    armed.trigger({
      captureId: "cap-1",
      stats: { droppedPreviewBatches: 0, recordedSamples: 100, overflowDetected: true },
    });
    expect(events).toEqual([
      { captureId: "cap-1", stats: { droppedPreviewBatches: 1, recordedSamples: 99, overflowDetected: false } },
      { captureId: "cap-1", stats: { droppedPreviewBatches: 0, recordedSamples: 100, overflowDetected: true } },
    ]);

    unlisten();
    armed.unlisten();
  });
});

// --- v2 additions: generator (P4) ---------------------------------------------------

describe("generate", () => {
  it("invokes with nested dual-cased params and parses the meta", async () => {
    invokeMock.mockResolvedValue({
      signal_id: "gen-1",
      path: "",
      display_name: "",
      format: "complex",
      sample_count: 1800,
      sample_rate: 1e6,
      duration_secs: 0.0018,
    });
    const meta = await generate({ modulation: "ASK", bits: "10101100", samplesPerBit: 100, pauseSamples: 1000, center: 0.5 });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("generate", {
      params: {
        modulation: "ASK",
        bits: "10101100",
        samples_per_bit: 100,
        samplesPerBit: 100,
        pause_samples: 1000,
        pauseSamples: 1000,
        center: 0.5,
      },
    });
    expect(meta.signal_id).toBe("gen-1");
    expect(meta.sample_count).toBe(1800);
  });

  it("normalizes a backend rejection", async () => {
    invokeMock.mockRejectedValue({ code: "invalid-argument", message: "bits too long" });
    await expect(generate({ modulation: "ASK", bits: "1", samplesPerBit: 1, pauseSamples: 0, center: 0 })).rejects.toMatchObject({
      name: "IpcError",
      code: "invalid-argument",
    });
  });

  it("encodeGeneratorParams exposes the dual-cased shape directly", () => {
    expect(encodeGeneratorParams({ modulation: "FSK", bits: "01", samplesPerBit: 8, pauseSamples: 4, center: -1 })).toEqual({
      modulation: "FSK",
      bits: "01",
      samples_per_bit: 8,
      samplesPerBit: 8,
      pause_samples: 4,
      pauseSamples: 4,
      center: -1,
    });
  });
});

describe("saveSignalAs / pickSaveComplexPath", () => {
  it("invokes save_signal_as with dual-cased ids", async () => {
    invokeMock.mockResolvedValue(undefined);
    await saveSignalAs("gen-1", "/tmp/out.complex");
    expect(invokeMock).toHaveBeenCalledWith("save_signal_as", {
      signal_id: "gen-1",
      signalId: "gen-1",
      path: "/tmp/out.complex",
    });
  });

  it("normalizes a write failure", async () => {
    invokeMock.mockRejectedValue({ code: "io", message: "disk full" });
    await expect(saveSignalAs("gen-1", "/tmp/out.complex")).rejects.toMatchObject({ name: "IpcError", code: "io" });
  });

  it("opens the native save dialog with a .complex filter", async () => {
    saveMock.mockResolvedValue("/home/u/out.complex");
    await expect(pickSaveComplexPath()).resolves.toBe("/home/u/out.complex");
    expect(saveMock).toHaveBeenCalledTimes(1);
    expect(saveMock).toHaveBeenCalledWith({
      defaultPath: "signal.complex",
      filters: [{ name: "Complex IQ (f32)", extensions: ["complex"] }],
    });
  });

  it("uses the suggested default name and returns null on cancel", async () => {
    saveMock.mockResolvedValue(null);
    await expect(pickSaveComplexPath("gen-3.complex")).resolves.toBeNull();
    expect(saveMock).toHaveBeenCalledWith(expect.objectContaining({ defaultPath: "gen-3.complex" }));
  });
});

// --- v2 additions: project (P4) -----------------------------------------------------

describe("encodeProjectState", () => {
  it("sends the snake_case struct with dual-cased keys and tuples as arrays", () => {
    const encoded = encodeProjectState({
      version: 2,
      signals: [{ path: "/a.complex", sampleRate: 1e6 }],
      activeSignalId: "sig-1",
      analysis: [{ signalId: "sig-1", params: { modulation: "ASK", bitLength: 100 } }],
      view: [{ signalId: "sig-1", start: 0, end: 4096, mode: WaveformMode.Amplitude }],
    });
    expect(encoded).toEqual({
      version: 2,
      signals: [{ path: "/a.complex", sample_rate: 1e6, sampleRate: 1e6 }],
      active_signal_id: "sig-1",
      activeSignalId: "sig-1",
      analysis: [
        [
          "sig-1",
          {
            modulation: "ASK",
            bit_length: 100,
            bitLength: 100,
            center: null,
            noise_threshold: null,
            noiseThreshold: null,
            tolerance: null,
          },
        ],
      ],
      view: [["sig-1", 0, 4096, WaveformMode.Amplitude]],
    });
  });

  it("encodes a null active id and empty sections", () => {
    const encoded = encodeProjectState({
      version: PROJECT_VERSION,
      signals: [],
      activeSignalId: null,
      analysis: [],
      view: [],
    });
    expect(encoded.active_signal_id).toBeNull();
    expect(encoded.activeSignalId).toBeNull();
    expect(encoded.signals).toEqual([]);
    expect(encoded.analysis).toEqual([]);
    expect(encoded.view).toEqual([]);
  });
});

describe("saveProject", () => {
  it("invokes save_project with the path and the nested encoded state", async () => {
    invokeMock.mockResolvedValue(undefined);
    await saveProject("/tmp/p.urhproj", {
      version: 2,
      signals: [{ path: "/a.complex", sampleRate: 1e6 }],
      activeSignalId: null,
      analysis: [],
      view: [],
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("save_project", {
      path: "/tmp/p.urhproj",
      state: {
        version: 2,
        signals: [{ path: "/a.complex", sample_rate: 1e6, sampleRate: 1e6 }],
        active_signal_id: null,
        activeSignalId: null,
        analysis: [],
        view: [],
      },
    });
  });

  it("normalizes a write failure", async () => {
    invokeMock.mockRejectedValue({ code: "io", message: "permission denied" });
    await expect(
      saveProject("/tmp/p.urhproj", { version: 2, signals: [], activeSignalId: null, analysis: [], view: [] }),
    ).rejects.toMatchObject({ name: "IpcError", code: "io" });
  });
});

/** A complete, well-formed ProjectState in the backend's snake_case JSON. */
function projectRaw(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    version: 2,
    signals: [
      { path: "/home/u/a.complex", sample_rate: 1e6 },
      { path: "/home/u/b.complex", sample_rate: 2.4e6 },
    ],
    active_signal_id: "sig-2",
    analysis: [["sig-1", { modulation: "ASK", bit_length: 100, center: null, noise_threshold: null, tolerance: null }]],
    view: [["sig-1", 16, 4096, 2]],
    ...overrides,
  };
}

describe("loadProject", () => {
  it("invokes load_project with a dual-cased path arg and parses the state", async () => {
    invokeMock.mockResolvedValue(projectRaw());
    const state = await loadProject("/tmp/p.urhproj");
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("load_project", { path: "/tmp/p.urhproj" });
    expect(state).toEqual({
      version: 2,
      signals: [
        { path: "/home/u/a.complex", sampleRate: 1e6 },
        { path: "/home/u/b.complex", sampleRate: 2.4e6 },
      ],
      activeSignalId: "sig-2",
      analysis: [{ signalId: "sig-1", params: { modulation: "ASK", bitLength: 100 } }],
      view: [{ signalId: "sig-1", start: 16, end: 4096, mode: WaveformMode.Amplitude }],
    });
  });

  it("accepts camelCase keys and a null/missing active id defensively", async () => {
    invokeMock.mockResolvedValue({
      version: 2,
      signals: [{ path: "/a.complex", sampleRate: 1e6 }],
      activeSignalId: null,
      analysis: [["sig-1", { bitLength: 8 }]],
      view: [["sig-1", 0, 8, 1]],
    });
    const state = await loadProject("/tmp/p.urhproj");
    expect(state.activeSignalId).toBeNull();
    expect(state.analysis[0]?.params).toEqual({ bitLength: 8 });
    expect(state.view[0]?.mode).toBe(WaveformMode.Iq);
  });

  it("parses all-auto params into an empty params object", async () => {
    const state = parseProjectState(
      projectRaw({ analysis: [["sig-1", { modulation: null, bit_length: null, center: null, noise_threshold: null, tolerance: null }]] }),
    );
    expect(state.analysis[0]?.params).toEqual({});
  });

  it("normalizes a backend rejection (e.g. file-not-found)", async () => {
    invokeMock.mockRejectedValue({ code: "file-not-found", message: "no such file" });
    await expect(loadProject("/tmp/missing.urhproj")).rejects.toMatchObject({
      name: "IpcError",
      code: "file-not-found",
    });
  });
});

describe("parseProjectState validation", () => {
  it("rejects a wrong version with a clear message (older and newer)", () => {
    expect(() => parseProjectState(projectRaw({ version: 1 }))).toThrowError(/unsupported project version 1/);
    expect(() => parseProjectState(projectRaw({ version: 3 }))).toThrowError(/unsupported project version 3/);
  });

  it("rejects a non-integer or missing version", () => {
    expect(() => parseProjectState(projectRaw({ version: 2.5 }))).toThrowError(/version/);
    expect(() => parseProjectState(projectRaw({ version: undefined }))).toThrowError(/version/);
  });

  it("rejects malformed top-level shapes", () => {
    expect(() => parseProjectState(42)).toThrowError(/non-object/);
    expect(() => parseProjectState(projectRaw({ signals: {} }))).toThrowError(/signals/);
    expect(() => parseProjectState(projectRaw({ analysis: "x" }))).toThrowError(/analysis/);
    expect(() => parseProjectState(projectRaw({ view: null }))).toThrowError(/view/);
  });

  it("rejects a signal without a usable path or rate", () => {
    expect(() => parseProjectState(projectRaw({ signals: [{ path: "", sample_rate: 1e6 }] }))).toThrowError(/path/);
    expect(() => parseProjectState(projectRaw({ signals: [{ path: "/a", sample_rate: 0 }] }))).toThrowError(/sample_rate/);
    expect(() => parseProjectState(projectRaw({ signals: [{ path: "/a" }] }))).toThrowError(/sample_rate/);
  });

  it("rejects analysis entries that are not [id, params] tuples", () => {
    expect(() => parseProjectState(projectRaw({ analysis: [{ signal_id: "sig-1" }] }))).toThrowError(/tuple/);
    expect(() => parseProjectState(projectRaw({ analysis: [["sig-1"]] }))).toThrowError(/tuple/);
    expect(() => parseProjectState(projectRaw({ analysis: [["", {}]] }))).toThrowError(/signal id/);
  });

  it("rejects invalid optional param values", () => {
    expect(() => parseProjectState(projectRaw({ analysis: [["sig-1", { bit_length: 0 }]] }))).toThrowError(/bit_length/);
    expect(() => parseProjectState(projectRaw({ analysis: [["sig-1", { bit_length: 1.5 }]] }))).toThrowError(/bit_length/);
    expect(() => parseProjectState(projectRaw({ analysis: [["sig-1", { modulation: "" }]] }))).toThrowError(/modulation/);
    expect(() => parseProjectState(projectRaw({ analysis: [["sig-1", { tolerance: -1 }]] }))).toThrowError(/tolerance/);
    expect(() => parseProjectState(projectRaw({ analysis: [["sig-1", null]] }))).toThrowError(/AnalysisParams/);
  });

  it("rejects view entries with bad extents or modes", () => {
    expect(() => parseProjectState(projectRaw({ view: [["sig-1", 100, 50, 1]] }))).toThrowError(/end/);
    expect(() => parseProjectState(projectRaw({ view: [["sig-1", -1, 50, 1]] }))).toThrowError(/start/);
    expect(() => parseProjectState(projectRaw({ view: [["sig-1", 0, 50, 3]] }))).toThrowError(/mode/);
    expect(() => parseProjectState(projectRaw({ view: [["sig-1", 0, 50]] }))).toThrowError(/tuple/);
  });

  it("rejects a non-string active id", () => {
    expect(() => parseProjectState(projectRaw({ active_signal_id: 7 }))).toThrowError(/active_signal_id/);
  });
});

describe("pickOpenProjectPath / pickSaveProjectPath", () => {
  it("opens the native dialog with a .urhproj filter", async () => {
    openMock.mockResolvedValue("/home/u/p.urhproj");
    await expect(pickOpenProjectPath()).resolves.toBe("/home/u/p.urhproj");
    expect(openMock).toHaveBeenCalledWith({
      multiple: false,
      filters: [{ name: "URH project", extensions: ["urhproj"] }],
    });
  });

  it("returns null when the open dialog is cancelled", async () => {
    openMock.mockResolvedValue(null);
    await expect(pickOpenProjectPath()).resolves.toBeNull();
  });

  it("saves with a default project name and returns the path", async () => {
    saveMock.mockResolvedValue("/home/u/new.urhproj");
    await expect(pickSaveProjectPath()).resolves.toBe("/home/u/new.urhproj");
    expect(saveMock).toHaveBeenCalledWith({
      defaultPath: "project.urhproj",
      filters: [{ name: "URH project", extensions: ["urhproj"] }],
    });
  });

  it("returns null when the save dialog is cancelled", async () => {
    saveMock.mockResolvedValue(null);
    await expect(pickSaveProjectPath("demo.urhproj")).resolves.toBeNull();
    expect(saveMock).toHaveBeenCalledWith(expect.objectContaining({ defaultPath: "demo.urhproj" }));
  });
});

// --- v2 additions: simulator (P5) ---------------------------------------------------

describe("loadSimulator", () => {
  it("invokes load_simulator and parses the rule entries", async () => {
    invokeMock.mockResolvedValue([
      { index: 0, name: "Preamble", kind: "rule", summary: "1010 x 4" },
      { index: 1, name: "Send command", kind: "action", summary: "source A → destination B" },
    ]);
    const rules = await loadSimulator("/home/u/rf.simulator");
    expect(invokeMock).toHaveBeenCalledWith("load_simulator", { path: "/home/u/rf.simulator" });
    expect(rules).toEqual([
      { index: 0, name: "Preamble", kind: "rule", summary: "1010 x 4" },
      { index: 1, name: "Send command", kind: "action", summary: "source A → destination B" },
    ]);
  });

  it("normalizes a backend rejection (bad XML, missing file)", async () => {
    invokeMock.mockRejectedValue({ code: "invalid-argument", message: "malformed simulator XML" });
    await expect(loadSimulator("/bad.simulator")).rejects.toMatchObject({
      name: "IpcError",
      code: "invalid-argument",
    });
  });

  it("rejects a non-array response", async () => {
    invokeMock.mockResolvedValue({});
    await expect(loadSimulator("/x.simulator")).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });

  it("rejects rule entries with missing or mistyped fields", async () => {
    invokeMock.mockResolvedValue([{ index: 0, name: "R", kind: "rule" }]); // no summary
    await expect(loadSimulator("/x.simulator")).rejects.toMatchObject({ name: "IpcError", code: "internal" });
    invokeMock.mockResolvedValue([{ index: -1, name: "R", kind: "rule", summary: "" }]);
    await expect(loadSimulator("/x.simulator")).rejects.toMatchObject({ name: "IpcError", code: "internal" });
  });
});

describe("runSimulation", () => {
  it("invokes run_simulation with a dual-cased timeout and parses the transcript", async () => {
    invokeMock.mockResolvedValue([
      { timestamp: 0, source: "A", destination: "B", bits: "10101100", action: "send" },
      { timestamp: 0.25, source: "B", destination: "A", bits: "", action: "log" },
    ]);
    const transcript = await runSimulation("/home/u/rf.simulator", 30);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("run_simulation", {
      path: "/home/u/rf.simulator",
      timeout_secs: 30,
      timeoutSecs: 30,
    });
    expect(transcript).toEqual([
      { timestamp: 0, source: "A", destination: "B", bits: "10101100", action: "send" },
      { timestamp: 0.25, source: "B", destination: "A", bits: "", action: "log" },
    ]);
  });

  it("accepts camelCase transcript keys defensively", async () => {
    invokeMock.mockResolvedValue([{ timestamp: 1.5, source: "A", destination: "B", bits: "01", action: "send" }]);
    const transcript = await runSimulation("/x.simulator", 5);
    expect(transcript[0]).toEqual({ timestamp: 1.5, source: "A", destination: "B", bits: "01", action: "send" });
  });

  it("normalizes a timeout rejection", async () => {
    invokeMock.mockRejectedValue({ code: "timeout", message: "simulation exceeded 30 s" });
    await expect(runSimulation("/x.simulator", 30)).rejects.toMatchObject({ name: "IpcError", code: "timeout" });
  });

  it("rejects transcript entries with bad shapes", async () => {
    invokeMock.mockResolvedValue(["send"]);
    await expect(runSimulation("/x.simulator", 1)).rejects.toMatchObject({ name: "IpcError", code: "internal" });
    expect(() => parseSimTranscriptEntry({ timestamp: -0.5, source: "A", destination: "B", bits: "", action: "log" })).toThrowError(
      /timestamp/,
    );
    expect(() => parseSimTranscriptEntry({ timestamp: 1, source: "A", destination: 2, bits: "", action: "log" })).toThrowError(
      /destination/,
    );
  });
});

describe("pickSimulatorPath", () => {
  it("opens the native dialog with a .simulator filter", async () => {
    openMock.mockResolvedValue("/home/u/rf.simulator");
    await expect(pickSimulatorPath()).resolves.toBe("/home/u/rf.simulator");
    expect(openMock).toHaveBeenCalledWith({
      multiple: false,
      filters: [{ name: "URH simulator", extensions: ["simulator"] }],
    });
  });

  it("returns null when cancelled", async () => {
    openMock.mockResolvedValue(null);
    await expect(pickSimulatorPath()).resolves.toBeNull();
  });
});
