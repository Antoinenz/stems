<script lang="ts">
  let {
    label,
    value,
    disabled = false,
    format = (v: number) => String(v),
    onchange
  }: {
    label: string;
    value: number;
    disabled?: boolean;
    format?: (v: number) => string;
    onchange: (v: number) => void;
  } = $props();

  // Throttle while dragging so a swipe does not flood the socket.
  let pending: number | null = null;
  let timer = 0;
  function input(e: Event) {
    pending = Number((e.target as HTMLInputElement).value) / 100;
    if (!timer) {
      timer = window.setTimeout(() => {
        timer = 0;
        if (pending !== null) onchange(pending);
        pending = null;
      }, 60);
    }
  }
</script>

<label class:disabled>
  <span class="row"><span class="name">{label}</span><span class="val">{format(value)}</span></span>
  <input type="range" min="0" max="100" value={Math.round(value * 100)} {disabled} oninput={input} />
</label>

<style>
  label { display: flex; flex-direction: column; gap: 6px; }
  label.disabled { opacity: 0.4; }
  .row { display: flex; justify-content: space-between; font-size: 14px; }
  .name { text-transform: uppercase; letter-spacing: 0.1em; font-size: 12px; color: var(--dim); }
  .val { font-variant-numeric: tabular-nums; }
</style>
