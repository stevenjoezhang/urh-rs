/**
 * Analysis side panel (P2): parameter editor, run/cancel, job state, and
 * the AWRE trigger. Presentational only — all state and IPC sequencing live
 * in useAnalysis; the panel resolves the "selection only" range from the
 * shared selection interval and hands it to `run`.
 */

import type { Interval } from "../render/WaveformRenderer";
import { type AnalysisApi, type ParamFieldName } from "../app/useAnalysis";
import { errorText } from "../app/errors";
import { MODULATIONS } from "../bridge/api";
import { formatCount } from "../app/format";

interface AnalysisPanelProps {
  analysis: AnalysisApi;
  /** Normalized selection interval (single source of truth), or null. */
  selectionInterval: Interval | null;
  /** A signal is open. */
  enabled: boolean;
}

/** One labeled numeric parameter input; empty string means "auto". */
function ParamInput(props: {
  label: string;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  onChange: (v: string) => void;
}): JSX.Element {
  const { label, value, placeholder = "auto", disabled = false, onChange } = props;
  return (
    <label className="param-row">
      <span className="param-label">{label}</span>
      <input
        className="param-input"
        type="text"
        inputMode="decimal"
        spellCheck={false}
        aria-label={label}
        placeholder={placeholder}
        value={value}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  );
}

export function AnalysisPanel(props: AnalysisPanelProps): JSX.Element {
  const { analysis, selectionInterval, enabled } = props;
  const { fields, job, awre, paramError, selectionOnly } = analysis;

  const running = job.kind === "running";
  const rangeLocked = selectionOnly && selectionInterval === null;
  const canRun = enabled && !running && !rangeLocked;

  const setField = (field: ParamFieldName) => (v: string) => analysis.setField(field, v);

  const onRun = (): void => {
    void analysis.run(selectionOnly && selectionInterval !== null ? selectionInterval : null);
  };

  const jobStateText = (): string => {
    switch (job.kind) {
      case "idle":
        return "idle";
      case "running":
        return "running…";
      case "done":
        return `done · ${formatCount(job.result.messages.length)} messages`;
      case "error":
        return "error";
    }
  };

  const est = job.kind === "done" ? job.result.params : null;

  return (
    <div className="analysis-panel">
      <div className="panel-section">
        <div className="panel-title">Parameters</div>
        <label className="param-row">
          <span className="param-label">modulation</span>
          <select
            className="param-select"
            aria-label="Modulation"
            value={fields.modulation}
            disabled={!enabled}
            onChange={(e) => analysis.setField("modulation", e.target.value)}
          >
            <option value="">auto</option>
            {MODULATIONS.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </label>
        <ParamInput label="bit length" value={fields.bitLength} disabled={!enabled} onChange={setField("bitLength")} />
        <ParamInput label="center" value={fields.center} disabled={!enabled} onChange={setField("center")} />
        <ParamInput
          label="noise threshold"
          value={fields.noiseThreshold}
          disabled={!enabled}
          onChange={setField("noiseThreshold")}
        />
        <ParamInput label="tolerance" value={fields.tolerance} disabled={!enabled} onChange={setField("tolerance")} />
        {paramError !== null && (
          <div className="param-error" role="alert">
            {paramError}
          </div>
        )}
      </div>

      <div className="panel-section">
        <label className="check-row" title={rangeLocked ? "Drag a selection on the waveform first" : undefined}>
          <input
            type="checkbox"
            checked={selectionOnly}
            disabled={!enabled}
            onChange={(e) => analysis.setSelectionOnly(e.target.checked)}
          />
          <span>Analyze selection only</span>
        </label>
        <span className="param-hint">
          {selectionOnly
            ? selectionInterval !== null
              ? `range ${formatCount(selectionInterval.start)} – ${formatCount(selectionInterval.end)}`
              : "no selection — drag on the waveform"
            : "whole signal"}
        </span>
      </div>

      <div className="panel-section run-row">
        <button type="button" className="primary" onClick={onRun} disabled={!canRun}>
          Analyze
        </button>
        <button type="button" onClick={() => void analysis.cancel()} disabled={!running}>
          Cancel
        </button>
        <span className={`job-state job-${job.kind}`}>{jobStateText()}</span>
      </div>

      {job.kind === "error" && (
        <div className="error-banner" role="alert">
          {errorText(job.error)}
        </div>
      )}

      {est !== null && (
        <div className="panel-section">
          <div className="panel-title">
            Estimated
            <button type="button" className="link-btn" onClick={analysis.applyEstimatedToFields}>
              use as parameters
            </button>
          </div>
          <div className="est-line">
            {est.modulation} · {formatCount(est.bitLength)} spb · center {est.center} · noise {est.noiseThreshold} ·
            tol {formatCount(est.tolerance)}
          </div>
        </div>
      )}

      <div className="panel-section run-row">
        <button
          type="button"
          onClick={() => void analysis.detectFields()}
          disabled={!enabled || job.kind !== "done" || awre.kind === "running"}
          title="Automatic Wireless Recognition Engine — detect preamble/sync/… fields"
        >
          Detect fields (AWRE)
        </button>
        {awre.kind === "running" && <span className="job-state">detecting…</span>}
        {awre.kind === "done" && <span className="job-state">{awre.labels.length} labels</span>}
      </div>
      {awre.kind === "error" && (
        <div className="error-banner" role="alert">
          {errorText(awre.error)}
        </div>
      )}
    </div>
  );
}
