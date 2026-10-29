/**
 * Simulator view model (P5): load a .simulator config (read-only rule list),
 * run it with a timeout, hold the transcript. IPC-thin like the other hooks —
 * the bridge parses/validates, the pure simulatorModel validates the timeout
 * field and shapes the bits preview; this hook sequences them.
 *
 * v2 is read + run only: no rule editing, no live-signal sources (contract).
 * The run has a hard timeout (default 30 s, editable); cancelling is not
 * required by the contract and not offered — the timeout bounds every run.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type SimRuleInfo,
  type SimTranscriptEntry,
  loadSimulator,
  pickSimulatorPath,
  runSimulation,
} from "../bridge/api";
import { type ErrorState, toErrorState } from "./errors";
import { DEFAULT_SIM_TIMEOUT_SECS, parseSimTimeout } from "./simulatorModel";

/** Elapsed-time tick rate while a simulation runs (spinner + seconds). */
const ELAPSED_TICK_MS = 100;

export type SimStatus = "idle" | "loading" | "running";

export function useSimulator() {
  const [simPath, setSimPath] = useState<string | null>(null);
  /** null = no simulator loaded; [] = loaded but rule-less. */
  const [rules, setRules] = useState<SimRuleInfo[] | null>(null);
  const [timeoutText, setTimeoutText] = useState(String(DEFAULT_SIM_TIMEOUT_SECS));
  const [status, setStatus] = useState<SimStatus>("idle");
  const [transcript, setTranscript] = useState<SimTranscriptEntry[] | null>(null);
  const [error, setError] = useState<ErrorState | null>(null);
  const [paramError, setParamError] = useState<string | null>(null);
  /** Milliseconds since the run started (drives the elapsed readout). */
  const [elapsedMs, setElapsedMs] = useState(0);

  // Mirrors so the async actions always read the latest values.
  const statusRef = useRef<SimStatus>("idle");
  const timeoutTextRef = useRef(timeoutText);
  const simPathRef = useRef(simPath);
  const startedAtRef = useRef(0);

  useEffect(() => {
    timeoutTextRef.current = timeoutText;
    simPathRef.current = simPath;
  }, [timeoutText, simPath]);

  // Elapsed ticker: alive exactly while a run is in flight.
  useEffect(() => {
    if (status !== "running") return;
    const id = window.setInterval(() => {
      setElapsedMs(performance.now() - startedAtRef.current);
    }, ELAPSED_TICK_MS);
    return () => window.clearInterval(id);
  }, [status]);

  const setStatusState = useCallback((next: SimStatus): void => {
    statusRef.current = next;
    setStatus(next);
  }, []);

  /** Load a .simulator config (read-only rule listing); cancel = no-op. */
  const load = useCallback(async (): Promise<void> => {
    if (statusRef.current !== "idle") return;
    setStatusState("loading");
    setError(null);
    setParamError(null);
    try {
      const path = await pickSimulatorPath();
      if (path === null) return; // dialog cancelled
      const loaded = await loadSimulator(path);
      setSimPath(path);
      setRules(loaded);
      setTranscript(null); // the old run belongs to the old config
    } catch (e) {
      setError(toErrorState(e));
    } finally {
      setStatusState("idle");
    }
  }, [setStatusState]);

  /** Run the loaded config; an invalid timeout never reaches IPC. */
  const run = useCallback(async (): Promise<void> => {
    if (statusRef.current !== "idle") return;
    const path = simPathRef.current;
    if (path === null) return;
    const parsed = parseSimTimeout(timeoutTextRef.current);
    if (!parsed.ok) {
      setParamError(`timeout: ${parsed.message}`);
      return;
    }
    setParamError(null);
    setError(null);
    startedAtRef.current = performance.now();
    setElapsedMs(0);
    setStatusState("running");
    try {
      const entries = await runSimulation(path, parsed.secs);
      setTranscript(entries);
    } catch (e) {
      setError(toErrorState(e));
    } finally {
      setStatusState("idle");
    }
  }, [setStatusState]);

  /** Update the timeout field (text, seconds); clears the last error. */
  const setTimeoutField = useCallback((value: string): void => {
    setTimeoutText(value);
    setParamError(null);
  }, []);

  const dismissError = useCallback((): void => setError(null), []);

  return {
    // state
    simPath,
    rules,
    timeoutText,
    status,
    transcript,
    error,
    paramError,
    elapsedMs,
    // actions
    load,
    run,
    setTimeoutField,
    dismissError,
  };
}

export type SimulatorApi = ReturnType<typeof useSimulator>;
