<script lang="ts">
  import { onMount } from 'svelte';
  import { StemsClient } from '../../shared/client.svelte';
  import type { ViewMode } from '../../shared/types';
  import Slider from './Slider.svelte';
  import Stepper from './Stepper.svelte';

  const client = new StemsClient();
  onMount(() => {
    client.start();
    return () => client.stop();
  });

  const ps = $derived(client.state);
  const track = $derived(ps.track);
  const position = $derived(client.position);

  const MODES: { id: ViewMode; label: string }[] = [
    { id: 'lyrics', label: 'Lyrics' },
    { id: 'karaoke', label: 'Karaoke' },
    { id: 'video', label: 'Video' },
    { id: 'canvas', label: 'Canvas' }
  ];

  const fmt = (ms: number) => {
    const s = Math.max(0, Math.floor(ms / 1000));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  };
  const progress = $derived(
    track && track.duration_ms > 0 ? Math.min(1, position / track.duration_ms) : 0
  );

  // Local echo so sliders feel instant; the server state wins when it arrives.
  let volume = $state(1);
  let vocal = $state(0);
  $effect(() => {
    volume = ps.volume;
  });
  $effect(() => {
    vocal = ps.karaoke.vocal_gain;
  });

  let deviceName = $state<string | null>(null);
  onMount(async () => {
    try {
      const r = await fetch('/api/info');
      if (r.ok) deviceName = ((await r.json()) as { device_name: string }).device_name;
    } catch {
      /* offline */
    }
  });

  function toggleKaraoke() {
    client.send({ cmd: 'set_karaoke', enabled: !ps.karaoke.enabled });
  }
</script>

<main>
  <header>
    <span class="dot" class:live={client.connected}></span>
    <span class="name">stems</span>
    <span class="status">
      {client.connected ? ps.status : 'connecting…'}
    </span>
  </header>

  <section class="now">
    {#if track}
      {#if track.art}<img class="art" src={track.art} alt="" />{/if}
      <div class="meta">
        <div class="title">{track.title}</div>
        <div class="artists">{track.artists.join(', ')}</div>
        <div class="bar"><div class="fill" style:width="{progress * 100}%"></div></div>
        <div class="time">{fmt(position)} / {fmt(track.duration_ms)}</div>
      </div>
    {:else}
      <div class="idle">
        Nothing playing.<br />
        <small>Pick “{deviceName ?? 'stems'}” in Spotify Connect.</small>
      </div>
    {/if}
  </section>

  <section class="card">
    <button class="big" class:on={ps.karaoke.enabled} onclick={toggleKaraoke}>
      {ps.karaoke.enabled ? 'Karaoke on' : 'Karaoke off'}
      <small>{ps.karaoke.backend ?? ''}</small>
    </button>
    <Slider
      label="Vocals"
      value={vocal}
      disabled={!ps.karaoke.enabled}
      format={(v) => `${Math.round(v * 100)}%`}
      onchange={(v) => {
        vocal = v;
        client.send({ cmd: 'set_vocal_gain', gain: v });
      }}
    />
  </section>

  <section class="card">
    <Slider
      label="Volume"
      value={volume}
      format={(v) => `${Math.round(v * 100)}%`}
      onchange={(v) => {
        volume = v;
        client.send({ cmd: 'set_volume', level: v });
      }}
    />
  </section>

  <section class="card">
    <div class="label">Stage</div>
    <div class="modes">
      {#each MODES as m (m.id)}
        <button
          class:on={ps.view_mode === m.id}
          disabled={(m.id === 'video' && !ps.media.video_url) ||
            (m.id === 'canvas' && !ps.media.canvas_url)}
          onclick={() => client.send({ cmd: 'set_view_mode', mode: m.id })}
        >
          {m.label}
        </button>
      {/each}
    </div>
  </section>

  <section class="card">
    <Stepper
      label="Lyrics timing"
      value={ps.lyrics?.offset_ms ?? 0}
      step={100}
      disabled={!ps.lyrics}
      onchange={(v) => client.send({ cmd: 'set_lyrics_offset', offset_ms: v })}
    />
    {#if ps.media.video_url}
      <Stepper
        label="Video timing"
        value={ps.media.video_offset_ms}
        step={250}
        onchange={(v) => client.send({ cmd: 'set_video_offset', offset_ms: v })}
      />
    {/if}
  </section>

  {#if ps.next_track}
    <section class="next">
      Up next: <b>{ps.next_track.title}</b> · {ps.next_track.artists.join(', ')}
    </section>
  {/if}
</main>

<style>
  main {
    max-width: 480px;
    margin: 0 auto;
    padding: max(12px, env(safe-area-inset-top)) 16px max(24px, env(safe-area-inset-bottom));
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  header { display: flex; align-items: center; gap: 8px; color: var(--dim); font-size: 14px; }
  .name { font-weight: 800; letter-spacing: 0.08em; color: var(--fg); }
  .status { margin-left: auto; text-transform: capitalize; }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: #c33; }
  .dot.live { background: #4c6; }

  .now { display: flex; gap: 14px; align-items: center; min-height: 84px; }
  .art { width: 84px; height: 84px; border-radius: 10px; object-fit: cover; }
  .meta { flex: 1; min-width: 0; }
  .title { font-size: 18px; font-weight: 700; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .artists { color: var(--dim); font-size: 14px; margin-bottom: 8px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .bar { height: 4px; background: rgba(255,255,255,.15); border-radius: 4px; overflow: hidden; }
  .fill { height: 100%; background: var(--accent); }
  .time { font-size: 12px; color: var(--dim); margin-top: 6px; font-variant-numeric: tabular-nums; }
  .idle { color: var(--dim); line-height: 1.5; }

  .card { background: var(--card); border-radius: 16px; padding: 14px; display: flex; flex-direction: column; gap: 12px; }
  .label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.1em; color: var(--dim); }
  .big { font-size: 20px; padding: 18px; display: flex; flex-direction: column; gap: 2px; background: rgba(255,255,255,.12); }
  .big small { font-size: 12px; font-weight: 400; opacity: 0.6; }
  .modes { display: grid; grid-template-columns: repeat(4, 1fr); gap: 8px; }
  .modes button { padding: 12px 4px; font-size: 14px; }
  .modes button:disabled { opacity: 0.3; }
  .next { color: var(--dim); font-size: 13px; padding: 0 4px; }
</style>
