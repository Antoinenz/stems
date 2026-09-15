<script lang="ts">
  import { tick } from 'svelte';
  import { lineProgress, lineWordSpans } from './lib/lyrics';
  import type { Lyrics } from './lib/types';

  let {
    lyrics,
    active,
    position,
    compact = false
  }: { lyrics: Lyrics; active: number; position: number; compact?: boolean } = $props();

  // One long column; we translate it so the active line sits at the centre.
  // Measuring instead of estimating keeps wrapped lines exact.
  let viewport: HTMLDivElement | undefined = $state();
  let column: HTMLDivElement | undefined = $state();
  let lineEls: HTMLDivElement[] = $state([]);
  let shift = $state(0);

  function recenter() {
    if (!viewport || !column) return;
    const target = active >= 0 ? lineEls[active] : lineEls[0];
    if (!target) return;
    const centre = viewport.clientHeight / 2;
    const lineCentre = target.offsetTop + target.offsetHeight / 2;
    shift = centre - lineCentre;
  }

  $effect(() => {
    // re-run when the active line or the lyrics change
    void active;
    void lyrics;
    tick().then(recenter);
  });

  $effect(() => {
    const ro = new ResizeObserver(recenter);
    if (viewport) ro.observe(viewport);
    return () => ro.disconnect();
  });

  const progress = $derived(active >= 0 ? lineProgress(lyrics, active, position) : 0);
  const activeWords = $derived(active >= 0 ? lineWordSpans(lyrics, active) : []);

  /** Fill fraction 0..1 for one word given the line's progress. */
  const wordFill = (w: { start: number; end: number }) =>
    Math.min(1, Math.max(0, (progress - w.start) / Math.max(1e-6, w.end - w.start)));
</script>

<div class="viewport" class:compact bind:this={viewport}>
  <div class="column" bind:this={column} style:transform="translateY({shift}px)">
    {#each lyrics.lines as line, i (i)}
      {@const d = i - active}
      <div
        class="line"
        class:active={i === active}
        class:past={i < active}
        class:hidden={compact ? Math.abs(d) > 1 : Math.abs(d) > 4}
        style:--d={Math.abs(d)}
        bind:this={lineEls[i]}
      >
        {#if i === active && line.text}
          {#each activeWords as w, wi (wi)}
            <span class="word" style:--fill="{wordFill(w) * 100}%">{w.text}</span>{' '}
          {/each}
        {:else}
          {line.text || '♪'}
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .viewport {
    position: absolute; inset: 0;
    overflow: hidden;
    /* soft top/bottom fade */
    mask-image: linear-gradient(to bottom, transparent 4%, #000 22%, #000 78%, transparent 96%);
  }
  .column {
    position: absolute; left: 8vw; right: 8vw; top: 0;
    display: flex; flex-direction: column; align-items: center; gap: 2.4vmin;
    transition: transform 360ms cubic-bezier(0.22, 1, 0.36, 1);
    will-change: transform;
  }
  .line {
    font-size: 4.4vmin; font-weight: 600; line-height: 1.25;
    text-align: center; max-width: 84vw;
    color: rgba(255, 255, 255, 0.38);
    opacity: calc(1 - min(0.85, var(--d) * 0.22));
    transform: scale(calc(1 - min(0.3, var(--d) * 0.07)));
    transform-origin: center;
    /* keep these short: the highlight must land on the beat, not settle after it */
    transition: transform 250ms cubic-bezier(0.22, 1, 0.36, 1), opacity 250ms ease, color 120ms ease,
      font-size 250ms cubic-bezier(0.22, 1, 0.36, 1);
    text-wrap: balance;
  }
  .line.hidden { opacity: 0; }
  .line.past { color: rgba(255, 255, 255, 0.55); }
  .line.active {
    font-size: 6.2vmin; font-weight: 800; color: #fff;
    text-shadow: 0 2px 30px rgba(0, 0, 0, 0.6);
  }

  /* karaoke: bigger, only neighbours visible */
  .compact .column { gap: 4vmin; }
  .compact .line { font-size: 5.2vmin; }
  .compact .line.active { font-size: 8vmin; }

  /* word wipe: gradient text fill driven by --fill */
  .word {
    display: inline-block;
    background: linear-gradient(90deg, var(--accent) var(--fill), rgba(255, 255, 255, 0.92) var(--fill));
    -webkit-background-clip: text;
    background-clip: text;
    color: transparent;
    -webkit-text-fill-color: transparent;
    text-shadow: none;
  }
</style>
