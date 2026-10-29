/**
 * Simulator panel (P5, left rail alongside Capture/Generator): "Load
 * .simulator…" (read-only rule list — index, name, kind, summary, with an
 * empty-state message), Run with an editable timeout (30 s default; spinner +
 * elapsed while running — cancel is not required by the contract), and the
 * transcript below (timestamp, source → destination, action, bits preview
 * truncated like the protocol table).
 *
 * Presentational only — state and IPC sequencing live in useSimulator.
 */

import { type SimulatorApi } from "../app/useSimulator";
import { errorText } from "../app/errors";
import { formatDuration } from "../app/format";
import { simBitsPreview } from "../app/simulatorModel";

interface SimulatorPanelProps {
  simulator: SimulatorApi;
}

export function SimulatorPanel(props: SimulatorPanelProps): JSX.Element {
  const { simulator } = props;
  const { simPath, rules, timeoutText, status, transcript, error, paramError, elapsedMs } = simulator;

  const loaded = rules !== null;
  const running = status === "running";
  const rulesBody = (): JSX.Element => {
    if (!loaded) {
      return <span className="param-hint sim-hint">no simulator loaded</span>;
    }
    if (rules.length === 0) {
      return <span className="param-hint sim-hint">this simulator has no rules</span>;
    }
    return (
      <div className="sim-table" role="table" aria-label="Simulator rules">
        <div className="sim-row sim-head" role="row">
          <span className="sim-c sim-c-idx">#</span>
          <span className="sim-c">name</span>
          <span className="sim-c sim-c-kind">kind</span>
          <span className="sim-c">summary</span>
        </div>
        {rules.map((r) => (
          <div key={r.index} className="sim-row" role="row" title={r.summary}>
            <span className="sim-c sim-c-idx">{r.index}</span>
            <span className="sim-c" title={r.name}>
              {r.name}
            </span>
            <span className="sim-c sim-c-kind" title={r.kind}>
              {r.kind}
            </span>
            <span className="sim-c" title={r.summary}>
              {r.summary}
            </span>
          </div>
        ))}
      </div>
    );
  };

  return (
    <div className="simulator-panel">
      <div className="panel-section">
        <div className="panel-title">Simulator</div>
        <div className="run-row">
          <button type="button" className="primary" onClick={() => void simulator.load()} disabled={status !== "idle"}>
            Load .simulator…
          </button>
          {status === "loading" && <span className="job-state job-running">loading…</span>}
        </div>
        {simPath !== null && (
          <span className="param-hint sim-hint" title={simPath}>
            {simPath}
          </span>
        )}
      </div>

      <div className="panel-section">
        <div className="panel-title">Rules</div>
        {rulesBody()}
      </div>

      <div className="panel-section">
        <div className="panel-title">Run</div>
        <div className="run-row">
          <button type="button" className="primary" onClick={() => void simulator.run()} disabled={!loaded || status !== "idle"}>
            Run
          </button>
          <label className="sim-timeout" title="Hard timeout for the simulation run, in seconds">
            <span>timeout</span>
            <input
              className="param-input"
              type="text"
              inputMode="decimal"
              spellCheck={false}
              aria-label="Timeout (seconds)"
              value={timeoutText}
              disabled={status !== "idle"}
              onChange={(e) => simulator.setTimeoutField(e.target.value)}
            />
            <span>s</span>
          </label>
        </div>
        {running && (
          <span className="job-state job-running sim-elapsed">
            <span className="loading-dot" aria-label="running" /> running… {(elapsedMs / 1000).toFixed(1)} s
          </span>
        )}
        {paramError !== null && (
          <div className="param-error" role="alert">
            {paramError}
          </div>
        )}
      </div>

      <div className="panel-section sim-transcript-section">
        <div className="panel-title">
          Transcript
          {transcript !== null && <span className="sim-count">{transcript.length} entries</span>}
        </div>
        {transcript === null ? (
          <span className="param-hint sim-hint">run the simulator to see its transcript</span>
        ) : transcript.length === 0 ? (
          <span className="param-hint sim-hint">the simulation produced no transcript entries</span>
        ) : (
          <div className="sim-transcript" role="log" aria-label="Simulation transcript">
            {transcript.map((e, i) => (
              <div key={i} className="sim-entry">
                <span className="sim-entry-time" title={`${e.timestamp} s`}>
                  {formatDuration(e.timestamp)}
                </span>
                <span className="sim-entry-flow" title={`${e.source} → ${e.destination} · ${e.action}`}>
                  {e.source} → {e.destination} · {e.action}
                </span>
                <span className="sim-entry-bits" title={e.bits}>
                  {simBitsPreview(e.bits)}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {error !== null && (
        <div className="error-banner" role="alert">
          {errorText(error)}
          <button type="button" className="link-btn sim-dismiss" onClick={simulator.dismissError}>
            dismiss
          </button>
        </div>
      )}
    </div>
  );
}
