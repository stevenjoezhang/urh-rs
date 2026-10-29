/**
 * Protocol analysis view model (P2): parameter fields, the analysis job
 * state machine (idle → running → done/error) and the AWRE label job.
 *
 * The hook is deliberately IPC-thin — all parsing/validation of user input
 * happens in the exported pure {@link parseParamFields} (unit-tested), and
 * all parsing of backend responses happens in the bridge (also tested). The
 * state machine only sequences them and guards against stale settles: a
 * result is applied only when it still belongs to the current job of the
 * current signal.
 *
 * One job at a time per panel (the contract's `job_id` identifies it;
 * `cancel_analysis` makes the promise reject with code `"cancelled"`, which
 * lands back in `idle` rather than the error state). Job ids come from a
 * counter that never resets, so a job started after a signal switch can
 * never collide with a still-running job from before it.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type AnalysisParams,
  type AnalysisResult,
  type EstimatedParams,
  type FieldLabel,
  type SampleRange,
  type SignalMeta,
  MODULATIONS,
  analyzeSignal,
  cancelAnalysis,
  runAwre,
} from "../bridge/api";
import { type ErrorState, toErrorState } from "./errors";

/** Editable parameter fields as text; `""` means "auto" for every field. */
export interface ParamFields {
  modulation: string;
  bitLength: string;
  center: string;
  noiseThreshold: string;
  tolerance: string;
}

export const EMPTY_PARAM_FIELDS: ParamFields = {
  modulation: "",
  bitLength: "",
  center: "",
  noiseThreshold: "",
  tolerance: "",
};

export type ParamFieldName = keyof ParamFields;

export type ParseParamsResult =
  | { ok: true; params: AnalysisParams }
  | { ok: false; field: ParamFieldName; message: string };

/** Strip digit separators the rate input also tolerates. */
function parseNumberText(text: string): number {
  return Number.parseFloat(text.replace(/[,\s_]/g, ""));
}

/**
 * Validate the editor fields into {@link AnalysisParams}. Empty string =
 * auto (field omitted). Modulation matches {@link MODULATIONS}
 * case-insensitively; `bit_length` must be a positive integer, `tolerance` a
 * non-negative integer, `center`/`noise_threshold` any finite number.
 */
export function parseParamFields(fields: ParamFields): ParseParamsResult {
  const params: AnalysisParams = {};

  const modulation = fields.modulation.trim();
  if (modulation !== "") {
    const match = MODULATIONS.find((m) => m.toLowerCase() === modulation.toLowerCase());
    if (match === undefined) {
      return { ok: false, field: "modulation", message: `unknown modulation "${modulation}"` };
    }
    params.modulation = match;
  }

  const bitLength = fields.bitLength.trim();
  if (bitLength !== "") {
    const v = parseNumberText(bitLength);
    if (!Number.isFinite(v) || !Number.isInteger(v) || v < 1) {
      return { ok: false, field: "bitLength", message: `"${bitLength}" is not a positive integer` };
    }
    params.bitLength = v;
  }

  for (const field of ["center", "noiseThreshold"] as const) {
    const text = fields[field].trim();
    if (text === "") continue;
    const v = parseNumberText(text);
    if (!Number.isFinite(v)) {
      return { ok: false, field, message: `"${text}" is not a number` };
    }
    params[field] = v;
  }

  const tolerance = fields.tolerance.trim();
  if (tolerance !== "") {
    const v = parseNumberText(tolerance);
    if (!Number.isFinite(v) || !Number.isInteger(v) || v < 0) {
      return { ok: false, field: "tolerance", message: `"${tolerance}" is not a non-negative integer` };
    }
    params.tolerance = v;
  }

  return { ok: true, params };
}

/** The analysis params AWRE should run with: the ones the finished job used. */
export function estimatedToParams(e: EstimatedParams): AnalysisParams {
  return {
    modulation: e.modulation,
    bitLength: e.bitLength,
    center: e.center,
    noiseThreshold: e.noiseThreshold,
    tolerance: e.tolerance,
  };
}

export type AnalysisJobState =
  | { kind: "idle" }
  | { kind: "running"; jobId: number }
  | { kind: "done"; result: AnalysisResult; range: SampleRange | null }
  | { kind: "error"; error: ErrorState };

export type AwreState =
  | { kind: "idle" }
  | { kind: "running" }
  | { kind: "done"; labels: FieldLabel[] }
  | { kind: "error"; error: ErrorState };

export function useAnalysis(meta: SignalMeta | null) {
  const [fields, setFields] = useState<ParamFields>(EMPTY_PARAM_FIELDS);
  const [selectionOnly, setSelectionOnly] = useState(false);
  const [job, setJob] = useState<AnalysisJobState>({ kind: "idle" });
  const [awre, setAwre] = useState<AwreState>({ kind: "idle" });
  const [paramError, setParamError] = useState<string | null>(null);

  // Mirrors so the async actions always read the latest values.
  const metaRef = useRef<SignalMeta | null>(null);
  const fieldsRef = useRef(fields);
  /** job currently in flight, null when none. */
  const jobRef = useRef<number | null>(null);
  /** ever-increasing job-id source; never reset, so ids stay unique. */
  const jobSeqRef = useRef(0);
  /** AWRE request in flight. */
  const awreBusyRef = useRef(false);
  /** ever-increasing AWRE request source; settles from superseded requests drop. */
  const awreSeqRef = useRef(0);
  /** range+params of the last finished analysis, for AWRE. */
  const lastDoneRef = useRef<{ range: SampleRange | null; params: AnalysisParams } | null>(null);

  useEffect(() => {
    fieldsRef.current = fields;
  }, [fields]);

  useEffect(() => {
    metaRef.current = meta;
  }, [meta]);

  // Reset the analysis when the *signal* changes (not on meta refreshes like
  // a sample-rate update): a result from another signal must never survive
  // into the new one, and a running job for the old one is cancelled
  // fire-and-forget — its settle is dropped via the jobRef guard.
  const signalId = meta?.signal_id ?? null;
  useEffect(() => {
    if (signalId === null) {
      lastDoneRef.current = null;
      setJob({ kind: "idle" });
      setAwre({ kind: "idle" });
      setParamError(null);
      return;
    }
    return () => {
      const running = jobRef.current;
      jobRef.current = null;
      awreBusyRef.current = false;
      lastDoneRef.current = null;
      setJob({ kind: "idle" });
      setAwre({ kind: "idle" });
      setParamError(null);
      // Best-effort cancel of a job that outlived its signal; failure is
      // surfaced by the analyze() rejection, not here.
      if (running !== null) void cancelAnalysis(running).catch(() => undefined);
    };
  }, [signalId]);

  // --- actions -----------------------------------------------------------------

  /** Update one parameter field (text; `""` = auto). Clears the last error. */
  const setField = useCallback((field: ParamFieldName, value: string): void => {
    setFields((prev) => ({ ...prev, [field]: value }));
    setParamError(null);
  }, []);

  /** Replace all parameter fields at once (project load). Clears the last error. */
  const applyFields = useCallback((next: ParamFields): void => {
    setFields(next);
    setParamError(null);
  }, []);

  /** Fill the editor with the finished job's estimated params (for tweaking). */
  const applyEstimatedToFields = useCallback((): void => {
    const state = job;
    if (state.kind !== "done") return;
    const e = state.result.params;
    setFields({
      modulation: e.modulation,
      bitLength: String(e.bitLength),
      center: String(e.center),
      noiseThreshold: String(e.noiseThreshold),
      tolerance: String(e.tolerance),
    });
    setParamError(null);
  }, [job]);

  /**
   * Run an analysis. `range` is the already-resolved analysis range: the
   * selection interval when "selection only" is on, else null (whole
   * signal). Invalid fields surface as `paramError` and never reach IPC.
   */
  const run = useCallback(async (range: SampleRange | null): Promise<void> => {
    const m = metaRef.current;
    if (m === null || jobRef.current !== null) return;
    const parsed = parseParamFields(fieldsRef.current);
    if (!parsed.ok) {
      setParamError(`${parsed.field}: ${parsed.message}`);
      return;
    }
    setParamError(null);
    setAwre({ kind: "idle" }); // labels belong to the previous result
    const jobId = ++jobSeqRef.current;
    jobRef.current = jobId;
    setJob({ kind: "running", jobId });
    try {
      const result = await analyzeSignal(m.signal_id, range, parsed.params, jobId);
      if (jobRef.current !== jobId) return; // superseded (signal changed)
      lastDoneRef.current = { range, params: parsed.params };
      setJob({ kind: "done", result, range });
    } catch (e) {
      if (jobRef.current !== jobId) return;
      const err = toErrorState(e);
      setJob(err.code === "cancelled" ? { kind: "idle" } : { kind: "error", error: err });
    } finally {
      if (jobRef.current === jobId) jobRef.current = null;
    }
  }, []);

  /** Cancel the running job (idempotent; harmless when none). */
  const cancel = useCallback(async (): Promise<void> => {
    const jobId = jobRef.current;
    if (jobId === null) return;
    try {
      await cancelAnalysis(jobId);
    } catch {
      // The analyze() rejection is the authoritative outcome; a failed
      // cancel call (e.g. backend gone) surfaces through it.
    }
  }, []);

  /**
   * Run AWRE field detection over the last finished analysis (same signal,
   * range and effective params). No-op until a result exists. Settles are
   * dropped when superseded (re-run) or when the signal changed mid-flight.
   */
  const detectFields = useCallback(async (): Promise<void> => {
    const m = metaRef.current;
    const done = lastDoneRef.current;
    if (m === null || done === null || awreBusyRef.current) return;
    const signalIdNow = m.signal_id;
    const seq = ++awreSeqRef.current;
    awreBusyRef.current = true;
    setAwre({ kind: "running" });
    try {
      const labels = await runAwre(signalIdNow, done.range, done.params);
      if (awreSeqRef.current !== seq || metaRef.current?.signal_id !== signalIdNow) return;
      setAwre({ kind: "done", labels });
    } catch (e) {
      if (awreSeqRef.current !== seq || metaRef.current?.signal_id !== signalIdNow) return;
      const err = toErrorState(e);
      setAwre(err.code === "cancelled" ? { kind: "idle" } : { kind: "error", error: err });
    } finally {
      if (awreSeqRef.current === seq) awreBusyRef.current = false;
    }
  }, []);

  const result = job.kind === "done" ? job.result : null;
  const labels = awre.kind === "done" ? awre.labels : null;

  return {
    // state
    fields,
    selectionOnly,
    job,
    awre,
    paramError,
    result,
    labels,
    // actions
    setField,
    applyFields,
    applyEstimatedToFields,
    setSelectionOnly,
    run,
    cancel,
    detectFields,
  };
}

export type AnalysisApi = ReturnType<typeof useAnalysis>;
