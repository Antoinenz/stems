import type { Lyrics } from './types';

/** Index of the line that should be highlighted at `position_ms`, or -1 before the first. */
export function activeLine(lyrics: Lyrics | null, positionMs: number): number {
  if (!lyrics || lyrics.lines.length === 0) return -1;
  const t = positionMs - lyrics.offset_ms;
  // lines are sorted by time; binary search for the last line with time <= t
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
