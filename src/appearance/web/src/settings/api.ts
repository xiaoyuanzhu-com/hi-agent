// BYOK credential settings — typed wrappers over the Rust `server/settings.rs`
// route. The raw key is never sent to the browser; the view shows `configured`
// plus a `key_hint`.

export interface CredentialsView {
  llm: {
    base_url: string;
    model: string | null;
    configured: boolean;
    key_hint: string;
    env_fallback: boolean;
  };
}

export interface CredentialsUpdate {
  llm: {
    base_url: string;
    model: string | null;
    // Omit to keep the stored key; "" clears it; a value replaces it.
    api_key?: string;
  };
}

export interface SaveResult {
  ok: boolean;
  restart_required?: boolean;
  configured?: boolean;
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
