<script lang="ts">
  import { onMount } from "svelte";
  import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
  import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { startDrag } from "@crabnebula/tauri-plugin-drag";
  import {
    api, fileUrl, inTauri, timecode, bytes, highlight,
    type Status, type Results, type Hit, type Filters, type Line, type Select, type ExportFormat, type SettingsView,
  } from "$lib/api";
  import "../app.css";

  const EXAMPLES = [
    "drone shot over a beach at sunset",
    "someone laughing at a dinner table",
    "猫が窓辺で寝ている",
    "close-up of hands typing on a laptop",
    "persona corriendo bajo la lluvia",
    "city street at night with neon lights",
    "crianças brincando no parque",
    "a dog catching a frisbee",
    "पहाड़ों पर बर्फ़",
    "when she talks about the budget",
  ];
  const DATES: [string, number | null][] = [["Any time", null], ["Past week", 7], ["Past month", 31], ["Past year", 365]];
  const EXPORTS: { format: ExportFormat; label: string; hint: string; ext: string }[] = [
    { format: "fcpxml", label: "Final Cut Pro / DaVinci Resolve", hint: "FCPXML timeline", ext: "fcpxml" },
    { format: "xml", label: "Premiere Pro", hint: "XML timeline", ext: "xml" },
    { format: "edl", label: "EDL", hint: "Avid, Resolve, Premiere", ext: "edl" },
    { format: "csv", label: "Spreadsheet", hint: "CSV with timecodes", ext: "csv" },
    { format: "clips", label: "Render clips to a folder", hint: "MP4 / WAV files", ext: "" },
  ];

  type Tab = "all" | "moments" | "said" | "sounds" | "photos";
  let status = $state<Status | null>(null);
  let query = $state("");
  let lastQuery = $state("");
  let results = $state<Results | null>(null);
  let searching = $state(false);
  let error = $state("");
  let tab = $state<Tab>("all");
  let selected = $state<Hit | null>(null);
  let selectedKind = $state<"moment" | "said" | "sound" | "photo">("moment");
  let previewSrc = $state("");
  let previewBusy = $state(false);
  let clipPath = $state("");
  let lines = $state<Line[]>([]);
  let dropActive = $state(false);
  let searchLabel = $state("");
  let exampleIdx = $state(0);
  let toast = $state("");
  let input: HTMLInputElement;
  let hoverTimer: ReturnType<typeof setTimeout> | null = null;
  let hoverId = $state(-1);
  let hoverSrc = $state("");
  let showWeak = $state(false);
  // filters
  let kind = $state("");
  let folder = $state("");
  let days = $state<number | null>(null);
  // selects
  let selects = $state<Select[]>([]);
  let showSelects = $state(false);
  let handles = $state(1);
  let exporting = $state(false);
  let lastSearch: (() => Promise<Results>) | null = null;
  // settings
  let showSettings = $state(false);
  let sv = $state<SettingsView | null>(null);
  const LANGS: [string, string][] = [
    ["auto", "Detect automatically"], ["en", "English"], ["es", "Spanish"], ["pt", "Portuguese"], ["fr", "French"],
    ["de", "German"], ["it", "Italian"], ["nl", "Dutch"], ["pl", "Polish"], ["tr", "Turkish"], ["ru", "Russian"],
    ["uk", "Ukrainian"], ["ar", "Arabic"], ["hi", "Hindi"], ["ur", "Urdu"], ["bn", "Bengali"], ["id", "Indonesian"],
    ["vi", "Vietnamese"], ["th", "Thai"], ["ja", "Japanese"], ["ko", "Korean"], ["zh", "Chinese"],
  ];

  const ready = $derived(status?.runtime.state === "ready");
  const ix = $derived(status?.index);
  const indexing = $derived(!!ix && (ix.files_pending > 0 || !!ix.scanning));
  const transcribing = $derived(!!ix && !indexing && status?.transcribe && ix.speech_pending > 0);
  const pct = $derived(ix && ix.files_total ? Math.round((100 * ix.files_done) / ix.files_total) : 0);
  const speechPct = $derived(
    ix && ix.speech_done + ix.speech_pending ? Math.round((100 * ix.speech_done) / (ix.speech_done + ix.speech_pending)) : 0,
  );
  const filters = $derived<Filters>({
    kinds: kind ? [kind] : [],
    folders: folder ? [folder] : [],
    since: days ? Math.floor(Date.now() / 1000) - days * 86400 : null,
    until: null,
  });

  const strongOf = (hs: Hit[]) => hs.filter((h) => h.strong);
  const weakOf = (hs: Hit[]) => hs.filter((h) => !h.strong);
  const visible = (hs: Hit[], allTab: number) => {
    // the Sounds tab always lists the closest sounds: typed text -> sound is beta and never "strong"
    const list = showWeak || (tab === "sounds" && hs === results?.sounds) ? hs : strongOf(hs);
    return tab === "all" ? list.slice(0, allTab) : list;
  };
  const counts = $derived(
    results
      ? {
          m: strongOf(results.moments).length,
          w: strongOf(results.said).length,
          s: strongOf(results.sounds).length,
          p: strongOf(results.photos).length,
        }
      : null,
  );
  const weakTotal = $derived(
    results ? [results.moments, results.said, results.sounds, results.photos].reduce((n, l) => n + weakOf(l).length, 0) : 0,
  );
  const selectedKey = (h: Hit) => `${h.file_id}:${h.t0.toFixed(2)}`;
  const inSelects = $derived(new Set(selects.map((s) => `${s.file_id}:${s.t0.toFixed(2)}`)));

  async function refresh() {
    try {
      status = await api.status();
    } catch (e) {
      console.error(e);
    }
  }

  async function loadSelects() {
    try {
      selects = await api.selects();
    } catch {}
  }

  onMount(() => {
    refresh();
    loadSelects();
    const t = setInterval(refresh, 800);
    const ex = setInterval(() => (exampleIdx = (exampleIdx + 1) % EXAMPLES.length), 3200);
    const onKey = (e: KeyboardEvent) => {
      const typing = document.activeElement instanceof HTMLInputElement || document.activeElement instanceof HTMLTextAreaElement;
      if (e.key === "/" && !typing) {
        e.preventDefault();
        input?.focus();
      } else if (e.key === "Escape") {
        if (selected) selected = null;
        else if (showSettings) showSettings = false;
        else if (showSelects) showSelects = false;
      } else if (e.key.toLowerCase() === "s" && !typing && selected && selectedKind !== "photo") {
        addSelect(selected);
      } else if (e.key.startsWith("Arrow") && !selected && !showSelects && !showSettings) {
        if (typing && !(document.activeElement === input && e.key === "ArrowDown")) return;
        if (moveFocus(e.key)) e.preventDefault();
      }
    };
    window.addEventListener("keydown", onKey);
    let unlisten: (() => void) | undefined;
    if (inTauri)
      getCurrentWebview()
        .onDragDropEvent(async (ev) => {
          const p = ev.payload;
          if (p.type === "enter") dropActive = true;
          else if (p.type === "leave") dropActive = false;
          else if (p.type === "drop") {
            dropActive = false;
            for (const path of p.paths) {
              if (await api.isDir(path)) {
                await api.addFolder(path);
                flash(`Added ${path}`);
              } else {
                await runFileSearch(path);
                break;
              }
            }
          }
        })
        .then((u) => (unlisten = u));
    return () => {
      clearInterval(t);
      clearInterval(ex);
      window.removeEventListener("keydown", onKey);
      unlisten?.();
    };
  });

  // re-run the current search when a filter changes
  let lastFilterKey = "";
  $effect(() => {
    const key = JSON.stringify([kind, folder, days]);
    if (key !== lastFilterKey) {
      const first = lastFilterKey === "";
      lastFilterKey = key;
      if (!first && lastSearch) rerun();
    }
  });

  /** Arrow keys move between results; grids move by row with ↑/↓. */
  function moveFocus(key: string): boolean {
    const els = [...document.querySelectorAll<HTMLElement>(".results [data-nav]")];
    if (!els.length) return false;
    const i = els.indexOf(document.activeElement as HTMLElement);
    if (i < 0) {
      els[0].focus();
      return true;
    }
    const cur = els[i];
    const rowPeers = els.filter((el) => el.parentElement === cur.parentElement);
    const cols = Math.max(1, rowPeers.filter((el) => el.offsetTop === rowPeers[0].offsetTop).length);
    const step = key === "ArrowRight" ? 1 : key === "ArrowLeft" ? -1 : key === "ArrowDown" ? cols : -cols;
    const next = els[Math.min(els.length - 1, Math.max(0, i + step))];
    next.focus();
    next.scrollIntoView({ block: "nearest" });
    return true;
  }

  async function openSettings() {
    sv = await api.getSettings();
    showSettings = true;
  }

  async function saveSettings() {
    if (!sv) return;
    await api.setSettings(sv.settings);
    flash("Settings saved");
  }

  function flash(msg: string) {
    toast = msg;
    setTimeout(() => (toast = ""), 2600);
  }

  async function run(fn: () => Promise<Results>, label = "") {
    searching = true;
    error = "";
    lastSearch = fn;
    try {
      results = await fn();
      showWeak = false;
      searchLabel = label;
      tab = "all";
    } catch (e) {
      error = String(e);
    } finally {
      searching = false;
    }
  }

  async function rerun() {
    if (!lastSearch) return;
    searching = true;
    try {
      results = await lastSearch();
    } catch (e) {
      error = String(e);
    } finally {
      searching = false;
    }
  }

  function submit(e?: Event) {
    e?.preventDefault();
    const q = query.trim();
    if (!q) return;
    lastQuery = q;
    run(() => api.search(q, filters));
  }

  function tryExample() {
    query = EXAMPLES[exampleIdx];
    submit();
  }

  async function runFileSearch(path: string) {
    query = "";
    lastQuery = "";
    await run(() => api.searchByFile(path, filters), `Similar to ${path.split(/[\\/]/).pop()}`);
  }

  async function similar(h: Hit) {
    selected = null;
    query = "";
    lastQuery = "";
    await run(
      () => api.searchSimilar(h.item_id, filters),
      `More like ${h.name}${h.file_kind !== "image" ? " @ " + timecode(h.t0) : ""}`,
    );
  }

  async function addFolder() {
    const dir = await openDialog({ directory: true, multiple: true, title: "Add folders to your library" });
    for (const d of Array.isArray(dir) ? dir : dir ? [dir] : []) await api.addFolder(d);
    refresh();
  }

  async function playAt(h: Hit, t0: number, t1: number) {
    previewSrc = "";
    clipPath = "";
    if (h.file_kind === "image") return;
    previewBusy = true;
    try {
      previewSrc = fileUrl(await api.preview(h.path, t0, Math.max(t1, t0 + 4)));
    } catch (e) {
      error = String(e);
    } finally {
      previewBusy = false;
    }
    api.exportClip(h.path, t0, Math.max(t1, t0 + 3), 1.0, null)
      .then((p) => {
        if (selected === h) clipPath = p;
      })
      .catch(() => {});
  }

  async function select(h: Hit, k: typeof selectedKind) {
    selected = h;
    selectedKind = k;
    lines = [];
    if (h.file_kind !== "image") api.transcript(h.file_id).then((l) => (selected === h ? (lines = l) : null));
    await playAt(h, h.t0, h.t1);
  }

  function jumpTo(l: Line) {
    if (!selected) return;
    const h = { ...selected, t0: l.t0, t1: Math.max(l.t1, l.t0 + 3), text: l.text, tc: l.tc };
    selected = h;
    playAt(h, h.t0, h.t1);
  }

  function hoverIn(h: Hit) {
    hoverId = h.item_id;
    hoverSrc = "";
    if (hoverTimer) clearTimeout(hoverTimer);
    hoverTimer = setTimeout(async () => {
      try {
        const p = await api.preview(h.path, h.t0, Math.max(h.t1, h.t0 + 4));
        if (hoverId === h.item_id) hoverSrc = fileUrl(p);
      } catch {}
    }, 350);
  }

  function hoverOut() {
    hoverId = -1;
    hoverSrc = "";
    if (hoverTimer) clearTimeout(hoverTimer);
  }

  async function addSelect(h: Hit, e?: Event) {
    e?.stopPropagation();
    if (inSelects.has(selectedKey(h))) {
      flash("Already in selects");
      return;
    }
    await api.addSelect(h.file_id, h.t0, Math.max(h.t1, h.t0 + 3), h.text ?? null);
    await loadSelects();
    flash(`Added to selects (${selects.length})`);
  }

  async function removeSelect(id: number) {
    await api.removeSelect(id);
    await loadSelects();
  }

  async function exportSelects(f: (typeof EXPORTS)[number]) {
    let dest: string | null = null;
    if (f.format === "clips") {
      const d = await openDialog({ directory: true, title: "Render selects into this folder" });
      dest = Array.isArray(d) ? d[0] : d;
    } else {
      dest = await saveDialog({ title: `Export for ${f.label}`, defaultPath: `Scrubless Selects.${f.ext}`, filters: [{ name: f.hint, extensions: [f.ext] }] });
    }
    if (!dest) return;
    exporting = true;
    try {
      const out = await api.exportSelects(f.format, dest, handles);
      flash(f.format === "clips" ? "Clips rendered" : `Exported — import it in ${f.label.split(" /")[0]}`);
      revealItemInDir(out);
    } catch (e) {
      error = String(e);
    } finally {
      exporting = false;
    }
  }

  async function exportSelected() {
    if (!selected) return;
    const h = selected;
    const isAudio = h.file_kind === "audio";
    const stem = h.name.replace(/\.[^.]+$/, "");
    const dest = await saveDialog({
      title: "Export clip",
      defaultPath: `${stem} @${timecode(h.t0).replace(/:/g, "-")}.${isAudio ? "wav" : "mp4"}`,
      filters: [{ name: isAudio ? "WAV audio" : "MP4 video", extensions: [isAudio ? "wav" : "mp4"] }],
    });
    if (!dest) return;
    flash("Exporting…");
    try {
      const out = await api.exportClip(h.path, h.t0, Math.max(h.t1, h.t0 + 3), 1.0, dest);
      flash("Clip exported");
      revealItemInDir(out);
    } catch (e) {
      error = String(e);
    }
  }

  async function dragOut(e: MouseEvent) {
    if (!clipPath || !inTauri) return;
    e.preventDefault();
    try {
      await startDrag({ item: [clipPath], icon: selected?.thumb ?? clipPath });
    } catch (err) {
      error = String(err);
    }
  }

  function copyTimecode() {
    if (!selected) return;
    navigator.clipboard.writeText(`${selected.name} ${selected.tc}`);
    flash("Copied clip name and timecode");
  }

  const dirOf = (p: string) => p.split(/[\\/]/).slice(-3, -1).join("/");
  const isCurrent = (l: Line) => !!selected && l.t0 <= selected.t0 + 0.05 && selected.t0 < Math.max(l.t1, l.t0 + 0.5);
</script>

{#snippet card(h: Hit)}
  <div class="card" class:weak={!h.strong} role="button" tabindex="0" data-nav
    onclick={() => select(h, "moment")} onkeydown={(e) => e.key === "Enter" && select(h, "moment")}
    onmouseenter={() => hoverIn(h)} onmouseleave={hoverOut}>
    <div class="thumb">
      <img src={fileUrl(h.thumb)} alt="" loading="lazy" />
      {#if hoverId === h.item_id && hoverSrc}<video src={hoverSrc} autoplay muted loop playsinline></video>{/if}
      <span class="tc">{h.tc}</span>
      <button class="add" class:on={inSelects.has(selectedKey(h))} title="Add to selects (S)" onclick={(e) => addSelect(h, e)}>
        {inSelects.has(selectedKey(h)) ? "✓" : "+"}
      </button>
    </div>
    <div class="meta">
      <span class="name" title={h.path}>{h.name}</span>
      <span class="dir">{dirOf(h.path)}</span>
    </div>
  </div>
{/snippet}

{#snippet saidRow(h: Hit)}
  <div class="said-row" class:weak={!h.strong} role="button" tabindex="0" data-nav
    onclick={() => select(h, "said")} onkeydown={(e) => e.key === "Enter" && select(h, "said")}>
    <div class="said-thumb">
      {#if h.thumb}<img src={fileUrl(h.thumb)} alt="" loading="lazy" />{:else}<span>♪</span>{/if}
    </div>
    <div class="said-body">
      <q>{#each highlight(h.text ?? "", lastQuery) as p}{#if p.m}<mark>{p.t}</mark>{:else}{p.t}{/if}{/each}</q>
      <span class="dir"><span class="tcs">{h.tc}</span> · {h.name}</span>
    </div>
    {#if h.exact}<span class="badge">exact</span>{/if}
    <button class="add inline" class:on={inSelects.has(selectedKey(h))} title="Add to selects" onclick={(e) => addSelect(h, e)}>
      {inSelects.has(selectedKey(h)) ? "✓" : "+"}
    </button>
  </div>
{/snippet}

<div class="app">
  <aside class="side">
    <div class="brand">
      <svg viewBox="0 0 24 24" width="22" height="22" aria-hidden="true"
        ><circle cx="10.5" cy="10.5" r="6.5" fill="none" stroke="currentColor" stroke-width="2.2" /><path
          d="M15.5 15.5 21 21" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" /><path
          d="M8 10.5h5M10.5 8v5" stroke="var(--accent)" stroke-width="2" stroke-linecap="round" /></svg>
      <span>scrubless</span>
    </div>

    <section>
      <div class="sec-head">
        <h3>Library</h3>
        <button class="ghost small" onclick={addFolder} title="Add folder">+ Add</button>
      </div>
      {#if status?.folders.length}
        <ul class="folders">
          {#each status.folders as f}
            <li title={f} class:active={folder === f}>
              <button class="fbtn" onclick={() => (folder = folder === f ? "" : f)} title="Search only this folder">
                <span class="fname">{f.split(/[\\/]/).pop() || f}</span>
                <span class="fpath">{f}</span>
              </button>
              <button class="x" title="Remove from library" onclick={() => api.removeFolder(f).then(refresh)}>×</button>
            </li>
          {/each}
        </ul>
      {:else}
        <p class="muted small">Add the folders with your footage, photos and sounds. Nothing leaves this computer.</p>
      {/if}
    </section>

    <button class="selects-btn" class:has={selects.length > 0} onclick={() => (showSelects = !showSelects)}>
      <span>Selects</span><span class="count">{selects.length}</span>
    </button>

    <section class="engine">
      {#if status}
        {#if status.runtime.state === "downloading"}
          <h3>Downloading the search model</h3>
          <div class="bar"><div style="width:{(100 * status.runtime.done) / Math.max(1, status.runtime.total)}%"></div></div>
          <p class="muted small">{bytes(status.runtime.done)} / {bytes(status.runtime.total)} · one time only</p>
        {:else if status.runtime.state === "starting" || status.runtime.state === "needs_model"}
          <h3><span class="dot warm"></span>Starting engine…</h3>
          <p class="muted small">The first start on a GPU takes a minute while it gets ready.</p>
        {:else if status.runtime.state === "failed"}
          <h3><span class="dot bad"></span>Engine failed</h3>
          <p class="muted small">{status.runtime.error}</p>
        {:else if ix}
          <h3>
            <span class="dot {indexing || transcribing ? (ix.paused ? 'warm' : 'busy') : 'ok'}"></span>
            {indexing ? (ix.paused ? "Paused" : "Indexing") : transcribing ? (ix.paused ? "Paused" : "Transcribing") : "Up to date"}
            <span class="device">{status.runtime.state === "ready" ? status.runtime.device.toUpperCase() : ""}</span>
          </h3>
          {#if indexing}
            <div class="bar"><div style="width:{pct}%"></div></div>
            <p class="muted small">
              {ix.files_done} / {ix.files_total} files
              {#if ix.current}· <span class="cur" title={ix.current}>{ix.current.split(/[\\/]/).pop()}</span>{/if}
              {#if ix.scanning}· scanning…{/if}
            </p>
          {:else if transcribing}
            <div class="bar"><div style="width:{speechPct}%"></div></div>
            <p class="muted small">
              {#if status.speech.state === "downloading"}Downloading speech model · {bytes(status.speech.done)} / {bytes(status.speech.total)}
              {:else}Listening to {ix.speech_done} / {ix.speech_done + ix.speech_pending} files
                {#if ix.transcribing}· <span class="cur" title={ix.transcribing}>{ix.transcribing.split(/[\\/]/).pop()}</span>{/if}{/if}
            </p>
          {:else}
            <p class="muted small">
              {ix.files_done.toLocaleString()} files · {ix.items.toLocaleString()} searchable moments{ix.speech_done ? ` · ${ix.speech_done} transcribed` : ""}{ix.files_failed ? ` · ${ix.files_failed} unreadable` : ""}
            </p>
          {/if}
          <div class="row-btns">
            {#if indexing || transcribing}<button class="ghost small" onclick={() => api.setPaused(!ix?.paused)}>{ix.paused ? "Resume" : "Pause"}</button>
            {:else}<button class="ghost small" onclick={() => api.rescan()}>Rescan</button>{/if}
            <label class="toggle small" title="Transcribe speech so you can search what people say">
              <input type="checkbox" checked={status.transcribe} onchange={(e) => api.setTranscribe((e.currentTarget as HTMLInputElement).checked)} />
              Speech
            </label>
          </div>
        {/if}
      {/if}
    </section>

    <footer class="foot">
      <span class="muted small">100% offline</span>
      <button class="ghost small" onclick={openSettings} title="Settings">Settings</button>
    </footer>
  </aside>

  <main>
    <form class="search" onsubmit={submit}>
      <svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true"
        ><circle cx="10.5" cy="10.5" r="6.5" fill="none" stroke="currentColor" stroke-width="2" /><path
          d="M15.5 15.5 21 21" stroke="currentColor" stroke-width="2" stroke-linecap="round" /></svg>
      <input
        bind:this={input}
        bind:value={query}
        placeholder={ready ? `Describe a shot or what's said… e.g. “${EXAMPLES[exampleIdx]}”` : "Getting ready…"}
        disabled={!ready}
        spellcheck="false"
      />
      {#if searching}<span class="spinner"></span>{/if}
      <kbd>/</kbd>
    </form>

    <div class="filters">
      <div class="chips">
        {#each [["", "All"], ["video", "Videos"], ["image", "Photos"], ["audio", "Audio"]] as [k, label]}
          <button class:on={kind === k} onclick={() => (kind = k)}>{label}</button>
        {/each}
      </div>
      <select bind:value={days} title="Modified">
        {#each DATES as [label, d]}<option value={d}>{label}</option>{/each}
      </select>
      {#if folder}<button class="chip-on" onclick={() => (folder = "")} title="Clear folder filter">{folder.split(/[\\/]/).pop()} ×</button>{/if}
      <span class="spacer"></span>
      {#if results}<span class="muted small">{searchLabel ? searchLabel + " · " : ""}{results.took_ms} ms</span>{/if}
    </div>

    {#if results}
      <div class="tabs">
        <button class:on={tab === "all"} onclick={() => (tab = "all")}>All</button>
        <button class:on={tab === "moments"} onclick={() => (tab = "moments")}>Moments <i>{counts?.m}</i></button>
        <button class:on={tab === "said"} onclick={() => (tab = "said")}>Said <i>{counts?.w}</i></button>
        <button class:on={tab === "sounds"} onclick={() => (tab = "sounds")}>Sounds <i>{counts?.s || "beta"}</i></button>
        <button class:on={tab === "photos"} onclick={() => (tab = "photos")}>Photos <i>{counts?.p}</i></button>
      </div>
    {/if}

    {#if error}<div class="error" role="alert">{error}<button class="x" onclick={() => (error = "")}>×</button></div>{/if}

    <div class="results">
      {#if !results}
        <div class="empty">
          {#if !status?.folders.length}
            <h1>Find any moment by describing it.</h1>
            <p class="muted">Add your footage, photo and sound folders. Scrubless watches every shot and listens to every word, so you can search them like text — in any language, fully offline.</p>
            <button class="primary" onclick={addFolder}>Add a folder</button>
            <p class="muted small">or drag a folder onto this window</p>
          {:else if ready && ix && ix.items > 0}
            <h1>What are you looking for?</h1>
            <p class="muted">Describe a shot, quote something someone said, or drop a photo or sound to find similar ones.</p>
            <button class="ghost" onclick={tryExample}>Try “{EXAMPLES[exampleIdx]}” →</button>
          {:else}
            <h1>Getting your library ready…</h1>
            <p class="muted">You can search as soon as the first files are indexed.</p>
          {/if}
        </div>
      {:else}
        {#if (tab === "all" || tab === "moments") && visible(results.moments, 12).length}
          <section>
            {#if tab === "all"}<h2>Moments</h2>{/if}
            <div class="grid moments">
              {#each visible(results.moments, 12) as h (h.item_id)}{@render card(h)}{/each}
            </div>
            {#if tab === "all" && visible(results.moments, 999).length > 12}<button class="more" onclick={() => (tab = "moments")}>All {visible(results.moments, 999).length} moments →</button>{/if}
          </section>
        {/if}

        {#if (tab === "all" || tab === "said") && visible(results.said, 5).length}
          <section>
            {#if tab === "all"}<h2>Said</h2>{/if}
            <div class="said">
              {#each visible(results.said, 5) as h (h.item_id)}{@render saidRow(h)}{/each}
            </div>
            {#if tab === "all" && visible(results.said, 999).length > 5}<button class="more" onclick={() => (tab = "said")}>All {visible(results.said, 999).length} quotes →</button>{/if}
          </section>
        {/if}

        {#if (tab === "all" || tab === "sounds") && visible(results.sounds, 6).length}
          <section>
            {#if tab === "all"}<h2>Sounds <span class="beta">beta</span></h2>{/if}
            <div class="sounds">
              {#each visible(results.sounds, 6) as h (h.item_id)}
                <div class="row" class:weak={!h.strong} role="button" tabindex="0" data-nav
                  onclick={() => select(h, "sound")} onkeydown={(e) => e.key === "Enter" && select(h, "sound")}>
                  <span class="play">▶</span>
                  <span class="name" title={h.path}>{h.name}</span>
                  <span class="dir">{dirOf(h.path)}</span>
                  <span class="tc">{h.tc}</span>
                  <button class="add inline" class:on={inSelects.has(selectedKey(h))} onclick={(e) => addSelect(h, e)}>{inSelects.has(selectedKey(h)) ? "✓" : "+"}</button>
                </div>
              {/each}
            </div>
            {#if tab === "all" && visible(results.sounds, 999).length > 6}<button class="more" onclick={() => (tab = "sounds")}>All {visible(results.sounds, 999).length} sounds →</button>{/if}
          </section>
        {/if}

        {#if (tab === "all" || tab === "photos") && visible(results.photos, 12).length}
          <section>
            {#if tab === "all"}<h2>Photos</h2>{/if}
            <div class="grid photos">
              {#each visible(results.photos, 12) as h (h.item_id)}
                <div class="card photo" class:weak={!h.strong} role="button" tabindex="0" data-nav
                  onclick={() => select(h, "photo")} onkeydown={(e) => e.key === "Enter" && select(h, "photo")}>
                  <div class="thumb">
                    <img src={fileUrl(h.thumb)} alt="" loading="lazy" />
                    <button class="add" class:on={inSelects.has(selectedKey(h))} onclick={(e) => addSelect(h, e)}>{inSelects.has(selectedKey(h)) ? "✓" : "+"}</button>
                  </div>
                  <div class="meta"><span class="name" title={h.path}>{h.name}</span></div>
                </div>
              {/each}
            </div>
            {#if tab === "all" && visible(results.photos, 999).length > 12}<button class="more" onclick={() => (tab = "photos")}>All {visible(results.photos, 999).length} photos →</button>{/if}
          </section>
        {/if}

        {#if counts && !counts.m && !counts.w && !counts.s && !counts.p && !showWeak}
          <div class="empty">
            <h1>No clear matches</h1>
            <p class="muted">Nothing in your library stands out for this. Try describing it differently{weakTotal ? ", or look at the closest results" : ""}.</p>
          </div>
        {/if}
        {#if weakTotal && !showWeak}
          <button class="ghost weak-toggle" onclick={() => (showWeak = true)}>Show {weakTotal} weaker matches</button>
        {:else if showWeak}
          <button class="ghost weak-toggle" onclick={() => (showWeak = false)}>Hide weaker matches</button>
        {/if}
      {/if}
    </div>
  </main>

  {#if selected}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="scrim" onclick={() => (selected = null)}></div>
    <aside class="detail">
      <button class="x close" onclick={() => (selected = null)} title="Close (Esc)">×</button>
      <div class="player">
        {#if selectedKind === "photo"}
          <img src={fileUrl(selected.path)} alt="" onerror={(e) => ((e.currentTarget as HTMLImageElement).src = fileUrl(selected?.thumb ?? ""))} />
        {:else if previewBusy}
          <div class="loading"><span class="spinner"></span></div>
        {:else if previewSrc && selected.file_kind === "audio"}
          <div class="audio-only"><span>♪</span><audio src={previewSrc} controls autoplay></audio></div>
        {:else if previewSrc}
          <!-- svelte-ignore a11y_media_has_caption -->
          <video src={previewSrc} controls autoplay></video>
        {/if}
      </div>
      <h3 title={selected.path}>{selected.name}</h3>
      <p class="muted small path">{selected.path}</p>
      {#if selectedKind !== "photo"}
        <p class="tcbig">{selected.tc} <span class="muted small">{selected.info}</span></p>
      {:else}
        <p class="muted small">{selected.info}</p>
      {/if}
      {#if selectedKind !== "photo"}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div class="dragout" class:ready={!!clipPath} onmousedown={dragOut}>
          <span class="grip">⠿</span>
          <div>
            <strong>{clipPath ? "Drag this clip into your editor" : "Preparing clip…"}</strong>
            <span class="muted small">Premiere, Resolve, Final Cut, CapCut or any folder</span>
          </div>
        </div>
      {/if}
      <div class="actions">
        <button class="primary" onclick={() => selected && addSelect(selected)}>
          {selected && inSelects.has(selectedKey(selected)) ? "✓ In selects" : "Add to selects"} <kbd class="k">S</kbd>
        </button>
        {#if selectedKind !== "photo"}<button class="ghost" onclick={exportSelected}>Export clip</button>{/if}
        <button class="ghost" onclick={() => similar(selected!)}>Find similar</button>
        <button class="ghost" onclick={() => revealItemInDir(selected!.path)}>Show in folder</button>
        <button class="ghost" onclick={() => openPath(selected!.path)}>Open file</button>
        {#if selectedKind !== "photo"}<button class="ghost" onclick={copyTimecode}>Copy timecode</button>{/if}
      </div>
      {#if lines.length}
        <h4>Transcript</h4>
        <div class="transcript">
          {#each lines as l}
            <button class="line" class:cur={isCurrent(l)} onclick={() => jumpTo(l)}>
              <span class="tcs">{l.tc}</span>
              <span>{#each highlight(l.text, lastQuery) as p}{#if p.m}<mark>{p.t}</mark>{:else}{p.t}{/if}{/each}</span>
            </button>
          {/each}
        </div>
      {/if}
    </aside>
  {/if}

  {#if showSelects}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="scrim" onclick={() => (showSelects = false)}></div>
    <aside class="detail selects">
      <button class="x close" onclick={() => (showSelects = false)} title="Close (Esc)">×</button>
      <h3>Selects <span class="muted">· {selects.length}</span></h3>
      <p class="muted small">Collect moments here, then send them to your editor as a timeline that points at your original files — nothing is re-encoded.</p>
      {#if selects.length}
        <div class="sel-list">
          {#each selects as s, i (s.id)}
            <div class="sel">
              <span class="num">{i + 1}</span>
              <div class="sel-thumb">{#if s.thumb}<img src={fileUrl(s.thumb)} alt="" />{:else}<span>♪</span>{/if}</div>
              <div class="sel-body">
                <span class="name" title={s.path}>{s.name}</span>
                <span class="dir">{s.tc ? s.tc + " · " : ""}{Math.max(1, Math.round(s.t1 - s.t0))}s{s.note ? " · “" + s.note + "”" : ""}</span>
              </div>
              <button class="x" title="Remove" onclick={() => removeSelect(s.id)}>×</button>
            </div>
          {/each}
        </div>
        <label class="handles small">Handles <input type="number" min="0" max="10" step="0.5" bind:value={handles} /> s before and after each clip</label>
        <h4>Send to</h4>
        <div class="exports">
          {#each EXPORTS as f}
            <button class="exp" disabled={exporting} onclick={() => exportSelects(f)}>
              <strong>{f.label}</strong><span class="muted small">{f.hint}</span>
            </button>
          {/each}
        </div>
        <button class="ghost small clear" onclick={() => api.clearSelects().then(loadSelects)}>Clear all</button>
      {:else}
        <div class="empty small-empty"><p class="muted">Press <kbd>+</kbd> on any result (or <kbd>S</kbd> in the preview) to collect it here.</p></div>
      {/if}
    </aside>
  {/if}

  {#if showSettings && sv}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="scrim" onclick={() => (showSettings = false)}></div>
    <aside class="detail settings">
      <button class="x close" onclick={() => (showSettings = false)} title="Close (Esc)">×</button>
      <h3>Settings</h3>

      <h4>Speech</h4>
      <label class="toggle"><input type="checkbox" bind:checked={sv.settings.transcribe} onchange={saveSettings} /> Transcribe speech so you can search what people say</label>
      <label class="field">Language of your footage
        <select bind:value={sv.settings.speech_language} onchange={saveSettings}>
          {#each LANGS as [code, label]}<option value={code}>{label}</option>{/each}
        </select>
        <span class="muted small">Pick one if all your footage is in the same language: it's faster and avoids mix-ups (Hindi written as Urdu, for example).</span>
      </label>
      <label class="field">Quality
        <select bind:value={sv.settings.speech_quality} onchange={saveSettings}>
          <option value="auto">Automatic (accurate on a GPU, fast without one)</option>
          <option value="accurate">Accurate (large-v3-turbo, 574 MB)</option>
          <option value="fast">Fast (small, 190 MB)</option>
        </select>
      </label>
      <button class="ghost small" onclick={() => api.retranscribeAll().then((n) => flash(`Re-transcribing ${n} files`))}>Re-transcribe everything with these settings</button>

      <h4>Engine</h4>
      <label class="field">Processor
        <select bind:value={sv.settings.device} onchange={saveSettings}>
          <option value="auto">Use the graphics card when available</option>
          <option value="cpu">CPU only (frees the GPU for other apps)</option>
        </select>
      </label>

      <h4>Storage</h4>
      <label class="field">Preview and clip cache limit
        <span class="inline"><input type="number" min="0.5" max="100" step="0.5" bind:value={sv.settings.cache_gb} onchange={saveSettings} /> GB · using {bytes(sv.cache_bytes)}</span>
      </label>
      <div class="row-btns">
        <button class="ghost small" onclick={() => api.clearCache().then(openSettings)}>Clear cache</button>
        <button class="ghost small" onclick={() => sv && openPath(sv.data_dir)}>Open data folder</button>
      </div>
      <p class="muted small path">{sv.data_dir}</p>

      <h4>About</h4>
      <p class="muted small">Search runs entirely on this computer with EmbeddingGemma 2 (Apache 2.0) through llama.cpp, and speech with OpenAI Whisper through whisper.cpp. Your files never leave your machine.</p>
    </aside>
  {/if}

  {#if dropActive}
    <div class="drop"><div><strong>Drop it</strong><span>Drop a folder to add it, or a photo / sound / clip to find similar ones</span></div></div>
  {/if}
  {#if toast}<div class="toast">{toast}</div>{/if}
</div>
