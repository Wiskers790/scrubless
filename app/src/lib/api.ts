import { invoke as tauriInvoke, convertFileSrc } from "@tauri-apps/api/core";

// Outside the Tauri window (dev only), talk to the backend's localhost bridge instead.
const BRIDGE = "http://127.0.0.1:1430";
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  if (inTauri) return tauriInvoke<T>(cmd, args);
  const r = await fetch(`${BRIDGE}/invoke/${cmd}`, { method: "POST", body: JSON.stringify(args) });
  const body = await r.json();
  if (!r.ok) throw body;
  return body as T;
}

export type RuntimeState =
  | { state: "needs_model" }
  | { state: "downloading"; file: string; done: number; total: number }
  | { state: "starting"; device: string }
  | { state: "ready"; device: string }
  | { state: "failed"; error: string };

export type SpeechState =
  | { state: "idle" }
  | { state: "downloading"; file: string; done: number; total: number }
  | { state: "starting" }
  | { state: "ready"; model: string; device: string }
  | { state: "unavailable"; error: string };

export interface Progress {
  scanning: string | null;
  current: string | null;
  current_done: number;
  current_total: number;
  files_total: number;
  files_done: number;
  files_pending: number;
  files_failed: number;
  items: number;
  paused: boolean;
  transcribing: string | null;
  speech_done: number;
  speech_pending: number;
}

export interface Status {
  runtime: RuntimeState;
  speech: SpeechState;
  index: Progress;
  folders: string[];
  transcribe: boolean;
}

export interface Hit {
  item_id: number;
  file_id: number;
  path: string;
  name: string;
  file_kind: "video" | "audio" | "image";
  t0: number;
  t1: number;
  score: number;
  z: number;
  strong: boolean;
  exact: boolean;
  thumb: string | null;
  text: string | null;
  tc: string;
  info: string;
}

export interface Results {
  moments: Hit[];
  said: Hit[];
  sounds: Hit[];
  photos: Hit[];
  took_ms: number;
}

export interface Filters {
  kinds: string[];
  folders: string[];
  since: number | null;
  until: number | null;
}

export interface Line {
  t0: number;
  t1: number;
  tc: string;
  text: string;
}

export interface Select {
  id: number;
  file_id: number;
  path: string;
  name: string;
  file_kind: string;
  t0: number;
  t1: number;
  tc: string;
  thumb: string | null;
  note: string | null;
}

export interface Settings {
  transcribe: boolean;
  speech_language: string;
  speech_quality: "auto" | "accurate" | "fast";
  device: "auto" | "cpu";
  cache_gb: number;
}

export interface SettingsView {
  settings: Settings;
  data_dir: string;
  cache_bytes: number;
}

export type ExportFormat = "fcpxml" | "xml" | "edl" | "csv" | "clips";

export const api = {
  status: () => invoke<Status>("status"),
  addFolder: (path: string) => invoke<void>("add_folder", { path }),
  removeFolder: (path: string) => invoke<void>("remove_folder", { path }),
  rescan: () => invoke<void>("rescan"),
  setPaused: (paused: boolean) => invoke<void>("set_paused", { paused }),
  setTranscribe: (on: boolean) => invoke<void>("set_transcribe", { on }),
  getSettings: () => invoke<SettingsView>("get_settings"),
  setSettings: (settings: Settings) => invoke<void>("set_settings", { settings }),
  retranscribeAll: () => invoke<number>("retranscribe_all"),
  clearCache: () => invoke<void>("clear_cache"),
  isDir: (path: string) => invoke<boolean>("is_dir", { path }),
  search: (query: string, filters: Filters) => invoke<Results>("search", { query, filters }),
  searchByFile: (path: string, filters: Filters) => invoke<Results>("search_by_file", { path, filters }),
  searchSimilar: (itemId: number, filters: Filters) => invoke<Results>("search_similar", { itemId, filters }),
  transcript: (fileId: number) => invoke<Line[]>("transcript", { fileId }),
  preview: (path: string, t0: number, t1: number) => invoke<string>("preview", { path, t0, t1 }),
  exportClip: (path: string, t0: number, t1: number, pad: number, dest: string | null) =>
    invoke<string>("export_clip", { path, t0, t1, pad, dest }),
  addSelect: (fileId: number, t0: number, t1: number, note: string | null) =>
    invoke<number>("add_select", { fileId, t0, t1, note }),
  removeSelect: (id: number) => invoke<void>("remove_select", { id }),
  clearSelects: () => invoke<void>("clear_selects"),
  selects: () => invoke<Select[]>("selects"),
  exportSelects: (format: ExportFormat, dest: string, pad: number) =>
    invoke<string>("export_selects", { format, dest, pad }),
};

export const fileUrl = (p: string | null) =>
  !p ? "" : inTauri ? convertFileSrc(p) : `${BRIDGE}/file?path=${encodeURIComponent(p)}`;

export function timecode(t: number): string {
  const s = Math.max(0, Math.floor(t));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(sec).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

export function bytes(n: number): string {
  if (n < 1e6) return `${(n / 1e3).toFixed(0)} KB`;
  if (n < 1e9) return `${(n / 1e6).toFixed(0)} MB`;
  return `${(n / 1e9).toFixed(2)} GB`;
}

const STOP = new Set(["the", "and", "for", "with", "from", "that", "this", "are", "was", "were", "has", "had", "his", "her", "they", "their", "she", "you", "its", "into", "who", "how", "what"]);
const NO_SPACES = /[\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Thai}]/u;

/** Split `text` into plain and marked parts: case-insensitive occurrences of the query (whole
 *  phrase, or words of 3+ characters that aren't filler) are marked. Matches must be whole words,
 *  so "on" never lights up inside "abatieron"; scripts written without spaces match anywhere. */
export function highlight(text: string, q: string): { t: string; m: boolean }[] {
  const words = q.trim().split(/\s+/).filter((w) => w.length >= 3 && !STOP.has(w.toLowerCase()));
  const terms = [...new Set([q.trim(), ...words])].filter((w) => w.length >= 2).sort((a, b) => b.length - a.length);
  if (!terms.length) return [{ t: text, m: false }];
  const alts = terms.map((w) => {
    const e = w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    return NO_SPACES.test(w) ? e : `(?<![\\p{L}\\p{N}])${e}(?![\\p{L}\\p{N}])`;
  });
  // with one capturing group, split() puts the matches at the odd indexes
  return text
    .split(new RegExp(`(${alts.join("|")})`, "giu"))
    .map((t, i) => ({ t, m: i % 2 === 1 }))
    .filter((p) => p.t);
}
