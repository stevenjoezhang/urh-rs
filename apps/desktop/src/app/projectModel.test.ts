/**
 * Project view-model tests (P4): buildProjectState's save collection (signal
 * filtering by empty path, param/view collection, active-id handling) and the
 * load resolution rules (active signal, entry picking, params→fields).
 */

import { describe, expect, it } from "vitest";

import type { AnalysisParams, SignalMeta } from "../bridge/api";
import { WaveformMode } from "../bridge/protocol";
import {
  type SignalViewState,
  buildProjectState,
  hasAnyParam,
  paramsToFields,
  pickProjectEntry,
  resolveActiveSignal,
} from "./projectModel";

function meta(overrides: Partial<SignalMeta>): SignalMeta {
  return {
    signal_id: "sig-1",
    path: "/a.complex",
    display_name: "a.complex",
    format: "complex",
    sample_count: 1000,
    sample_rate: 1e6,
    duration_secs: 0.001,
    ...overrides,
  };
}

const FILE_A = meta({ signal_id: "sig-1", path: "/a.complex", sample_rate: 1e6 });
const FILE_B = meta({ signal_id: "sig-4", path: "/b.complex", sample_rate: 2.4e6 });
/** In-memory signals (recordings / generated) — empty path, not reopenable. */
const REC = meta({ signal_id: "rec-2", path: "", display_name: "", sample_rate: 1e6 });
const GEN = meta({ signal_id: "gen-3", path: "", display_name: "", sample_rate: 1e6 });

describe("buildProjectState", () => {
  it("keeps only file-backed signals (skips rec-*/gen-* with empty paths)", () => {
    const state = buildProjectState({
      signals: [FILE_A, REC, GEN, FILE_B],
      activeSignalId: null,
      analysisBySignal: {},
      viewBySignal: {},
    });
    expect(state.signals).toEqual([
      { path: "/a.complex", sampleRate: 1e6 },
      { path: "/b.complex", sampleRate: 2.4e6 },
    ]);
    expect(state.version).toBe(2);
  });

  it("collects analysis params only for kept signals that have them", () => {
    const params: AnalysisParams = { modulation: "ASK", bitLength: 100 };
    const state = buildProjectState({
      signals: [FILE_A, FILE_B, REC],
      activeSignalId: "sig-1",
      // sig-4 has all-auto params, rec-2 is skipped despite having params
      analysisBySignal: { "sig-1": params, "sig-4": {}, "rec-2": { tolerance: 3 } },
      viewBySignal: {},
    });
    expect(state.analysis).toEqual([{ signalId: "sig-1", params }]);
  });

  it("collects view entries for kept signals in signals order", () => {
    const va: SignalViewState = { start: 16, end: 512, mode: WaveformMode.Amplitude };
    const vb: SignalViewState = { start: 0, end: 1000, mode: WaveformMode.Iq };
    const state = buildProjectState({
      signals: [FILE_A, REC, FILE_B],
      activeSignalId: null,
      analysisBySignal: {},
      viewBySignal: { "sig-4": vb, "sig-1": va },
    });
    expect(state.view).toEqual([
      { signalId: "sig-1", start: 16, end: 512, mode: WaveformMode.Amplitude },
      { signalId: "sig-4", start: 0, end: 1000, mode: WaveformMode.Iq },
    ]);
  });

  it("nulls the active id when it belongs to an in-memory signal or nothing", () => {
    for (const activeSignalId of ["rec-2", "gen-3", "sig-99", null]) {
      const state = buildProjectState({
        signals: [FILE_A, REC],
        activeSignalId,
        analysisBySignal: {},
        viewBySignal: {},
      });
      expect(state.activeSignalId).toBeNull();
    }
  });

  it("keeps the active id when the active signal is file-backed", () => {
    const state = buildProjectState({
      signals: [FILE_A, REC, FILE_B],
      activeSignalId: "sig-4",
      analysisBySignal: {},
      viewBySignal: {},
    });
    expect(state.activeSignalId).toBe("sig-4");
  });

  it("emits an empty but well-formed project when nothing is savable", () => {
    const state = buildProjectState({
      signals: [REC, GEN],
      activeSignalId: "rec-2",
      analysisBySignal: { "rec-2": { modulation: "FSK" } },
      viewBySignal: { "rec-2": { start: 0, end: 10, mode: WaveformMode.Iq } },
    });
    expect(state).toEqual({ version: 2, signals: [], activeSignalId: null, analysis: [], view: [] });
  });
});

describe("hasAnyParam", () => {
  it("is false only for the fully-auto params object", () => {
    expect(hasAnyParam({})).toBe(false);
    expect(hasAnyParam({ modulation: undefined, bitLength: undefined })).toBe(false);
    expect(hasAnyParam({ tolerance: 0 })).toBe(true);
    expect(hasAnyParam({ center: 0 })).toBe(true);
    expect(hasAnyParam({ modulation: "ASK" })).toBe(true);
  });
});

describe("resolveActiveSignal", () => {
  const opened = [meta({ signal_id: "sig-7", path: "/a.complex" }), meta({ signal_id: "sig-8", path: "/b.complex" })];

  it("prefers the saved active id when it matches a re-opened signal", () => {
    expect(resolveActiveSignal({ ...project, activeSignalId: "sig-8" }, opened)?.signal_id).toBe("sig-8");
  });

  it("falls back to the first opened signal when the id matches nothing", () => {
    expect(resolveActiveSignal({ ...project, activeSignalId: "sig-2" }, opened)?.signal_id).toBe("sig-7");
    expect(resolveActiveSignal({ ...project, activeSignalId: null }, opened)?.signal_id).toBe("sig-7");
  });

  it("returns null when nothing was opened", () => {
    expect(resolveActiveSignal({ ...project, activeSignalId: "sig-7" }, [])).toBeNull();
  });
});

const project = buildProjectState({
  signals: [FILE_A],
  activeSignalId: "sig-1",
  analysisBySignal: {},
  viewBySignal: {},
});

describe("pickProjectEntry", () => {
  const opened = [meta({ signal_id: "sig-7", path: "/a.complex" })];
  const target = opened[0];

  it("returns the entry whose id matches the activated signal", () => {
    const entries = [{ signalId: "sig-9", params: {} }, { signalId: "sig-7", params: {} }];
    expect(pickProjectEntry(entries, target, opened)?.signalId).toBe("sig-7");
  });

  it("falls back to the first entry when no entry id matches any opened signal", () => {
    const entries = [{ signalId: "sig-1", params: {} }];
    expect(pickProjectEntry(entries, target, opened)?.signalId).toBe("sig-1");
  });

  it("returns null when another entry did match (ids are meaningful, just not ours)", () => {
    const openedBoth = [meta({ signal_id: "sig-7", path: "/a.complex" }), meta({ signal_id: "sig-8", path: "/b.complex" })];
    const entries = [{ signalId: "sig-8", params: {} }];
    expect(pickProjectEntry(entries, target, openedBoth)).toBeNull();
  });

  it("returns null for no target or no entries", () => {
    expect(pickProjectEntry([{ signalId: "sig-7" }], null, opened)).toBeNull();
    expect(pickProjectEntry([], target, opened)).toBeNull();
  });
});

describe("paramsToFields", () => {
  it("renders every pinned field and leaves auto fields empty", () => {
    expect(
      paramsToFields({ modulation: "FSK", bitLength: 100, center: -0.02, noiseThreshold: 0.001, tolerance: 5 }),
    ).toEqual({ modulation: "FSK", bitLength: "100", center: "-0.02", noiseThreshold: "0.001", tolerance: "5" });
  });

  it("renders fully-auto params as the empty editor state", () => {
    expect(paramsToFields({})).toEqual({
      modulation: "",
      bitLength: "",
      center: "",
      noiseThreshold: "",
      tolerance: "",
    });
  });

  it("round-trips through useAnalysis's parseParamFields", async () => {
    const { parseParamFields } = await import("./useAnalysis");
    const params: AnalysisParams = { modulation: "ASK", bitLength: 42, center: 0.5, noiseThreshold: 0.01, tolerance: 2 };
    expect(parseParamFields(paramsToFields(params))).toEqual({ ok: true, params });
  });
});
