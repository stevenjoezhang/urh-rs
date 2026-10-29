/**
 * Device/capture panel (P3): device dropdown (unavailable entries disabled
 * with their reason as the option tooltip), capture parameters (frequency,
 * sample rate, bandwidth, gain — host/port for rtl-tcp), the record checkbox,
 * Start/Stop and the live stats line with a visible overflow warning.
 *
 * Presentational only — state and IPC sequencing live in useCapture; the
 * center-area live view (waveform + waterfall) is rendered by App while a
 * capture runs.
 */

import { type CaptureApi, RTL_TCP_DEVICE } from "../app/useCapture";
import { errorText } from "../app/errors";
import { formatCount, formatDuration } from "../app/format";

interface DevicePanelProps {
  capture: CaptureApi;
}

/** One labeled text parameter input (mirrors the analysis panel's rows). */
function CaptureInput(props: {
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

export function DevicePanel(props: DevicePanelProps): JSX.Element {
  const { capture } = props;
  const { fields, devices, status, active, stats, error, paramError, droppedPreview } = capture;

  const running = status !== "idle";
  const controlsLocked = running || devices.length === 0;
  const isRtlTcp = fields.device === RTL_TCP_DEVICE;
  const duration =
    active !== null && stats !== null && active.sampleRate > 0
      ? formatDuration(stats.recordedSamples / active.sampleRate)
      : null;

  const statusText = (): string => {
    switch (status) {
      case "idle":
        return active === null && stats === null ? "idle" : "stopped";
      case "starting":
        return "starting…";
      case "running":
        return "capturing…";
      case "stopping":
        return "stopping…";
    }
  };

  return (
    <div className="device-panel">
      <div className="panel-section">
        <div className="panel-title">Device</div>
        <select
          className="param-select"
          aria-label="Capture device"
          value={fields.device}
          disabled={controlsLocked}
          onChange={(e) => capture.setField("device", e.target.value)}
        >
          {devices.length === 0 && <option value="">no devices</option>}
          {devices.map((d) => (
            <option
              key={d.name}
              value={d.name}
              disabled={!d.available}
              title={d.available ? d.backend : d.detail === "" ? "unavailable" : d.detail}
            >
              {d.available ? `${d.name} (${d.backend})` : `${d.name} — unavailable`}
            </option>
          ))}
        </select>
        <span className="param-hint">
          {devices.length === 0
            ? "no devices enumerated"
            : devices.every((d) => !d.available)
              ? "all devices unavailable"
              : "unavailable devices are disabled"}
        </span>
      </div>

      <div className="panel-section">
        <div className="panel-title">Parameters</div>
        <CaptureInput
          label="frequency"
          value={fields.frequency}
          disabled={controlsLocked}
          title="Tuning frequency in Hz"
          onChange={(v) => capture.setField("frequency", v)}
        />
        <CaptureInput
          label="sample rate"
          value={fields.sampleRate}
          disabled={controlsLocked}
          title="Sample rate in Hz"
          onChange={(v) => capture.setField("sampleRate", v)}
        />
        <CaptureInput
          label="bandwidth"
          value={fields.bandwidth}
          disabled={controlsLocked}
          title="Channel filter bandwidth in Hz"
          onChange={(v) => capture.setField("bandwidth", v)}
        />
        <CaptureInput
          label="gain"
          value={fields.gain}
          disabled={controlsLocked}
          title="Gain (device units, usually dB)"
          onChange={(v) => capture.setField("gain", v)}
        />
        {isRtlTcp && (
          <>
            <CaptureInput
              label="host"
              value={fields.host}
              disabled={controlsLocked}
              title="rtl-tcp server host"
              onChange={(v) => capture.setField("host", v)}
            />
            <CaptureInput
              label="port"
              value={fields.port}
              disabled={controlsLocked}
              title="rtl-tcp server port (1–65535)"
              onChange={(v) => capture.setField("port", v)}
            />
          </>
        )}
        {paramError !== null && (
          <div className="param-error" role="alert">
            {paramError}
          </div>
        )}
      </div>

      <div className="panel-section">
        <label className="check-row" title="Register the recorded samples as a rec-<n> signal on stop">
          <input
            type="checkbox"
            checked={fields.record}
            disabled={running}
            onChange={(e) => capture.setRecord(e.target.checked)}
          />
          <span>Record into a signal</span>
        </label>
        <span className="param-hint">
          {fields.record
            ? "preview batches stream live; samples register on stop"
            : "preview only — nothing is kept"}
        </span>
      </div>

      <div className="panel-section run-row">
        <button
          type="button"
          className="primary"
          onClick={() => void capture.start()}
          disabled={running || devices.length === 0}
        >
          Start capture
        </button>
        <button type="button" onClick={() => void capture.stop()} disabled={status !== "running"}>
          Stop capture
        </button>
        <span className={`job-state job-${status === "idle" ? "idle" : "running"}`}>{statusText()}</span>
      </div>

      {stats !== null && (
        <div className="panel-section">
          <div className="panel-title">Stats</div>
          <div className="stats-line">
            recorded {formatCount(stats.recordedSamples)} samples
            {duration === null ? "" : ` (${duration})`} · dropped {formatCount(droppedPreview)} preview batches
          </div>
          {stats.overflowDetected && (
            <div className="error-banner" role="alert">
              Recording overflowed — the capture stopped and recorded samples were lost.
            </div>
          )}
        </div>
      )}

      {error !== null && (
        <div className="error-banner" role="alert">
          {errorText(error)}
        </div>
      )}
    </div>
  );
}
