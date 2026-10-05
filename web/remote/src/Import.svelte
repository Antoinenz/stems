<script lang="ts">
  import { onMount } from 'svelte';
  import { fetchImportStatus, importSources } from '../../shared/client.svelte';
  import type { ImportStatus } from '../../shared/types';

  let text = $state('');
  let status = $state<ImportStatus | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);

  async function poll() {
    try {
      status = await fetchImportStatus();
    } catch {
      /* not in library mode */
    }
  }
  onMount(() => {
    poll();
    const t = setInterval(poll, 3000);
    return () => clearInterval(t);
  });

  async function submit() {
    const sources = text
      .split(/\s+/)
      .map((s) => s.trim())
      .filter(Boolean);
    if (!sources.length) return;
    busy = true;
    try {
      await importSources(sources);
      text = '';
      error = null;
      await poll();
    } catch (e) {
      error = (e as Error).message;
    } finally {
      busy = false;
    }
  }
</script>

<section class="card">
  <div class="label">Import</div>
  <textarea rows="2" placeholder="Spotify track / album / playlist links" bind:value={text}></textarea>
  <button disabled={busy || !text.trim()} onclick={submit}>Import</button>
  {#if error}<div class="err">{error}</div>{/if}
  {#if status}
    {#if status.current}
      <div class="line">Importing <b>{status.current}</b>{#if status.waiting.length} · {status.waiting.length} waiting{/if}</div>
    {/if}
    {#each status.errors.slice(0, 3) as e, i (i)}<div class="err">{e}</div>{/each}
    {#each status.finished.slice(0, 5) as f (f.key + f.title)}
      <div class="line" class:warn={!f.playable || f.problems.length}>
        {f.playable ? '✓' : '✗'} {f.title}
        {#if f.problems.length}<small>· {f.problems.join('; ')}</small>{/if}
      </div>
    {/each}
  {/if}
</section>

<style>
  textarea { width: 100%; padding: 10px 12px; border-radius: 10px; border: 0; background: rgba(255,255,255,.1); color: var(--fg); font: inherit; font-size: 14px; box-sizing: border-box; resize: vertical; }
  .line { font-size: 13px; color: var(--dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .line b { color: var(--fg); font-weight: 600; }
  .line.warn, .err { color: #e96; }
  .err { font-size: 13px; }
</style>
