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
  let error = $state("");
  let unlisten;

  onMount(async () => {
    try {
      status = await invoke("get_status");
    } catch (e) {
      error = String(e);
    }
    unlisten = await listen("status", (event) => {
      status = event.payload;
      error = "";
    });
  });

  onDestroy(() => unlisten?.());

  const attached = $derived(status.state === "attached");
  const gone = $derived(status.state === "gone");
  const busy = $derived(status.speed_rx + status.speed_tx > 0);
  const icon = $derived(attached ? (busy ? "🌉⇅" : "🌉") : gone ? "💤" : "🚧");
  const headline = $derived(
    attached
      ? status.model || "телефон"
      : gone
        ? "телефон отключён"
        : "жду телефон",
  );
  const subline = $derived(
    attached
      ? status.mounted
        ? "том смонтирован"
        : "том готовится…"
      : gone
        ? "настройки сохранены — подключите кабель"
        : "кабель + режим «Передача файлов»",
  );

  function rate(bps) {
    if (!bps) return "—";
    const mib = bps / (1024 * 1024);
    return mib >= 1 ? `${mib.toFixed(1)} MiB/s` : `${Math.round(bps / 1024)} KiB/s`;
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
    error = "";
    try {
      await invoke(command);
    } catch (e) {
      error = String(e);
    }
  }
</script>

<main class:attached>
  <header>
    <span class="brand">pereprava</span>
    <span class="pill" class:on={attached} class:off={gone}>
      {attached ? "подключено" : gone ? "оффлайн" : "ожидание"}
    </span>
  </header>

  <section class="hero">
    <div class="glyph" class:busy>{icon}</div>
    <div class="who">
      <h1>{headline}</h1>
      <p>{subline}</p>
    </div>
  </section>

  <section class="speeds">
    <div class="card">
      <span class="label">▼ скачивание</span>
      <strong>{rate(status.speed_rx)}</strong>
      <span class="total">всего {size(status.rx)}</span>
    </div>
    <div class="card">
      <span class="label">▲ загрузка</span>
      <strong>{rate(status.speed_tx)}</strong>
      <span class="total">всего {size(status.tx)}</span>
    </div>
  </section>

  <section class="mount">
    <span class="label">точка монтирования</span>
    <code>{status.mounted || "—"}</code>
  </section>

  {#if error}
    <p class="error">{error}</p>
  {/if}

  <section class="actions">
    <button onclick={() => act("open_volume")} disabled={!status.mounted}>
      Открыть том
    </button>
    <button class="ghost" onclick={() => act("unmount_volume")} disabled={!status.mounted}>
      Размонтировать
    </button>
  </section>
</main>

<style>
  main {
    height: 100%;
    padding: 22px 20px 20px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    background:
      radial-gradient(120% 80% at 100% 0%, rgba(74, 123, 247, 0.18), transparent 60%),
      radial-gradient(120% 80% at 0% 100%, rgba(53, 208, 192, 0.14), transparent 55%),
      var(--bg);
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .brand {
    font-weight: 700;
    letter-spacing: 0.02em;
  }

  .pill {
    font-size: 11px;
    padding: 3px 10px;
    border-radius: 999px;
    background: var(--panel-2);
    color: var(--muted);
  }

  .pill.on {
    background: rgba(53, 208, 192, 0.16);
    color: var(--accent);
  }

  .pill.off {
    background: rgba(255, 107, 107, 0.14);
    color: var(--danger);
  }

  .hero {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 18px;
    border-radius: 16px;
    background: var(--panel);
    border: 1px solid rgba(255, 255, 255, 0.05);
  }

  .glyph {
    font-size: 40px;
    line-height: 1;
    filter: grayscale(0.35) brightness(1.1);
    transition: filter 0.2s ease;
  }

  .glyph.busy {
    filter: none;
  }

  .who h1 {
    margin: 0;
    font-size: 20px;
  }

  .who p {
    margin: 4px 0 0;
    font-size: 12px;
    color: var(--muted);
  }

  .speeds {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
  }

  .card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 14px;
    border-radius: 14px;
    background: var(--panel);
    border: 1px solid rgba(255, 255, 255, 0.05);
  }

  .label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }

  .card strong {
    font-size: 18px;
    font-variant-numeric: tabular-nums;
  }

  .total {
    font-size: 11px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .mount {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .mount code {
    display: block;
    padding: 10px 12px;
    border-radius: 10px;
    background: var(--panel);
    border: 1px solid rgba(255, 255, 255, 0.05);
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text);
  }

  .error {
    margin: 0;
    padding: 10px 12px;
    border-radius: 10px;
    background: rgba(255, 107, 107, 0.12);
    color: var(--danger);
    font-size: 12px;
  }

  .actions {
    margin-top: auto;
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
  }

  button {
    padding: 11px 14px;
    border-radius: 11px;
    border: 1px solid transparent;
    background: linear-gradient(180deg, var(--accent), #24b3a6);
    color: #06231f;
    font-weight: 600;
    font-size: 13px;
    cursor: pointer;
    transition: opacity 0.15s ease, transform 0.05s ease;
  }

  button.ghost {
    background: var(--panel-2);
    color: var(--text);
    border-color: rgba(255, 255, 255, 0.08);
  }

  button:disabled {
    opacity: 0.4;
    cursor: default;
  }

  button:not(:disabled):active {
    transform: translateY(1px);
  }
</style>
