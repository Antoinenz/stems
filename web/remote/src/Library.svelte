<script lang="ts">
  import { onMount } from 'svelte';
  import { fetchLibrary } from '../../shared/client.svelte';
  import type { LibraryTrack } from '../../shared/types';

  let {
    host,
    onqueue,
    onplay
  }: {
    host: boolean;
    onqueue: (key: string, singer: string | null) => void;
    onplay: (key: string, singer: string | null) => void;
  } = $props();

  let q = $state('');
  let tracks = $state<LibraryTrack[]>([]);
  let error = $state<string | null>(null);
  let singer = $state(localStorage.getItem('stems.singer') ?? '');

  let timer: ReturnType<typeof setTimeout> | undefined;
  async function load() {
    try {
      tracks = await fetchLibrary(q);
      error = null;
    } catch (e) {
      error = (e as Error).message;
    }
  }
  function search() {
    clearTimeout(timer);
    timer = setTimeout(load, 200);
  }
  onMount(() => {
    load();
    // imports land while the page is open
    const t = setInterval(load, 15_000);
    return () => clearInterval(t);
  });
  $effect(() => {
    localStorage.setItem('stems.singer', singer);
  });

  const who = () => (singer.trim() ? singer.trim() : null);
  const shown = $derived(tracks.slice(0, 100));
</script>

<section class="card">
  <div class="label">Library · {tracks.length} songs</div>
  <input type="search" placeholder="Search songs or artists" bind:value={q} oninput={search} />
  <input type="text" placeholder="Singer (optional)" bind:value={singer} />
  {#if error}<div class="err">{error}</div>{/if}
  <ul>
    {#each shown as t (t.key)}
      <li class:off={!t.playable}>
        {#if t.art}<img src={t.art} alt="" />{:else}<div class="noart"></div>{/if}
        <div class="meta">
          <div class="title">{t.title}</div>
          <div class="sub">
            {t.artists.join(', ')}
            {#if host}
              <span class="badges">
                {#if !t.playable}<b class="warn">not ready</b>{/if}
                {#if !t.lyrics}<b class="warn">no lyrics</b>{:else if !t.words}<b>lines</b>{/if}
                {#if t.video}<b>video</b>{/if}
              </span>
            {/if}
          </div>
        </div>
        <button disabled={!t.playable} onclick={() => onqueue(t.key, who())}>+</button>
        <button disabled={!t.playable} onclick={() => onplay(t.key, who())}>▶</button>
      </li>
    {/each}
  </ul>
  {#if tracks.length > shown.length}
    <div class="more">{tracks.length - shown.length} more — search to narrow down</div>
  {/if}
</section>

<style>
  input { width: 100%; padding: 10px 12px; border-radius: 10px; border: 0; background: rgba(255,255,255,.1); color: var(--fg); font-size: 15px; box-sizing: border-box; }
  ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 6px; max-height: 60vh; overflow-y: auto; }
  li { display: flex; align-items: center; gap: 10px; }
  li.off { opacity: 0.45; }
  img, .noart { width: 40px; height: 40px; border-radius: 6px; object-fit: cover; background: rgba(255,255,255,.08); flex: none; }
  .meta { flex: 1; min-width: 0; }
  .title { font-size: 15px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .sub { font-size: 12px; color: var(--dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .badges b { font-weight: 500; margin-left: 6px; color: var(--dim); }
  .badges b.warn { color: #e96; }
  li button { width: 40px; height: 40px; padding: 0; font-size: 18px; flex: none; }
  .err { color: #e96; font-size: 13px; }
  .more { color: var(--dim); font-size: 12px; text-align: center; }
</style>
