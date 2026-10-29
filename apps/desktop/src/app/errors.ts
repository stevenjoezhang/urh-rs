/**
 * Shared error plumbing for the app layer: normalizing unknown rejections
 * into the `{code, message}` shape the status bar / banners render, and the
 * code-specific message mapping (branch on `code`, never parse `message`).
 *
 * useSignalView has its own private copy (kept untouched); new view models
 * import these.
 */

import type { ErrorState } from "./useSignalView";

export type { ErrorState };

/** Normalize any rejection (IpcError, AppError shape, unknown) for display. */
export function toErrorState(e: unknown): ErrorState {
  if (e instanceof Error && "code" in e && typeof (e as { code: unknown }).code === "string") {
    return { code: (e as { code: string }).code, message: e.message };
  }
  return { code: "internal", message: e instanceof Error ? e.message : String(e) };
}

/** Human text for an error state; branches on the stable code. */
export function errorText(err: ErrorState): string {
  switch (err.code) {
    case "file-not-found":
      return `File not found: ${err.message}`;
    case "unknown-signal":
      return `Signal is no longer open: ${err.message}`;
    case "invalid-range":
      return `View range rejected: ${err.message}`;
    case "invalid-argument":
      return `Invalid argument: ${err.message}`;
    case "io":
      return `Read error: ${err.message}`;
    case "internal":
      return `Internal error: ${err.message}`;
    case "cancelled":
      return `Cancelled: ${err.message}`;
    default:
      return `${err.code}: ${err.message}`;
  }
}
