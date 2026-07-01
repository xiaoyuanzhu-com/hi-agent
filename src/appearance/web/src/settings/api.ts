// BYOK credential settings — typed wrappers over the Rust `server/settings.rs`
// route. The raw key is never sent to the browser; the view shows `configured`
// plus a `key_hint`.

/** The redacted view of a vendor (STT/TTS/vision/image/video). The key is never
 * returned (only `configured` + `key_hint`); `base_url`/`model` are non-secret. */
export interface VendorView {
  configured: boolean;
  key_hint: string;
  base_url: string;
  model: string | null;
}

/** How the agent obtains its credentials. */
export type Mode = "byok" | "xiaoyuanzhu";

/** The broker account snapshot (xiaoyuanzhu), absent until energy is fetched. */
export interface Account {
  tier: string;
  energy_remaining: number;
  energy_total: number;
  resets_at: string;
}

export interface CredentialsView {
  mode: Mode;
  account: Account | null;
  llm: {
    base_url: string;
    model: string | null;
    configured: boolean;
    key_hint: string;
  };
  stt: VendorView;
  tts: VendorView;
  vision: VendorView;
  image: VendorView;
  video: VendorView;
}

/** `api_key` is tri-state: omit to keep the stored key, "" clears it, a value sets
 * it. `base_url` / `model` are non-secret: omit to keep, a value ("" clears to the
 * default) sets. */
export interface VendorUpdate {
  api_key?: string;
  base_url?: string;
  model?: string;
}

export interface CredentialsUpdate {
  mode?: Mode;
  llm?: {
    base_url: string;
    model: string | null;
    api_key?: string;
  };
  stt?: VendorUpdate;
  tts?: VendorUpdate;
  vision?: VendorUpdate;
  image?: VendorUpdate;
  video?: VendorUpdate;
}

export interface SaveResult {
  ok: boolean;
  restart_required?: boolean;
  error?: string;
}

/** Read the current credential state (key redacted). Throws on HTTP error. */
export async function fetchCredentials(signal?: AbortSignal): Promise<CredentialsView> {
  const res = await fetch("/api/settings/credentials", { signal });
  if (!res.ok) throw new Error(`GET /api/settings/credentials → ${res.status}`);
  return (await res.json()) as CredentialsView;
}

/** Persist credentials. Throws on HTTP error; check `ok` for save failures. */
export async function saveCredentials(update: CredentialsUpdate): Promise<SaveResult> {
  const res = await fetch("/api/settings/credentials", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(update),
  });
  if (!res.ok) throw new Error(`POST /api/settings/credentials → ${res.status}`);
  return (await res.json()) as SaveResult;
}
