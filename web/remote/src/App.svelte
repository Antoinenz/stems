<script lang="ts">
  import { onMount } from 'svelte';
  import { StemsClient, fetchMe, fetchSettings, patchSettings } from '../../shared/client.svelte';
  import type { Me, Permission, ViewMode } from '../../shared/types';
  import Slider from './Slider.svelte';
  import Stepper from './Stepper.svelte';

  const client = new StemsClient();
  // Who we are decides what is shown: guests only get what the host allows.
  let me = $state<Me>({ role: 'guest', permissions: [] });
  const can = (p: Permission) => me.permissions.includes(p);
  onMount(() => {
    client.start();
    fetchMe().then((m) => (me = m)).catch(() => {});
    return () => client.stop();
  });

  // Host: which controls guests get. Applied immediately via the settings API.
  const GUEST_PERMS: { id: Permission; label: string }[] = [
    { id: 'karaoke', label: 'Karaoke on/off' },
    { id: 'vocals', label: 'Vocal level' },
    { id: 'volume', label: 'Volume' },
    { id: 'stage', label: 'Stage view' },
    { id: 'timing', label: 'Timing nudges' }
  ];
  let guestPerms = $state<Record<Permission, boolean> | null>(null);
  async function loadGuestPerms() {
    const s = (await fetchSettings()) as { config: { guests: Record<Permission, boolean> } };
    guestPerms = { ...s.config.guests };
  }
  $effect(() => {
    if (me.role === 'host' && !guestPerms) loadGuestPerms().catch(() => {});
  });
  async function setGuestPerm(id: Permission, on: boolean) {
    if (!guestPerms) return;
    guestPerms = { ...guestPerms, [id]: on };
    try {
      await patchSettings({ guests: { [id]: on } });
    } catch (e) {
      console.warn(e);
    }
  }

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

  // Host stats (CPU, memory, temperatures), polled every 3 s.
  interface SystemStats {
    cpu_percent: number;
    mem_used_mb: number;
    mem_total_mb: number;
    temps: { label: string; celsius: number }[];
  }
  let sys = $state<SystemStats | null>(null);
  onMount(() => {
    let alive = true;
    const poll = async () => {
      try {
        const r = await fetch('/api/system');
        if (r.ok) sys = (await r.json()) as SystemStats;
      } catch {
        /* keep the last sample */
      }
      if (alive) setTimeout(poll, 3000);
    };
    poll();
    return () => {
      alive = false;
    };
  });

  const stems = $derived(ps.karaoke.stems);
  const stemsPct = $derived(
    stems ? Math.round((100 * stems.ready_ms) / Math.max(1, stems.total_ms)) : 0
  );

  function toggleKaraoke() {
    client.send({ cmd: 'set_karaoke', enabled: !ps.karaoke.enabled });
  }
</script>

<main>
  <header>
    <span class="dot" class:live={client.connected}></span>
    <span class="name">stems</span>
    {#if me.role === 'host'}<span class="role">host</span>{/if}
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
    <button
      class="big"
      class:on={ps.karaoke.enabled}
      disabled={!can('karaoke')}
      onclick={toggleKaraoke}
    >
      {ps.karaoke.enabled ? 'Karaoke on' : 'Karaoke off'}
      <small>
        {ps.karaoke.backend ?? ''}
        {#if stems}
          · {stems.done ? 'stems ready' : `preparing ${stemsPct}%`}
          {#if !stems.done && stems.speed}· {stems.speed.toFixed(1)}× realtime{/if}
        {/if}
      </small>
    </button>
    {#if stems && !stems.done}
      <div class="bar"><div class="fill" style:width="{stemsPct}%"></div></div>
    {/if}
    <Slider
      label="Vocals"
      value={vocal}
      disabled={!ps.karaoke.enabled || !can('vocals')}
      format={(v) => `${Math.round(v * 100)}%`}
      onchange={(v) => {
        vocal = v;
        client.send({ cmd: 'set_vocal_gain', gain: v });
      }}
    />
  </section>

  {#if can('volume')}
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
  {/if}

  <section class="card">
    <div class="label">Stage</div>
    <div class="modes">
      {#each MODES as m (m.id)}
        <button
          class:on={ps.view_mode === m.id}
          disabled={!can('stage') ||
            (m.id === 'video' && !ps.media.video_url) ||
            (m.id === 'canvas' && !ps.media.canvas_url)}
          onclick={() => client.send({ cmd: 'set_view_mode', mode: m.id })}
        >
          {m.label}
        </button>
      {/each}
    </div>
  </section>

  {#if can('timing')}
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

  {/if}

  {#if ps.next_track}
    <section class="next">
      Up next: <b>{ps.next_track.title}</b> · {ps.next_track.artists.join(', ')}
    </section>
  {/if}

  {#if me.role === 'host' && guestPerms}
    <section class="card">
      <div class="label">Guests may</div>
      <div class="perms">
        {#each GUEST_PERMS as p (p.id)}
          <label>
            <input
              type="checkbox"
              checked={guestPerms[p.id]}
              onchange={(e) => setGuestPerm(p.id, (e.currentTarget as HTMLInputElement).checked)}
            />
            {p.label}
          </label>
        {/each}
      </div>
    </section>
  {/if}

  {#if sys}
    <section class="sys">
      cpu {Math.round(sys.cpu_percent)}% · mem {Math.round(sys.mem_used_mb / 1024)}/{Math.round(sys.mem_total_mb / 1024)} GB
      {#each sys.temps as t (t.label)}
        · {t.label.toLowerCase().replace(/temperature|sensor/g, '').trim()} {Math.round(t.celsius)}°
      {/each}
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
  .role { font-size: 11px; text-transform: uppercase; letter-spacing: 0.1em; color: var(--accent); border: 1px solid var(--accent); border-radius: 6px; padding: 1px 6px; }
  .perms { display: grid; grid-template-columns: 1fr 1fr; gap: 8px 12px; font-size: 14px; }
  .perms label { display: flex; align-items: center; gap: 8px; }
  .perms input { width: 18px; height: 18px; accent-color: var(--accent); }

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
  .sys { color: var(--dim); font-size: 12px; padding: 0 4px; font-variant-numeric: tabular-nums; }
</style>
