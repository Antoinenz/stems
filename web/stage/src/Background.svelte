<script lang="ts">
  import type { Media, TrackRef, ViewMode } from './lib/types';

  let {
    track,
    media,
    mode,
    position
  }: { track: TrackRef | null; media: Media; mode: ViewMode; position: number } = $props();

  const canvas = $derived(mode === 'canvas' ? media.canvas_url : null);
  const video = $derived(mode === 'video' ? media.video_url : null);

  // Keep the music video on the track's timeline; the loop (canvas) just plays.
  let videoEl: HTMLVideoElement | undefined = $state();
  $effect(() => {
    if (!videoEl || !video) return;
    const want = (position + media.video_offset_ms) / 1000;
    if (Math.abs(videoEl.currentTime - want) > 0.25) videoEl.currentTime = want;
  });
</script>

{#if video}
  <video class="layer video" bind:this={videoEl} src={video} muted autoplay playsinline></video>
  <div class="scrim light"></div>
{:else if canvas}
  <video class="layer canvas" src={canvas} muted autoplay loop playsinline></video>
  <div class="scrim"></div>
{:else if track?.art && mode !== 'karaoke'}
  <img class="layer art" src={track.art} alt="" />
  <div class="scrim"></div>
{:else}
  <div class="layer plain"></div>
{/if}

<style>
  .layer { position: absolute; inset: 0; width: 100%; height: 100%; object-fit: cover; }
  .art {
    inset: -10%; width: 120%; height: 120%;
    filter: blur(40px) brightness(0.45) saturate(1.3);
    transform: scale(1.1);
  }
  .canvas { filter: brightness(0.6); }
  .plain { background: radial-gradient(ellipse at center, #14141c, #05050a); }
  .scrim {
    position: absolute; inset: 0;
    background: radial-gradient(ellipse at center, rgba(0,0,0,0.15), rgba(0,0,0,0.75));
  }
  .scrim.light { background: linear-gradient(to top, rgba(0,0,0,0.75), rgba(0,0,0,0.05) 45%); }
</style>
