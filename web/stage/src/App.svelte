<script lang="ts">
  import { onMount } from 'svelte';
  import { StemsClient } from './lib/client.svelte';
  import { activeLine } from './lib/lyrics';
  import LyricsView from './LyricsView.svelte';
  import NowPlaying from './NowPlaying.svelte';

  const client = new StemsClient();
  onMount(() => {
    client.start();
    return () => client.stop();
  });

  const track = $derived(client.state.track);
  const lyrics = $derived(client.state.lyrics);
  const active = $derived(activeLine(lyrics, client.position));
</script>

<div class="stage">
  {#if track?.art}
    <img class="bg" src={track.art} alt="" />
  {/if}
  <div class="scrim"></div>

  <main>
    {#if !client.connected}
      <p class="hint">connecting to stems…</p>
    {:else if !track}
      <p class="hint">Pick <b>{client.state.source ?? 'stems'}</b> in Spotify to start</p>
    {:else if lyrics && lyrics.lines.length > 0}
      <LyricsView lines={lyrics.lines} {active} />
    {:else if lyrics?.plain}
      <pre class="plain">{lyrics.plain}</pre>
    {:else}
      <p class="hint">♪</p>
    {/if}
  </main>

  <NowPlaying {track} status={client.state.status} position={client.position} karaoke={client.state.karaoke} />
</div>

<style>
  .stage { position: relative; width: 100%; height: 100%; }
  .bg {
    position: absolute; inset: -10%;
    width: 120%; height: 120%;
    object-fit: cover;
    filter: blur(40px) brightness(0.45) saturate(1.3);
    transform: scale(1.1);
  }
  .scrim {
    position: absolute; inset: 0;
    background: radial-gradient(ellipse at center, rgba(0,0,0,0.1), rgba(0,0,0,0.75));
  }
  main {
    position: absolute; inset: 0;
    display: flex; align-items: center; justify-content: center;
    padding: 6vh 8vw 18vh;
  }
  .hint { font-size: 3vmin; color: var(--dim); }
  .plain {
    font-family: inherit; font-size: 3.2vmin; line-height: 1.5;
    white-space: pre-wrap; text-align: center; max-height: 70vh; overflow: hidden;
    color: var(--dim);
  }
</style>
