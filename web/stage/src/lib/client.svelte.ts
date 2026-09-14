// WebSocket client + playback clock, as Svelte 5 runes.
//
// The server sends `clock` ticks at ~10 Hz. We keep an estimate of
// (position - performance.now()) and extrapolate between ticks; small
// errors are slewed, large ones (seek, track change) snap.

import type { Command, Inbound, PlaybackState, PlaybackStatus } from './types';

const SNAP_THRESHOLD_MS = 150;
const SLEW = 0.15;

export const EMPTY_STATE: PlaybackState = {
  source: null,
  status: 'idle',
  track: null,
  next_track: null,
  lyrics: null,
  karaoke: { enabled: false, vocal_gain: 0, backend: null },
  view_mode: 'lyrics',
  media: { canvas_url: null, video_url: null, video_offset_ms: 0 },
  volume: 1,
  revision: 0
};

export class StemsClient {
  state = $state<PlaybackState>(EMPTY_STATE);
  connected = $state(false);
  /** Audible position in ms, updated every animation frame. */
  position = $state(0);

  #ws: WebSocket | null = null;
  #offset = 0; // position_ms - performance.now() at last tick
  #status: PlaybackStatus = 'idle';
  #lastTickPos = 0;
  #raf = 0;
  #retry = 0;

  constructor(private url = defaultUrl()) {}

  start() {
    this.#connect();
    const frame = () => {
      this.position = this.#now();
      this.#raf = requestAnimationFrame(frame);
    };
    this.#raf = requestAnimationFrame(frame);
  }

  stop() {
    cancelAnimationFrame(this.#raf);
    this.#ws?.close();
    this.#ws = null;
  }

  send(cmd: Command) {
    if (this.#ws?.readyState === WebSocket.OPEN) this.#ws.send(JSON.stringify(cmd));
  }

  #now(): number {
    if (this.#status !== 'playing') return this.#lastTickPos;
    return Math.max(0, this.#offset + performance.now());
  }

  #connect() {
    const ws = new WebSocket(this.url);
    this.#ws = ws;
    ws.onopen = () => {
      this.connected = true;
      this.#retry = 0;
    };
    ws.onmessage = (ev) => this.#onMessage(JSON.parse(ev.data) as Inbound);
    ws.onclose = () => {
      this.connected = false;
      if (this.#ws !== ws) return;
      const delay = Math.min(10_000, 500 * 2 ** this.#retry++);
      setTimeout(() => this.#connect(), delay);
    };
    ws.onerror = () => ws.close();
  }

  #onMessage(msg: Inbound) {
    switch (msg.type) {
      case 'state': {
        const { type: _t, ...s } = msg;
        this.state = s;
        break;
      }
      case 'clock': {
        this.#lastTickPos = msg.position_ms;
        const wasPlaying = this.#status === 'playing';
        this.#status = msg.status;
        const target = msg.position_ms - performance.now();
        if (!wasPlaying || Math.abs(target - this.#offset) > SNAP_THRESHOLD_MS) {
          this.#offset = target;
        } else {
          this.#offset += (target - this.#offset) * SLEW;
        }
        break;
      }
      case 'error':
        console.warn('server:', msg.message);
    }
  }
}

function defaultUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  return `${proto}://${location.host}/api/ws`;
}
