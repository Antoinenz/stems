import type { Lyrics } from '../../../shared/types';

/** Longest a line stays "in progress" when the next timestamp is far away. */
const MAX_LINE_MS = 6000;

/** Index of the line that should be highlighted at `position_ms`, or -1 before the first. */
export function activeLine(lyrics: Lyrics | null, positionMs: number): number {
  if (!lyrics || lyrics.lines.length === 0) return -1;
  const t = positionMs - lyrics.offset_ms;
  let lo = 0;
  let hi = lyrics.lines.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (lyrics.lines[mid].time_ms <= t) {
      ans = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return ans;
}

/**
 * Progress 0..1 through line `idx` at `positionMs`, assuming the line is sung
 * evenly until the next line starts (line-level LRC has no word timings).
 */
export function lineProgress(lyrics: Lyrics, idx: number, positionMs: number): number {
  const line = lyrics.lines[idx];
  if (!line) return 0;
  const t = positionMs - lyrics.offset_ms;
  const next = lyrics.lines[idx + 1]?.time_ms ?? line.time_ms + MAX_LINE_MS;
  const span = Math.min(MAX_LINE_MS, Math.max(1, next - line.time_ms));
  return Math.min(1, Math.max(0, (t - line.time_ms) / span));
}

export interface WordSpan {
  text: string;
  /** Range this word covers, 0..1: character fraction (interpolated) or time fraction (timed). */
  start: number;
  end: number;
}

/**
 * Word spans for a line. With real word timings the ranges are time
 * fractions of the line's span, so the wipe follows the singer exactly;
 * without them, character fractions (even pacing).
 */
export function lineWordSpans(lyrics: Lyrics, idx: number): WordSpan[] {
  const line = lyrics.lines[idx];
  if (!line) return [];
  const words = line.words;
  if (!words || words.length === 0) return wordSpans(line.text);
  const next = lyrics.lines[idx + 1]?.time_ms ?? line.time_ms + MAX_LINE_MS;
  const end = Math.min(next, line.time_ms + MAX_LINE_MS);
  const span = Math.max(1, end - line.time_ms);
  return words.map((w, i) => {
    const wEnd = words[i + 1]?.time_ms ?? end;
    return {
      text: w.text,
      start: Math.max(0, (w.time_ms - line.time_ms) / span),
      end: Math.min(1, (wEnd - line.time_ms) / span)
    };
  });
}

/** Split a line into words carrying their character-fraction range (for the wipe). */
export function wordSpans(text: string): WordSpan[] {
  const parts = text.split(/(\s+)/).filter((p) => p.length > 0);
  const total = text.length || 1;
  const out: WordSpan[] = [];
  let pos = 0;
  for (const p of parts) {
    if (!/^\s+$/.test(p)) out.push({ text: p, start: pos / total, end: (pos + p.length) / total });
    pos += p.length;
  }
  return out;
}
