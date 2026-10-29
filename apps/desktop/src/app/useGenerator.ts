/**
 * Generator view model (P4): form fields, validation and the generate /
 * save-as actions. IPC-thin like useAnalysis — validation is the exported
 * pure {@link parseGeneratorFields} (unit-tested), backend responses are
 * parsed in the bridge; this hook sequences them and reports the produced
 * signal to the app (which refreshes the signal list and activates it).
 *
 * "Save as .complex" saves the *active* signal — after Generate that is the
 * freshly produced `gen-<n>` signal, and it equally covers recorded `rec-<n>`
 * signals (the contract offers the action for generated signals; saving any
 * open signal through the same command is strictly more useful).
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type GeneratorParams,
  type SignalMeta,
  MODULATIONS,
  generate,
  pickSaveComplexPath,
  saveSignalAs,
} from "../bridge/api";
import { type ErrorState, toErrorState } from "./errors";

/** Editable generator fields as text; numbers are parsed on generate. */
export interface GeneratorFields {
  modulation: string;
  bits: string;
  samplesPerBit: string;
  pauseSamples: string;
  center: string;
}

export const DEFAULT_GENERATOR_FIELDS: GeneratorFields = {
  modulation: "ASK",
  bits: "10101100",
  samplesPerBit: "100",
  pauseSamples: "1000",
  center: "0.5",
};

export type GeneratorField = keyof GeneratorFields;

export type ParseGeneratorResult =
  | { ok: true; params: GeneratorParams }
  | { ok: false; field: GeneratorField; message: string };

/** Strip digit separators the rate input also tolerates. */
function parseNumberText(text: string): number {
  return Number.parseFloat(text.replace(/[,\s_]/g, ""));
}

/**
 * Validate the generator fields into {@link GeneratorParams}. `bits` must be
 * a non-empty `[01]` string (spaces are not tolerated — the field is a bit
 * string, not prose); `samples_per_bit` a positive integer, `pause_samples`
 * a non-negative integer, `center` any finite number; modulation matches
 * {@link MODULATIONS} case-insensitively and is canonicalized.
 */
export function parseGeneratorFields(fields: GeneratorFields): ParseGeneratorResult {
  const modulation = fields.modulation.trim();
  if (modulation === "") {
    return { ok: false, field: "modulation", message: "pick a modulation" };
  }
  const match = MODULATIONS.find((m) => m.toLowerCase() === modulation.toLowerCase());
  if (match === undefined) {
    return { ok: false, field: "modulation", message: `unknown modulation "${modulation}"` };
  }

  const bits = fields.bits.trim();
  if (!/^[01]+$/.test(bits)) {
    return { ok: false, field: "bits", message: "bits must be a non-empty string of 0s and 1s" };
  }

  const samplesPerBit = parseNumberText(fields.samplesPerBit);
  if (!Number.isFinite(samplesPerBit) || !Number.isInteger(samplesPerBit) || samplesPerBit < 1) {
    return { ok: false, field: "samplesPerBit", message: `"${fields.samplesPerBit}" is not a positive integer` };
  }

  const pauseSamples = parseNumberText(fields.pauseSamples);
  if (!Number.isFinite(pauseSamples) || !Number.isInteger(pauseSamples) || pauseSamples < 0) {
    return { ok: false, field: "pauseSamples", message: `"${fields.pauseSamples}" is not a non-negative integer` };
  }

  const center = parseNumberText(fields.center);
  if (!Number.isFinite(center)) {
    return { ok: false, field: "center", message: `"${fields.center}" is not a number` };
  }

  return { ok: true, params: { modulation: match, bits, samplesPerBit, pauseSamples, center } };
}

export interface UseGeneratorOptions {
  /** The signal "Save as .complex" writes (null = nothing to save). */
  getActiveSignalId: () => string | null;
  /** A generated signal was registered and should become the active one. */
  onGenerated: (meta: SignalMeta) => void;
}

export function useGenerator(options: UseGeneratorOptions) {
  const [fields, setFields] = useState<GeneratorFields>(DEFAULT_GENERATOR_FIELDS);
  const [busy, setBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ErrorState | null>(null);
  const [paramError, setParamError] = useState<string | null>(null);
  const [generated, setGenerated] = useState<SignalMeta | null>(null);
  const [savedPath, setSavedPath] = useState<string | null>(null);

  // Mirrors so the async actions always read the latest values.
  const fieldsRef = useRef(fields);
  const busyRef = useRef(false);
  const savingRef = useRef(false);
  const getActiveSignalIdRef = useRef(options.getActiveSignalId);
  const onGeneratedRef = useRef(options.onGenerated);

  useEffect(() => {
    fieldsRef.current = fields;
    getActiveSignalIdRef.current = options.getActiveSignalId;
    onGeneratedRef.current = options.onGenerated;
  }, [fields, options.getActiveSignalId, options.onGenerated]);

  // --- actions -----------------------------------------------------------------

  /** Update one field; clears the last validation error. */
  const setField = useCallback((field: GeneratorField, value: string): void => {
    setFields((prev) => ({ ...prev, [field]: value }));
    setParamError(null);
  }, []);

  /** Generate a `gen-<n>` signal; invalid fields never reach IPC. */
  const run = useCallback(async (): Promise<void> => {
    if (busyRef.current) return;
    const parsed = parseGeneratorFields(fieldsRef.current);
    if (!parsed.ok) {
      setParamError(`${parsed.field}: ${parsed.message}`);
      return;
    }
    setParamError(null);
    setError(null);
    setSavedPath(null);
    busyRef.current = true;
    setBusy(true);
    try {
      const meta = await generate(parsed.params);
      setGenerated(meta);
      onGeneratedRef.current(meta);
    } catch (e) {
      setError(toErrorState(e));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }, []);

  /** Save the active signal via the native save dialog; cancel = no-op. */
  const saveAs = useCallback(async (): Promise<void> => {
    if (savingRef.current) return;
    const signalId = getActiveSignalIdRef.current();
    if (signalId === null) return;
    setError(null);
    setSavedPath(null);
    savingRef.current = true;
    setSaving(true);
    try {
      const path = await pickSaveComplexPath();
      if (path === null) return; // dialog cancelled
      await saveSignalAs(signalId, path);
      setSavedPath(path);
    } catch (e) {
      setError(toErrorState(e));
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  }, []);

  return {
    // state
    fields,
    busy,
    saving,
    error,
    paramError,
    generated,
    savedPath,
    // actions
    setField,
    run,
    saveAs,
  };
}

export type GeneratorApi = ReturnType<typeof useGenerator>;
