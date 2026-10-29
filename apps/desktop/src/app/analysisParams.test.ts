/**
 * Analysis parameter parsing tests (P2): the pure editor-field →
 * AnalysisParams validation used by the run button, plus the
 * EstimatedParams → AnalysisParams conversion AWRE relies on.
 */

import { describe, expect, it } from "vitest";

import { EMPTY_PARAM_FIELDS, estimatedToParams, parseParamFields, type ParamFields } from "./useAnalysis";

function fields(overrides: Partial<ParamFields>): ParamFields {
  return { ...EMPTY_PARAM_FIELDS, ...overrides };
}

describe("parseParamFields", () => {
  it("treats all-empty fields as fully auto", () => {
    const parsed = parseParamFields(EMPTY_PARAM_FIELDS);
    expect(parsed).toEqual({ ok: true, params: {} });
  });

  it("accepts each modulation case-insensitively and canonicalizes it", () => {
    expect(parseParamFields(fields({ modulation: "FSK" }))).toEqual({ ok: true, params: { modulation: "FSK" } });
    expect(parseParamFields(fields({ modulation: "ask" }))).toEqual({ ok: true, params: { modulation: "ASK" } });
    expect(parseParamFields(fields({ modulation: "OQPSK" }))).toEqual({ ok: true, params: { modulation: "OQPSK" } });
  });

  it("rejects an unknown modulation", () => {
    const parsed = parseParamFields(fields({ modulation: "QAM" }));
    expect(parsed).toMatchObject({ ok: false, field: "modulation" });
  });

  it("accepts valid numeric fields", () => {
    const parsed = parseParamFields(fields({ bitLength: "100", center: "-0.02", noiseThreshold: "0.001", tolerance: "5" }));
    expect(parsed).toEqual({
      ok: true,
      params: { bitLength: 100, center: -0.02, noiseThreshold: 0.001, tolerance: 5 },
    });
  });

  it("tolerates digit separators like the rate input", () => {
    expect(parseParamFields(fields({ bitLength: "1_000" }))).toEqual({ ok: true, params: { bitLength: 1000 } });
  });

  it("rejects non-numeric and non-integer bit lengths", () => {
    expect(parseParamFields(fields({ bitLength: "abc" }))).toMatchObject({ ok: false, field: "bitLength" });
    expect(parseParamFields(fields({ bitLength: "1.5" }))).toMatchObject({ ok: false, field: "bitLength" });
    expect(parseParamFields(fields({ bitLength: "0" }))).toMatchObject({ ok: false, field: "bitLength" });
    expect(parseParamFields(fields({ bitLength: "-3" }))).toMatchObject({ ok: false, field: "bitLength" });
  });

  it("allows tolerance 0 but not negative or fractional", () => {
    expect(parseParamFields(fields({ tolerance: "0" }))).toEqual({ ok: true, params: { tolerance: 0 } });
    expect(parseParamFields(fields({ tolerance: "-1" }))).toMatchObject({ ok: false, field: "tolerance" });
    expect(parseParamFields(fields({ tolerance: "0.5" }))).toMatchObject({ ok: false, field: "tolerance" });
  });

  it("allows any finite center/noise values, rejects the rest", () => {
    expect(parseParamFields(fields({ center: "0", noiseThreshold: "-1e-9" }))).toEqual({
      ok: true,
      params: { center: 0, noiseThreshold: -1e-9 },
    });
    expect(parseParamFields(fields({ center: "x" }))).toMatchObject({ ok: false, field: "center" });
    expect(parseParamFields(fields({ noiseThreshold: "NaN" }))).toMatchObject({ ok: false, field: "noiseThreshold" });
  });

  it("reports only the first invalid field", () => {
    const parsed = parseParamFields(fields({ bitLength: "x", tolerance: "y" }));
    expect(parsed).toMatchObject({ ok: false, field: "bitLength" });
  });
});

describe("estimatedToParams", () => {
  it("resolves every field (no autos left) for the AWRE call", () => {
    expect(
      estimatedToParams({ modulation: "ASK", bitLength: 100, center: 0.01, noiseThreshold: 0.001, tolerance: 5 }),
    ).toEqual({
      modulation: "ASK",
      bitLength: 100,
      center: 0.01,
      noiseThreshold: 0.001,
      tolerance: 5,
    });
  });
});
