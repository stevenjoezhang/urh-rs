/**
 * Generator panel (P4): modulation dropdown, bits field ([01]* validation),
 * samples/bit, pause and center; Generate registers a `gen-<n>` signal (the
 * app refreshes the signal list and activates it); "Save as .complex" runs
 * the native save dialog and `save_signal_as` on the active signal.
 *
 * Presentational only — state and IPC sequencing live in useGenerator.
 */

import { type GeneratorApi, type GeneratorField } from "../app/useGenerator";
import { errorText } from "../app/errors";
import { formatCount } from "../app/format";
import { MODULATIONS } from "../bridge/api";

interface GeneratorPanelProps {
  generator: GeneratorApi;
  /** An active signal exists (Generate's result or a recording). */
  canSave: boolean;
  /** Display name of the signal "Save as .complex" would write. */
  saveTargetName: string | null;
}

/** One labeled text parameter input (mirrors the analysis panel's rows). */
function GeneratorInput(props: {
  label: string;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  title?: string;
  onChange: (v: string) => void;
}): JSX.Element {
  const { label, value, placeholder, disabled = false, title, onChange } = props;
  return (
    <label className="param-row" title={title}>
      <span className="param-label">{label}</span>
      <input
        className="param-input"
        type="text"
        inputMode="text"
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

export function GeneratorPanel(props: GeneratorPanelProps): JSX.Element {
  const { generator, canSave, saveTargetName } = props;
  const { fields, busy, saving, error, paramError, generated, savedPath } = generator;

  const setField = (field: GeneratorField) => (v: string) => generator.setField(field, v);

  return (
    <div className="generator-panel">
      <div className="panel-section">
        <div className="panel-title">Modulation</div>
        <label className="param-row">
          <span className="param-label">modulation</span>
          <select
            className="param-select"
            aria-label="Modulation"
            value={fields.modulation}
            disabled={busy}
            onChange={(e) => generator.setField("modulation", e.target.value)}
          >
            {MODULATIONS.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </label>
        <GeneratorInput
          label="bits"
          value={fields.bits}
          disabled={busy}
          title="Bit string to modulate (0s and 1s)"
          onChange={setField("bits")}
        />
        <GeneratorInput
          label="samples / bit"
          value={fields.samplesPerBit}
          disabled={busy}
          onChange={setField("samplesPerBit")}
        />
        <GeneratorInput
          label="pause"
          value={fields.pauseSamples}
          disabled={busy}
          title="Pause appended after the bits, in samples"
          onChange={setField("pauseSamples")}
        />
        <GeneratorInput
          label="center"
          value={fields.center}
          disabled={busy}
          title="Decision center (amplitude midpoint for ASK)"
          onChange={setField("center")}
        />
        {paramError !== null && (
          <div className="param-error" role="alert">
            {paramError}
          </div>
        )}
      </div>

      <div className="panel-section run-row">
        <button type="button" className="primary" onClick={() => void generator.run()} disabled={busy}>
          Generate
        </button>
        {busy && <span className="job-state job-running">generating…</span>}
        {!busy && generated !== null && (
          <span className="job-state">
            {generated.signal_id} · {formatCount(generated.sample_count)} samples
          </span>
        )}
      </div>

      <div className="panel-section">
        <div className="panel-title">Signal</div>
        <div className="run-row">
          <button
            type="button"
            onClick={() => void generator.saveAs()}
            disabled={!canSave || saving || busy}
            title={canSave ? `Write ${saveTargetName ?? "the active signal"} as .complex (f32)` : "No active signal"}
          >
            Save as .complex…
          </button>
          {saving && <span className="job-state job-running">saving…</span>}
        </div>
        {savedPath !== null && (
          <span className="param-hint" title={savedPath}>
            saved {savedPath}
          </span>
        )}
        <span className="param-hint">
          {canSave ? `saves the active signal (${saveTargetName ?? "unnamed"})` : "generate or open a signal first"}
        </span>
      </div>

      {error !== null && (
        <div className="error-banner" role="alert">
          {errorText(error)}
        </div>
      )}
    </div>
  );
}
