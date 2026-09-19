<script lang="ts">
  let {
    label,
    value,
    step,
    disabled = false,
    onchange
  }: {
    label: string;
    value: number;
    step: number;
    disabled?: boolean;
    onchange: (v: number) => void;
  } = $props();

  const fmt = (v: number) => (v > 0 ? `+${v}` : String(v)) + ' ms';
</script>

<div class="stepper" class:disabled>
  <span class="name">{label}</span>
  <button {disabled} onclick={() => onchange(value - step)} aria-label="earlier">−</button>
  <span class="val">{fmt(value)}</span>
  <button {disabled} onclick={() => onchange(value + step)} aria-label="later">+</button>
  <button class="reset" disabled={disabled || value === 0} onclick={() => onchange(0)}>reset</button>
</div>

<style>
  .stepper { display: flex; align-items: center; gap: 8px; }
  .stepper.disabled { opacity: 0.4; }
  .name { flex: 1; text-transform: uppercase; letter-spacing: 0.1em; font-size: 12px; color: var(--dim); }
  .val { min-width: 76px; text-align: center; font-variant-numeric: tabular-nums; font-size: 14px; }
  button { width: 44px; height: 40px; padding: 0; font-size: 20px; }
  .reset { width: auto; font-size: 12px; padding: 0 10px; }
  .reset:disabled { opacity: 0.3; }
</style>
