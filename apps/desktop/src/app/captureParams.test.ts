/**
 * Capture parameter validation tests (P3): the pure parseCaptureFields —
 * device availability gating, numeric field rules, and the rtl-tcp-only
 * host/port requirements. These all run before any IPC happens.
 */

import { describe, expect, it } from "vitest";

import type { DeviceInfo } from "../bridge/api";
import { type CaptureParamFields, DEFAULT_CAPTURE_FIELDS, RTL_TCP_DEVICE, parseCaptureFields } from "./useCapture";

const DEVICES: DeviceInfo[] = [
  { name: "mock", backend: "mock", available: true, detail: "" },
  { name: "rtl-tcp", backend: "rtltcp", available: true, detail: "" },
  { name: "borked", backend: "soapy", available: false, detail: "device busy: another process holds it" },
  { name: "ghost", backend: "soapy", available: false, detail: "" },
];

function fieldsWith(overrides: Partial<CaptureParamFields>): CaptureParamFields {
  return { ...DEFAULT_CAPTURE_FIELDS, device: "mock", ...overrides };
}

describe("parseCaptureFields — valid configs", () => {
  it("parses the default fields for an available device", () => {
    const parsed = parseCaptureFields({ ...DEFAULT_CAPTURE_FIELDS, device: "mock" }, DEVICES);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.config).toEqual({
      device: "mock",
      sampleRate: 1e6,
      frequency: 433.92e6,
      bandwidth: 1e6,
      gain: 20,
      record: true,
      host: null,
      port: null,
      identifier: null,
    });
  });

  it("parses scientific notation and digit separators", () => {
    const parsed = parseCaptureFields(
      fieldsWith({ frequency: "433_920_000", sampleRate: "2.5e6", bandwidth: "800,000", gain: "32.5" }),
      DEVICES,
    );
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.config.frequency).toBe(433920000);
    expect(parsed.config.sampleRate).toBe(2.5e6);
    expect(parsed.config.bandwidth).toBe(800000);
    expect(parsed.config.gain).toBe(32.5);
  });

  it("requires host and port only for rtl-tcp and passes them through", () => {
    const parsed = parseCaptureFields(
      fieldsWith({ device: "rtl-tcp", host: "192.168.1.5", port: "1234" }),
      DEVICES,
    );
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.config.host).toBe("192.168.1.5");
    expect(parsed.config.port).toBe(1234);
  });

  it("trims the device name and host", () => {
    const parsed = parseCaptureFields(fieldsWith({ device: "  mock  " }), DEVICES);
    expect(parsed.ok).toBe(true);
    const rtl = parseCaptureFields(
      fieldsWith({ device: RTL_TCP_DEVICE, host: "  localhost " }),
      DEVICES,
    );
    expect(rtl.ok).toBe(true);
    if (!rtl.ok || !parsed.ok) return;
    expect(parsed.config.device).toBe("mock");
    expect(rtl.config.host).toBe("localhost");
  });

  it("passes the record flag through both ways", () => {
    for (const record of [true, false]) {
      const parsed = parseCaptureFields(fieldsWith({ record }), DEVICES);
      expect(parsed.ok && parsed.config.record).toBe(record);
    }
  });

  it("accepts zero frequency, bandwidth and gain (finite, non-negative)", () => {
    const parsed = parseCaptureFields(fieldsWith({ frequency: "0", bandwidth: "0", gain: "0" }), DEVICES);
    expect(parsed.ok).toBe(true);
  });
});

describe("parseCaptureFields — device errors", () => {
  it("rejects no selection", () => {
    const parsed = parseCaptureFields({ ...DEFAULT_CAPTURE_FIELDS, device: "" }, DEVICES);
    expect(parsed).toMatchObject({ ok: false, field: "device" });
  });

  it("rejects an unknown device", () => {
    const parsed = parseCaptureFields(fieldsWith({ device: "hackrf" }), DEVICES);
    expect(parsed).toMatchObject({ ok: false, field: "device" });
    if (parsed.ok) return;
    expect(parsed.message).toContain("hackrf");
  });

  it("rejects an unavailable device, surfacing its detail as the message", () => {
    const parsed = parseCaptureFields(fieldsWith({ device: "borked" }), DEVICES);
    expect(parsed).toMatchObject({ ok: false, field: "device" });
    if (parsed.ok) return;
    expect(parsed.message).toBe("device busy: another process holds it");
  });

  it("falls back to a generic message when the detail is empty", () => {
    const parsed = parseCaptureFields(fieldsWith({ device: "ghost" }), DEVICES);
    expect(parsed).toMatchObject({ ok: false, field: "device" });
    if (parsed.ok) return;
    expect(parsed.message).toContain("unavailable");
  });
});

describe("parseCaptureFields — numeric field errors", () => {
  it("rejects a non-positive or non-numeric sample rate", () => {
    for (const bad of ["0", "-1e6", "abc", ""]) {
      const parsed = parseCaptureFields(fieldsWith({ sampleRate: bad }), DEVICES);
      expect(parsed).toMatchObject({ ok: false, field: "sampleRate" });
    }
  });

  it("rejects negative or non-numeric frequency/bandwidth/gain", () => {
    for (const field of ["frequency", "bandwidth", "gain"] as const) {
      for (const bad of ["-1", "NaN", "x"]) {
        const parsed = parseCaptureFields(fieldsWith({ [field]: bad }), DEVICES);
        expect(parsed).toMatchObject({ ok: false, field });
      }
    }
  });
});

describe("parseCaptureFields — rtl-tcp host/port", () => {
  it("rejects a missing host", () => {
    const parsed = parseCaptureFields(fieldsWith({ device: RTL_TCP_DEVICE, host: "  " }), DEVICES);
    expect(parsed).toMatchObject({ ok: false, field: "host" });
  });

  it("rejects ports outside 1–65535 and non-integers", () => {
    for (const bad of ["0", "65536", "-1", "1.5", "http"]) {
      const parsed = parseCaptureFields(
        fieldsWith({ device: RTL_TCP_DEVICE, host: "h", port: bad }),
        DEVICES,
      );
      expect(parsed).toMatchObject({ ok: false, field: "port" });
    }
  });

  it("ignores host/port fields for other devices", () => {
    const parsed = parseCaptureFields(
      fieldsWith({ device: "mock", host: "", port: "not-a-port" }),
      DEVICES,
    );
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.config.host).toBeNull();
    expect(parsed.config.port).toBeNull();
  });
});
