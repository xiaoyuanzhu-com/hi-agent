// BYOK credential settings — typed wrappers over the Rust `server/settings.rs`
// route. The raw key is never sent to the browser; the view shows `configured`
// plus a `key_hint`.

/** The redacted view of a key-only vendor (STT/TTS/vision/image/video). */
export interface VendorView {
  configured: boolean;
  key_hint: string;
  env_fallback: boolean;
}

export interface CredentialsView {
  llm: {
    base_url: string;
    model: string | null;
    configured: boolean;
    key_hint: string;
    env_fallback: boolean;
  };
  stt: VendorView;
  tts: VendorView;
  vision: VendorView;
  image: VendorView;
  video: VendorView;
}

/** Tri-state `api_key`: omit to keep the stored key, "" clears it, a value sets it. */
export interface VendorUpdate {
  api_key?: string;
}

export interface CredentialsUpdate {
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
