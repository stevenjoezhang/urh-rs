/**
 * Project view-model (P4) — pure save-collection and load-resolution logic,
 * no React, no DOM; the useProject hook sequences IPC around these and the
 * panels/components stay thin.
 *
 * Save semantics (what buildProjectState collects):
 * - signals: the open-signal list filtered to file-backed entries — recordings
 *   (`rec-<n>`) and generated signals (`gen-<n>`) have an empty `path`, cannot
 *   be reopened and are skipped;
 * - active signal id: kept only when the active signal is one of the kept
 *   (file-backed) signals, else null;
 * - analysis/view entries: emitted in `signals` order, only for kept signals
 *   that actually have them ("have them" = at least one non-auto param field
 *   / a view entry supplied by the caller).
 *
 * Load semantics (the id-correlation rule, also documented in useProject):
 * a project file references signals by their save-time repository ids, but
 * re-opening mints fresh ids, so ids only line up when the backend re-opens a
 * path into the same signal (or the numbering coincides). Resolution is
 * therefore: match by id first; if no entry id matches any re-opened signal,
 * the first entry is the best-effort match — exact for files saved by this
 * app, which writes at most one entry (the active signal's).
 */

import {
  type AnalysisParams,
  type ProjectAnalysisEntry,
  type ProjectState,
  type ProjectViewEntry,
  type SignalMeta,
  PROJECT_VERSION,
} from "../bridge/api";
import { WaveformMode } from "../bridge/protocol";
import type { ParamFields } from "./useAnalysis";

/** A per-signal waveform view as saved into / restored from a project. */
export interface SignalViewState {
  start: number;
  end: number;
  mode: WaveformMode;
}

/** What the app hands to {@link buildProjectState} when saving. */
export interface ProjectSaveInput {
  /** All open signals (opened files, recordings, generated). */
  signals: readonly SignalMeta[];
  /** Id of the active signal, if any. */
  activeSignalId: string | null;
  /** Analysis params per signal id (the panel state; only signals that have any). */
  analysisBySignal: Readonly<Record<string, AnalysisParams>>;
  /** View state per signal id (the waveform view model's, for signals it tracks). */
  viewBySignal: Readonly<Record<string, SignalViewState>>;
}

/** Does the params object pin at least one field (i.e. is not fully auto)? */
export function hasAnyParam(params: AnalysisParams): boolean {
  return (
    params.modulation !== undefined ||
    params.bitLength !== undefined ||
    params.center !== undefined ||
    params.noiseThreshold !== undefined ||
    params.tolerance !== undefined
  );
}

/**
 * Build the savable {@link ProjectState} from the app state: file-backed
 * signals only, active id nulled unless it is file-backed, analysis/view
 * entries for kept signals that have them (in `signals` order).
 */
export function buildProjectState(input: ProjectSaveInput): ProjectState {
  const kept = input.signals.filter((s) => s.path !== "");
  const keptIds = new Set(kept.map((s) => s.signal_id));
  const analysis: ProjectAnalysisEntry[] = [];
  const view: ProjectViewEntry[] = [];
  for (const s of kept) {
    const params = input.analysisBySignal[s.signal_id];
    if (params !== undefined && hasAnyParam(params)) {
      analysis.push({ signalId: s.signal_id, params });
    }
    const v = input.viewBySignal[s.signal_id];
    if (v !== undefined) {
      view.push({ signalId: s.signal_id, start: v.start, end: v.end, mode: v.mode });
    }
  }
  return {
    version: PROJECT_VERSION,
    signals: kept.map((s) => ({ path: s.path, sampleRate: s.sample_rate })),
    activeSignalId:
      input.activeSignalId !== null && keptIds.has(input.activeSignalId) ? input.activeSignalId : null,
    analysis,
    view,
  };
}

/**
 * Which re-opened signal to activate: the saved active id when it matches one
 * of them, else the first successfully opened signal (null when none opened).
 */
export function resolveActiveSignal(
  state: ProjectState,
  opened: readonly SignalMeta[],
): SignalMeta | null {
  if (opened.length === 0) return null;
  return opened.find((m) => m.signal_id === state.activeSignalId) ?? opened[0];
}

/**
 * Pick the project entry (analysis or view) that belongs to the activated
 * signal. Id match first; otherwise, when *no* entry id matches any re-opened
 * signal (fresh-session reload), the first entry is the best-effort match —
 * exact for this app's single-entry files. Null when nothing applies.
 */
export function pickProjectEntry<T extends { signalId: string }>(
  entries: readonly T[],
  target: SignalMeta | null,
  opened: readonly SignalMeta[],
): T | null {
  if (target === null) return null;
  const byId = entries.find((e) => e.signalId === target.signal_id);
  if (byId !== undefined) return byId;
  const anyIdMatch = entries.some((e) => opened.some((m) => m.signal_id === e.signalId));
  return anyIdMatch ? null : (entries[0] ?? null);
}

/**
 * Analysis params → editor fields (the load direction of useAnalysis's
 * `parseParamFields`): undefined ("auto") becomes the empty string.
 */
export function paramsToFields(params: AnalysisParams): ParamFields {
  return {
    modulation: params.modulation ?? "",
    bitLength: params.bitLength !== undefined ? String(params.bitLength) : "",
    center: params.center !== undefined ? String(params.center) : "",
    noiseThreshold: params.noiseThreshold !== undefined ? String(params.noiseThreshold) : "",
    tolerance: params.tolerance !== undefined ? String(params.tolerance) : "",
  };
}
