/**
 * Protocol table (P2): virtualized-by-slicing message grid.
 *
 * Only the rows intersecting the scroll viewport (plus overscan) are mounted,
 * at a fixed row height, over a full-height spacer — the DOM stays O(viewport)
 * no matter how many messages the analysis produced. All math
 * (slicing, truncation, overlap, label layout) is the pure view model in
 * app/protocolTable.ts.
 *
 * Bidirectional selection: a row click reports the message up (the waveform
 * highlights and frames its sample extent); conversely, rows whose
 * `[start_sample, end_sample)` overlaps the shared selection interval are
 * highlighted here.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import type { AnalysisResult, FieldLabel } from "../bridge/api";
import type { Interval } from "../render/WaveformRenderer";
import { formatCount } from "../app/format";
import {
  BITS_PREVIEW_MAX,
  ROW_HEIGHT,
  bitsToHex,
  labelColor,
  labelSegments,
  messageOverlapsSelection,
  truncateMiddle,
  visibleRows,
} from "../app/protocolTable";

/** Character budget for the hex column. */
const HEX_PREVIEW_MAX = 28;
/** Minimum label-bar width (fraction of the bits column) that shows a name. */
const LABEL_NAME_MIN = 0.12;

interface ProtocolTableProps {
  result: AnalysisResult | null;
  labels: FieldLabel[] | null;
  /** Shared selection interval (single source of truth), or null. */
  selectionInterval: Interval | null;
  /** Index of the row the user clicked (drives the waveform highlight). */
  selectedRow: number | null;
  onRowClick: (index: number) => void;
}

export function ProtocolTable(props: ProtocolTableProps): JSX.Element {
  const { result, labels, selectionInterval, selectedRow, onRowClick } = props;
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(0);

  // Track the scroll viewport's height (rows-per-view changes with the panel
  // size; the slice must follow).
  useEffect(() => {
    const el = scrollRef.current;
    if (el === null) return;
    const ro = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry !== undefined) setViewportHeight(entry.contentRect.height);
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // A new result resets the scroll position — row indices mean new messages.
  useEffect(() => {
    const el = scrollRef.current;
    if (el !== null) el.scrollTop = 0;
    setScrollTop(0);
  }, [result]);

  const onScroll = useCallback((e: React.UIEvent<HTMLDivElement>): void => {
    setScrollTop(e.currentTarget.scrollTop);
  }, []);

  const messages = result?.messages ?? null;

  if (messages === null) {
    return (
      <div className="pt-empty">
        <span>No analysis yet — set parameters and press Analyze.</span>
      </div>
    );
  }
  if (messages.length === 0) {
    return (
      <div className="pt-empty">
        <span>Analysis found no messages.</span>
      </div>
    );
  }

  const { first, count } = visibleRows(scrollTop, viewportHeight, messages.length);
  const rows = messages.slice(first, first + count);

  return (
    <div className="pt-table">
      <div className="pt-head" role="row">
        <span className="pt-c pt-c-idx">#</span>
        <span className="pt-c pt-c-bits">bits</span>
        <span className="pt-c pt-c-pause">pause</span>
        <span className="pt-c pt-c-hex">hex</span>
        <span className="pt-c pt-c-start">start</span>
      </div>
      <div className="pt-scroll" ref={scrollRef} onScroll={onScroll} role="grid" aria-rowcount={messages.length}>
        <div className="pt-body" style={{ height: messages.length * ROW_HEIGHT }}>
          <div className="pt-rows" style={{ transform: `translateY(${first * ROW_HEIGHT}px)` }}>
            {rows.map((m) => {
              const overlaps = messageOverlapsSelection(m, selectionInterval);
              const selected = selectedRow === m.index;
              const cls = `pt-row${overlaps ? " pt-highlight" : ""}${selected ? " pt-selected" : ""}`;
              const segments = labelSegments(labels ?? [], m.index, m.bitCount);
              return (
                <div key={m.index} className={cls} role="row" onClick={() => onRowClick(m.index)}>
                  <span className="pt-c pt-c-idx">{m.index}</span>
                  <span className="pt-c pt-c-bits">
                    {segments.length > 0 && (
                      <span className="pt-labels">
                        {segments.map((s, i) => (
                          <span
                            key={i}
                            className="pt-label"
                            style={{ left: `${s.left * 100}%`, width: `${s.width * 100}%`, background: labelColor(s.colorIndex) }}
                            title={`${s.name} [${s.bitStart}, ${s.bitEnd})`}
                          >
                            {s.width >= LABEL_NAME_MIN ? s.name : ""}
                          </span>
                        ))}
                      </span>
                    )}
                    <span className="pt-bits">{truncateMiddle(m.plainBits, BITS_PREVIEW_MAX)}</span>
                  </span>
                  <span className="pt-c pt-c-pause">{formatCount(m.pause)}</span>
                  <span className="pt-c pt-c-hex">{truncateMiddle(bitsToHex(m.plainBits), HEX_PREVIEW_MAX)}</span>
                  <span className="pt-c pt-c-start">{formatCount(m.startSample)}</span>
                </div>
              );
            })}
          </div>
        </div>
      </div>
    </div>
  );
}
