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

/** Coarse broker-sync state driving the account card: still bootstrapping, live,
 * or the last sync errored (with no cached energy to fall back on). */
export type AccountState = "connecting" | "connected" | "error";

/** Public, read-only account status (`GET /api/account`) — the anonymous free tier
 * + energy + sync state. Readable without a login, so the Settings page can show
 * the account even when the credential editor is behind the owner gate. */
export interface AccountStatus {
  mode: Mode;
  state: AccountState;
  tier: string | null;
  energy_remaining: number | null;
  energy_total: number | null;
  resets_at: string | null;
  /** Why the account is unavailable, when `state === "error"`. */
  error: string | null;
  /** RFC3339 of the last broker sync attempt (for a "checked …" hint). */
  checked_at: string | null;
  /** Whether the owner login gate is engaged — gates the credential editor and,
   * later, the sub-tier upgrade. When false there's no sign-in to offer. */
  auth_enabled: boolean;
}

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
  /** Agent behaviour tunables (apply in every mode; not credentials). */
  agent: {
    effort: string | null;
    permission_mode: string | null;
    pulse: string | null;
  };
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
  /** Each field absent-keeps; a value ("" clears to the default) sets it. */
  agent?: {
    effort?: string;
    permission_mode?: string;
    pulse?: string;
  };
}

export interface SaveResult {
  ok: boolean;
  restart_required?: boolean;
  error?: string;
}

/** Thrown when the credential API returns 401 — auth is on and the visitor isn't
 * signed in. The Settings page catches this to show a sign-in prompt (and still
 * render the public account status) instead of a generic error. */
export class UnauthorizedError extends Error {
  constructor() {
    super("unauthorized");
    this.name = "UnauthorizedError";
  }
}

/** Read the public account status (tier + energy + sync state). No auth required;
 * throws only on a genuine HTTP/network error. */
export async function fetchAccount(signal?: AbortSignal): Promise<AccountStatus> {
  const res = await fetch("/api/account", { signal });
  if (!res.ok) throw new Error(`GET /api/account → ${res.status}`);
  return (await res.json()) as AccountStatus;
}

/** Read the current credential state (key redacted). Throws {@link UnauthorizedError}
 * on 401 (gated, not signed in), or a generic Error on any other HTTP failure. */
export async function fetchCredentials(signal?: AbortSignal): Promise<CredentialsView> {
  const res = await fetch("/api/settings/credentials", { signal });
  if (res.status === 401) throw new UnauthorizedError();
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
