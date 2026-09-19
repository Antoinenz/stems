<script lang="ts">
  import { onMount } from 'svelte';
  import { StemsClient } from '../../shared/client.svelte';
  import { activeLine } from './lib/lyrics';
  import type { ViewMode } from '../../shared/types';
  import Background from './Background.svelte';
  import LyricsView from './LyricsView.svelte';
  import NowPlaying from './NowPlaying.svelte';

  const client = new StemsClient();
  onMount(() => {
    client.start();
    return () => client.stop();
  });

  // `?mode=karaoke` pins this screen; otherwise follow the server.
  const MODES: ViewMode[] = ['lyrics', 'karaoke', 'video', 'canvas'];
  const pinned = new URLSearchParams(location.search).get('mode') as ViewMode | null;
  const mode = $derived<ViewMode>(
    pinned && MODES.includes(pinned) ? pinned : client.state.view_mode
  );

  const track = $derived(client.state.track);
  const lyrics = $derived(client.state.lyrics);
  const active = $derived(activeLine(lyrics, client.position));
  const compact = $derived(mode === 'karaoke');
</script>

<div class="stage" data-mode={mode}>
  <Background {track} media={client.state.media} {mode} position={client.position} />

  <main class:video={mode === 'video'}>
    {#if !client.connected}
      <p class="hint">connecting to stems…</p>
    {:else if !track}
      <p class="hint">Pick <b>{client.state.source ?? 'stems'}</b> in Spotify to start</p>
    {:else if lyrics && lyrics.lines.length > 0}
      <LyricsView {lyrics} {active} position={client.position} {compact} />
    {:else if lyrics?.plain}
      <pre class="plain">{lyrics.plain}</pre>
    {:else}
      <p class="hint">♪</p>
    {/if}
  </main>

  <NowPlaying
    {track}
    status={client.state.status}
    position={client.position}
    karaoke={client.state.karaoke}
    minimal={compact || mode === 'video'}
  />
</div>

<style>
  .stage { position: relative; width: 100%; height: 100%; }
  main { position: absolute; inset: 6vh 0 16vh; }
  main.video { inset: auto 0 12vh; height: 32vh; }
  .hint {
    position: absolute; inset: 0; display: flex; align-items: center; justify-content: center;
    margin: 0; font-size: 3vmin; color: var(--dim);
  }
  .plain {
    position: absolute; inset: 0; margin: 0; padding: 0 8vw;
    display: flex; align-items: center; justify-content: center;
    font-family: inherit; font-size: 3.2vmin; line-height: 1.5;
    white-space: pre-wrap; text-align: center; overflow: hidden; color: var(--dim);
  }
</style>
