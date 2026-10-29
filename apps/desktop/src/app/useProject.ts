/**
 * Project view model (P4): the save/load sequencing around the pure
 * projectModel helpers. IPC-thin like the other hooks — the bridge parses and
 * validates the file, the pure model collects/resolves, and this hook only
 * sequences them and owns the transient UI state (busy, error, warnings).
 *
 * Load re-opens every project file via `open_signal` (with the saved sample
 * rate) and then activates + re-applies state. Per-signal open failures are
 * NON-fatal: the project still loads, the failures are collected as warning
 * lines for the banner. A failed load (unreadable/mismatched file) is fatal
 * and lands in the error banner; version mismatches surface the same way
 * (the bridge rejects them with a clear message before anything re-opens).
 *
 * The id-correlation rule for entries is projectModel's (see its header):
 * id match first, else the first entry — exact for this app's single-entry
 * files.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type ProjectState,
  type SignalMeta,
  loadProject,
  openSignal,
  pickOpenProjectPath,
  pickSaveProjectPath,
  saveProject,
} from "../bridge/api";
import { WaveformMode } from "../bridge/protocol";
import type { ViewRange } from "../render/WaveformRenderer";
import { type ErrorState, errorText, toErrorState } from "./errors";
import {
  type ProjectSaveInput,
  buildProjectState,
  paramsToFields,
  pickProjectEntry,
  resolveActiveSignal,
} from "./projectModel";
import { EMPTY_PARAM_FIELDS, type ParamFields } from "./useAnalysis";

export interface UseProjectOptions {
  /** Snapshot supplier for save — called once, after the dialog resolves. */
  getSaveInput: () => ProjectSaveInput;
  /** Re-read the open-signal list (called after load opened new signals). */
  refreshSignals: () => Promise<SignalMeta[]>;
  /**
   * Activate a signal; the optional restore bundle applies a saved view+mode
   * (clamped by the view model against the new signal's extent).
   */
  activateSignal: (m: SignalMeta, restore?: { view: ViewRange; mode: WaveformMode }) => void;
  /** Replace the analysis panel's parameter fields (project load). */
  applyAnalysisFields: (fields: ParamFields) => void;
}

export type ProjectBusy = "idle" | "saving" | "loading";

export function useProject(options: UseProjectOptions) {
  const [busy, setBusy] = useState<ProjectBusy>("idle");
  const [error, setError] = useState<ErrorState | null>(null);
  /** Per-signal reopen failures of the last load (null = no warnings). */
  const [warnings, setWarnings] = useState<string[] | null>(null);
  const [savedPath, setSavedPath] = useState<string | null>(null);

  // Mirrors so the async actions always read the latest values.
  const busyRef = useRef<ProjectBusy>("idle");
  const optionsRef = useRef(options);
  useEffect(() => {
    optionsRef.current = options;
  }, [options]);

  const setBusyState = useCallback((next: ProjectBusy): void => {
    busyRef.current = next;
    setBusy(next);
  }, []);

  /** Write the current app state into a .urhproj; dialog cancel = no-op. */
  const save = useCallback(async (): Promise<void> => {
    if (busyRef.current !== "idle") return;
    setBusyState("saving");
    setError(null);
    setSavedPath(null);
    try {
      const path = await pickSaveProjectPath();
      if (path === null) return; // dialog cancelled
      const state = buildProjectState(optionsRef.current.getSaveInput());
      await saveProject(path, state);
      setSavedPath(path);
    } catch (e) {
      setError(toErrorState(e));
    } finally {
      setBusyState("idle");
    }
  }, [setBusyState]);

  /** The load pipeline, split out so `load` stays a thin dialog wrapper. */
  const loadAndApply = useCallback(async (path: string): Promise<void> => {
    const state: ProjectState = await loadProject(path);

    // Reopen every file (saved sample rate included). One failure must not
    // fail the load — it becomes a warning line and the rest proceeds.
    const opened: SignalMeta[] = [];
    const failed: string[] = [];
    for (const s of state.signals) {
      try {
        opened.push(await openSignal(s.path, s.sampleRate));
      } catch (e) {
        failed.push(`${s.path}: ${errorText(toErrorState(e))}`);
      }
    }

    await optionsRef.current.refreshSignals();

    const target = resolveActiveSignal(state, opened);
    if (target !== null) {
      const viewEntry = pickProjectEntry(state.view, target, opened);
      optionsRef.current.activateSignal(
        target,
        viewEntry === null ? undefined : { view: { start: viewEntry.start, end: viewEntry.end }, mode: viewEntry.mode },
      );
      // The project defines the panel state: apply its params, or reset to
      // auto when this signal has no entry.
      const analysisEntry = pickProjectEntry(state.analysis, target, opened);
      optionsRef.current.applyAnalysisFields(
        analysisEntry === null ? EMPTY_PARAM_FIELDS : paramsToFields(analysisEntry.params),
      );
    }

    if (failed.length > 0) setWarnings(failed);
  }, []);

  /** Open a .urhproj, reopen its signals and re-apply state; cancel = no-op. */
  const load = useCallback(async (): Promise<void> => {
    if (busyRef.current !== "idle") return;
    setBusyState("loading");
    setError(null);
    setWarnings(null);
    try {
      const path = await pickOpenProjectPath();
      if (path === null) return; // dialog cancelled
      await loadAndApply(path);
    } catch (e) {
      setError(toErrorState(e));
    } finally {
      setBusyState("idle");
    }
  }, [setBusyState, loadAndApply]);

  const dismissError = useCallback((): void => setError(null), []);
  const dismissWarnings = useCallback((): void => setWarnings(null), []);

  return {
    // state
    busy,
    error,
    warnings,
    savedPath,
    // actions
    save,
    load,
    dismissError,
    dismissWarnings,
  };
}

export type ProjectApi = ReturnType<typeof useProject>;
