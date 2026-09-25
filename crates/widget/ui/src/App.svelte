<script>
  import { onMount, onDestroy } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  const EMPTY = {
    state: "waiting",
    model: "",
    mounted: "",
    rx: 0,
    tx: 0,
    speed_rx: 0,
    speed_tx: 0,
  };

  let status = $state({ ...EMPTY });
  let actionError = $state("");
  let pending = $state(false);
  let unlisten;

  onMount(async () => {
    try {
      status = await invoke("get_status");
    } catch (e) {
      actionError = String(e);
    }
    try {
      unlisten = await listen("status", (event) => {
        status = event.payload;
      });
    } catch (e) {
      actionError = `события статуса недоступны: ${e}`;
    }
  });

  onDestroy(() => unlisten?.());

  const attached = $derived(status.state === "attached");
  const gone = $derived(status.state === "gone");
  const stale = $derived(status.state === "stale");
  const busy = $derived(status.speed_rx + status.speed_tx > 0);
  const icon = $derived(
    attached ? (busy ? "🌉⇅" : "🌉") : gone ? "💤" : stale ? "⚠️" : "🚧",
  );
  const label = $derived(
    attached
      ? status.model || "телефон"
      : gone
        ? "отключён"
        : stale
          ? "демон молчит"
          : "жду телефон",
  );
  const canAct = $derived(attached && !pending);

  // Transfer speed shown prominently. Rates below 1 KiB/s keep one decimal so
  // small transfers do not collapse to "0 KiB/s".
  function rate(bps) {
    if (!bps) return "0";
    if (bps < 1024) return `${bps} B`;
    if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(bps < 10240 ? 1 : 0)} KiB`;
    return `${(bps / (1024 * 1024)).toFixed(1)} MiB`;
  }

  function size(bytes) {
    if (!bytes) return "0 B";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let i = 0;
    let v = bytes;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i += 1;
    }
    return `${v.toFixed(v >= 10 || i === 0 ? 0 : 1)} ${units[i]}`;
  }

  async function act(command) {
    if (pending) return;
    actionError = "";
    pending = true;
    try {
      await invoke(command);
    } catch (e) {
      actionError = String(e);
    } finally {
      pending = false;
    }
  }
</script>

<main class:attached>
  <!-- Drag anywhere in this strip to move the widget. -->
  <header data-tauri-drag-region>
    <span class="glyph" class:busy aria-hidden="true">{icon}</span>
    <span class="title" data-tauri-drag-region>{label}</span>
    <button class="pin" onclick={() => act("unmount_volume")} disabled={!canAct} title="Размонтировать">
      {pending ? "…" : "⏏"}
    </button>
  </header>

  <!-- Live transfer speed: the whole point of the widget. -->
  <section class="speed" aria-live="polite">
    <div class="dir down">
      <span class="arrow">▼</span>
      <strong>{rate(status.speed_rx)}</strong>
      <span class="unit">/s</span>
    </div>
    <div class="dir up">
      <span class="arrow">▲</span>
      <strong>{rate(status.speed_tx)}</strong>
      <span class="unit">/s</span>
    </div>
  </section>

  <div class="meter" aria-hidden="true">
    <div class="meter-fill down" style:width={pct(status.speed_rx)}></div>
    <div class="meter-fill up" style:width={pct(status.speed_tx)}></div>
  </div>

  <section class="foot">
    <span class="totals">⇩ {size(status.rx)} · ⇧ {size(status.tx)}</span>
    <span class="path">{status.mounted || "—"}</span>
  </section>

  {#if actionError}
    <p class="error" role="alert">{actionError}</p>
  {/if}

  <section class="actions">
    <button onclick={() => act("open_volume")} disabled={!canAct}>Открыть том</button>
  </section>
</main>

<script module>
  // Bars saturate at 20 MiB/s (above the USB 2.0 ceiling) for a stable visual.
  function pct(bps) {
    return `${Math.min(100, (bps / (20 * 1024 * 1024)) * 100).toFixed(1)}%`;
  }
</script>

<style>
  :global(body) {
    background: transparent !important;
    overflow: hidden;
  }

  main {
    height: 100%;
    box-sizing: border-box;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    border-radius: 16px;
    background: rgba(20, 26, 40, 0.88);
    backdrop-filter: blur(18px);
    border: 1px solid rgba(255, 255, 255, 0.1);
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
    color: #e8eefc;
    font-family:
      -apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif;
    user-select: none;
    -webkit-user-select: none;
  }

  header {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .glyph {
    font-size: 16px;
  }

  .title {
    flex: 1;
    font-size: 12px;
    font-weight: 600;
    color: #cddaf0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .pin {
    width: 22px;
    height: 22px;
    padding: 0;
    border-radius: 7px;
    border: 1px solid rgba(255, 255, 255, 0.12);
    background: rgba(255, 255, 255, 0.06);
    color: #cddaf0;
    font-size: 12px;
    cursor: pointer;
  }

  .pin:disabled {
    opacity: 0.3;
    cursor: default;
  }

  .speed {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .dir {
    display: flex;
    align-items: baseline;
    gap: 3px;
    padding: 7px 9px;
    border-radius: 11px;
    background: rgba(255, 255, 255, 0.05);
  }

  .dir strong {
    font-size: 19px;
    font-variant-numeric: tabular-nums;
    letter-spacing: -0.02em;
  }

  .dir .arrow {
    font-size: 10px;
  }

  .dir.down .arrow {
    color: #35d0c0;
  }

  .dir.up .arrow {
    color: #4a7bf7;
  }

  .unit {
    font-size: 10px;
    color: #8ea0c0;
  }

  .meter {
    height: 3px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.08);
    overflow: hidden;
  }

  .meter-fill {
    height: 100%;
    border-radius: 2px;
    transition: width 0.25s ease;
  }

  .meter-fill.down {
    background: #35d0c0;
  }

  .meter-fill.up {
    background: #4a7bf7;
  }

  .foot {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: 10px;
    color: #8ea0c0;
  }

  .totals {
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* Keep the path LTR; truncate at the end rather than reordering it. */
    direction: ltr;
    text-align: right;
  }

  .error {
    margin: 0;
    padding: 6px 8px;
    border-radius: 8px;
    background: rgba(255, 107, 107, 0.15);
    color: #ff8f8f;
    font-size: 10px;
  }

  .actions {
    margin-top: auto;
  }

  .actions button {
    width: 100%;
    padding: 6px 10px;
    border-radius: 9px;
    border: none;
    background: linear-gradient(180deg, #35d0c0, #24b3a6);
    color: #06231f;
    font-weight: 600;
    font-size: 12px;
    cursor: pointer;
  }

  .actions button:disabled {
    opacity: 0.35;
    cursor: default;
  }
</style>
