/**
 * Shared display formatters (status bar, chips, protocol table). Pure.
 */

export function formatCount(n: number): string {
  return Math.floor(n).toLocaleString("en-US");
}

export function trimNum(v: number): string {
  return Number(v.toFixed(3)).toString();
}

export function formatHz(hz: number): string {
  if (!Number.isFinite(hz) || hz <= 0) return "—";
  if (hz >= 1e9) return `${trimNum(hz / 1e9)} GHz`;
  if (hz >= 1e6) return `${trimNum(hz / 1e6)} MHz`;
  if (hz >= 1e3) return `${trimNum(hz / 1e3)} kHz`;
  return `${trimNum(hz)} Hz`;
}

export function formatDuration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return "—";
  if (secs === 0) return "0 s";
  if (secs >= 1) return `${trimNum(secs)} s`;
  if (secs >= 1e-3) return `${trimNum(secs * 1e3)} ms`;
  if (secs >= 1e-6) return `${trimNum(secs * 1e6)} µs`;
  return `${trimNum(secs * 1e9)} ns`;
}

export function formatZoom(zoom: number): string {
  if (!Number.isFinite(zoom) || zoom <= 0) return "—";
  if (zoom >= 0.1) return zoom.toFixed(2);
  if (zoom >= 0.001) return zoom.toFixed(4);
  return zoom.toExponential(1);
}
