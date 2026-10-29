/**
 * Generator parameter validation tests (P4): the pure parseGeneratorFields —
 * modulation canonicalization, the [01]* bits rule, integer/center field
 * validation. Everything runs before any IPC happens.
 */

import { describe, expect, it } from "vitest";

import { MODULATIONS } from "../bridge/api";
import { type GeneratorFields, DEFAULT_GENERATOR_FIELDS, parseGeneratorFields } from "./useGenerator";

function fieldsWith(overrides: Partial<GeneratorFields>): GeneratorFields {
  return { ...DEFAULT_GENERATOR_FIELDS, ...overrides };
}

describe("parseGeneratorFields — valid params", () => {
  it("parses the defaults", () => {
    const parsed = parseGeneratorFields(DEFAULT_GENERATOR_FIELDS);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;
    expect(parsed.params).toEqual({
      modulation: "ASK",
      bits: "10101100",
      samplesPerBit: 100,
      pauseSamples: 1000,
      center: 0.5,
    });
  });

  it("accepts every advertised modulation, canonicalizing case", () => {
    expect(MODULATIONS).toEqual(["ASK", "FSK", "PSK", "GFSK", "OQPSK"]);
    for (const m of MODULATIONS) {
      const shuffled = m.toLowerCase();
      const parsed = parseGeneratorFields(fieldsWith({ modulation: shuffled }));
      expect(parsed.ok && parsed.params.modulation).toBe(m);
    }
    const mixed = parseGeneratorFields(fieldsWith({ modulation: "aSk" }));
    expect(mixed.ok && mixed.params.modulation).toBe("ASK");
  });

  it("trims the bits string", () => {
    const parsed = parseGeneratorFields(fieldsWith({ bits: " 0101 " }));
    expect(parsed.ok && parsed.params.bits).toBe("0101");
  });

  it("accepts any finite center and zero pause", () => {
    for (const center of ["0", "-0.25", "1e3", "0.500001"]) {
      const parsed = parseGeneratorFields(fieldsWith({ center }));
      expect(parsed.ok).toBe(true);
    }
    const zeroPause = parseGeneratorFields(fieldsWith({ pauseSamples: "0" }));
    expect(zeroPause.ok && zeroPause.params.pauseSamples).toBe(0);
  });

  it("parses digit-separated integers", () => {
    const parsed = parseGeneratorFields(fieldsWith({ samplesPerBit: "1_000", pauseSamples: "2,500" }));
    expect(parsed.ok && parsed.params.samplesPerBit).toBe(1000);
    expect(parsed.ok && parsed.params.pauseSamples).toBe(2500);
  });
});

describe("parseGeneratorFields — errors", () => {
  it("rejects an empty or unknown modulation", () => {
    expect(parseGeneratorFields(fieldsWith({ modulation: "" }))).toMatchObject({ ok: false, field: "modulation" });
    expect(parseGeneratorFields(fieldsWith({ modulation: "QAM" }))).toMatchObject({ ok: false, field: "modulation" });
  });

  it("rejects bits that are not a non-empty [01] string", () => {
    for (const bad of ["", "0112", "01 01", "abc", "0x01"]) {
      const parsed = parseGeneratorFields(fieldsWith({ bits: bad }));
      expect(parsed).toMatchObject({ ok: false, field: "bits" });
    }
  });

  it("rejects a non-positive or non-integer samples_per_bit", () => {
    for (const bad of ["0", "-5", "1.5", "x"]) {
      const parsed = parseGeneratorFields(fieldsWith({ samplesPerBit: bad }));
      expect(parsed).toMatchObject({ ok: false, field: "samplesPerBit" });
    }
  });

  it("rejects a negative or non-integer pause", () => {
    for (const bad of ["-1", "0.5", "later"]) {
      const parsed = parseGeneratorFields(fieldsWith({ pauseSamples: bad }));
      expect(parsed).toMatchObject({ ok: false, field: "pauseSamples" });
    }
  });

  it("rejects a non-numeric center", () => {
    const parsed = parseGeneratorFields(fieldsWith({ center: "mid" }));
    expect(parsed).toMatchObject({ ok: false, field: "center" });
  });
});
