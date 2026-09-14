<script lang="ts">
  import type { KaraokeState, PlaybackStatus, TrackRef } from './lib/types';

  let {
    track,
    status,
    position,
    karaoke,
    minimal = false
  }: {
    track: TrackRef | null;
    status: PlaybackStatus;
    position: number;
    karaoke: KaraokeState;
    minimal?: boolean;
  } = $props();

  const fmt = (ms: number) => {
    const s = Math.floor(ms / 1000);
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  };
  const progress = $derived(track && track.duration_ms > 0 ? Math.min(1, position / track.duration_ms) : 0);
</script>

{#if track}
  <footer class:minimal>
    {#if track.art && !minimal}<img class="art" src={track.art} alt="" />{/if}
    <div class="meta">
      <div class="title">{track.title}</div>
      <div class="artists">{track.artists.join(', ')}</div>
      <div class="bar"><div class="fill" style:width="{progress * 100}%"></div></div>
      <div class="time">{fmt(position)} / {fmt(track.duration_ms)} {status === 'paused' ? '· paused' : ''}</div>
    </div>
    {#if karaoke.enabled}<div class="badge">KARAOKE</div>{/if}
  </footer>
{/if}

<style>
  footer {
    position: absolute; left: 4vw; right: 4vw; bottom: 4vh;
    display: flex; align-items: center; gap: 2vmin;
  }
  .art { width: 10vmin; height: 10vmin; border-radius: 1vmin; object-fit: cover; box-shadow: 0 6px 30px rgba(0,0,0,.5); }
  .meta { flex: 1; min-width: 0; }
  .title { font-size: 2.6vmin; font-weight: 700; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .artists { font-size: 2vmin; color: var(--dim); margin-bottom: 1vmin; }
  .bar { height: 0.5vmin; background: rgba(255,255,255,.15); border-radius: 1vmin; overflow: hidden; }
  .fill { height: 100%; background: var(--accent); }
  .time { font-size: 1.6vmin; color: var(--dim); margin-top: 0.6vmin; font-variant-numeric: tabular-nums; }
  footer.minimal { bottom: 2vh; opacity: 0.8; }
  footer.minimal .title { font-size: 2vmin; }
  footer.minimal .artists { display: none; }
  footer.minimal .time { display: none; }
  .badge {
    padding: 0.8vmin 1.6vmin; border-radius: 1vmin;
    background: var(--accent); color: #000; font-weight: 800; font-size: 1.8vmin; letter-spacing: 0.1em;
  }
</style>
