<script lang="ts">
  import type { LyricLine } from './lib/types';

  let { lines, active }: { lines: LyricLine[]; active: number } = $props();

  // Show a window of lines around the active one; the active line sits at
  // the vertical centre and the list slides as it advances.
  const BEFORE = 2;
  const AFTER = 4;
  const first = $derived(Math.max(0, active - BEFORE));
  const visible = $derived(lines.slice(first, Math.max(first, active) + AFTER + 1));
</script>

<div class="lyrics">
  {#each visible as line, i (first + i)}
    {@const idx = first + i}
    <div
      class="line"
      class:active={idx === active}
      class:past={idx < active}
      style:--d={idx - active}
    >
      {line.text || '♪'}
    </div>
  {/each}
</div>

<style>
  .lyrics {
    display: flex; flex-direction: column; align-items: center; gap: 2.2vmin;
    width: 100%;
    transform: translateY(calc(var(--shift, 0) * 1px));
  }
  .line {
    font-size: 4.6vmin; font-weight: 600; line-height: 1.25;
    text-align: center; max-width: 84vw;
    color: var(--dim);
    opacity: calc(1 - min(1, abs(var(--d)) * 0.28));
    transform: scale(calc(1 - min(0.35, abs(var(--d)) * 0.08)));
    transition: transform 320ms cubic-bezier(.2,.8,.2,1), opacity 320ms ease, color 200ms ease;
    text-wrap: balance;
  }
  .line.active {
    color: var(--fg);
    font-size: 6.2vmin; font-weight: 800;
    text-shadow: 0 2px 30px rgba(0,0,0,0.6);
  }
  .line.past { color: rgba(255,255,255,0.55); }
</style>
