// Mirrors crates/stems-core/src/state.rs (serde snake_case).

export type PlaybackStatus = 'idle' | 'playing' | 'paused';

export interface TrackRef {
  id: { kind: 'spotify' | 'local'; id: string };
  isrc: string | null;
  title: string;
  artists: string[];
  album: string | null;
  duration_ms: number;
  art: string | null;
}

export interface LyricLine {
  time_ms: number;
  text: string;
}

export interface Lyrics {
  provider: string;
  lines: LyricLine[];
  plain: string | null;
  offset_ms: number;
}

export interface KaraokeState {
  enabled: boolean;
  vocal_gain: number;
  backend: string | null;
}

export interface PlaybackState {
  source: string | null;
  status: PlaybackStatus;
  track: TrackRef | null;
  next_track: TrackRef | null;
  lyrics: Lyrics | null;
  karaoke: KaraokeState;
  volume: number;
  revision: number;
}

export interface ClockTick {
  revision: number;
  status: PlaybackStatus;
  position_ms: number;
  server_ms: number;
}

export type Inbound =
  | ({ type: 'state' } & PlaybackState)
  | ({ type: 'clock' } & ClockTick)
  | { type: 'error'; message: string };

export type Command =
  | { cmd: 'set_karaoke'; enabled: boolean }
  | { cmd: 'set_vocal_gain'; gain: number }
  | { cmd: 'set_volume'; level: number }
  | { cmd: 'set_lyrics_offset'; offset_ms: number };
