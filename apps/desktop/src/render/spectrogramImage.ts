/**
 * Pure spectrogram tile → RGBA pixel mapping (no canvas imports, unit-tested).
 *
 * The wire tile is `nTimeBins x freqBins` u8, row-major, row `t` = time bin,
 * column `f` = frequency bin with **DC first** (low frequencies at row
 * start). The output is an RGBA buffer for an offscreen canvas of the same
 * logical size with the vertical axis *flipped* so DC lands at the BOTTOM
 * row — the renderer then only has to scale it into the viewport.
 */

import type { DecodedSpectrogramTile } from "../bridge/protocol";

/** One colormap stop: position in [0, 1] plus its sRGB color. */
interface ColorStop {
  at: number;
  rgb: readonly [number, number, number];
}

/**
 * Inferno-like ramp (dark → purple → red → orange → sand). Black-ish at 0
 * keeps quiet bins close to the app background; the hot end stays legible on
 * it. Control points are on the u8 value curve, so `spectrogramColor(v)`
 * never allocates.
 */
const COLOR_STOPS: readonly ColorStop[] = [
  { at: 0, rgb: [0, 0, 4] },
  { at: 0.25, rgb: [87, 16, 110] },
  { at: 0.5, rgb: [188, 55, 84] },
  { at: 0.75, rgb: [249, 142, 9] },
  { at: 1, rgb: [252, 255, 164] },
];

/**
 * Map a tile byte (0..255 dB-scaled value) to an sRGB color by piecewise
 * linear interpolation between {@link COLOR_STOPS}. Values outside 0..255
 * are clamped.
 */
export function spectrogramColor(v: number): readonly [number, number, number] {
  const t = Math.min(Math.max(Number.isFinite(v) ? v : 0, 0), 255) / 255;
  for (let i = 1; i < COLOR_STOPS.length; i++) {
    const hi = COLOR_STOPS[i];
    if (t <= hi.at || i === COLOR_STOPS.length - 1) {
      const lo = COLOR_STOPS[i - 1];
      const f = (t - lo.at) / (hi.at - lo.at);
      return [
        Math.round(lo.rgb[0] + (hi.rgb[0] - lo.rgb[0]) * f),
        Math.round(lo.rgb[1] + (hi.rgb[1] - lo.rgb[1]) * f),
        Math.round(lo.rgb[2] + (hi.rgb[2] - lo.rgb[2]) * f),
      ];
    }
  }
  return COLOR_STOPS[0].rgb; // unreachable; stops start at 0
}

/** RGBA pixel buffer plus its dimensions, ready for `new ImageData(...)`. */
export interface TilePixels {
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8ClampedArray;
}

/**
 * Convert a decoded tile into an RGBA buffer of `nTimeBins x freqBins`
 * pixels: column `t` is time bin `t` (left = earliest), row 0 is the TOP =
 * highest frequency, the last row is DC (frequency bin 0) — i.e. the
 * vertical flip that puts DC at the bottom.
 */
export function tilePixels(tile: DecodedSpectrogramTile): TilePixels {
  const { nTimeBins, freqBins, data } = tile;
  const rgba = new Uint8ClampedArray(nTimeBins * freqBins * 4);
  for (let t = 0; t < nTimeBins; t++) {
    const rowOff = t * freqBins;
    for (let f = 0; f < freqBins; f++) {
      const [r, g, b] = spectrogramColor(data[rowOff + f]);
      // Flip the frequency axis: f = 0 (DC) writes the last row.
      const y = freqBins - 1 - f;
      const dst = (y * nTimeBins + t) * 4;
      rgba[dst] = r;
      rgba[dst + 1] = g;
      rgba[dst + 2] = b;
      rgba[dst + 3] = 255;
    }
  }
  return { width: nTimeBins, height: freqBins, rgba };
}
