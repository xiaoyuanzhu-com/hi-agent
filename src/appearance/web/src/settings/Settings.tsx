import { useEffect, useState } from "react";
import {
  fetchAccount,
  fetchCredentials,
  saveCredentials,
  UnauthorizedError,
  type AccountStatus,
  type CredentialsUpdate,
  type CredentialsView,
  type Mode,
  type VendorView,
} from "./api";
import "./settings.css";

/** The key-only vendors, in display order. `id` matches the API/store section. */
const VENDORS = [
  { id: "stt", label: "Speech-to-text", vendor: "Volcengine" },
  { id: "tts", label: "Text-to-speech", vendor: "Volcengine" },
  { id: "vision", label: "Vision", vendor: "Doubao" },
  { id: "image", label: "Image generation", vendor: "Doubao" },
  { id: "video", label: "Video generation", vendor: "Doubao" },
] as const;

const MODES: { id: Mode; label: string }[] = [
  { id: "xiaoyuanzhu", label: "Xiaoyuanzhu" },
  { id: "byok", label: "BYOK" },
];

/**
 * Settings — a top-level product page at `/settings`. Two ways to get model
 * credits: Xiaoyuanzhu (a broker account — anonymous daily free energy, or a
 * subscription once signed in via account.xiaoyuanzhu.com) or BYOK (the user's own
 * vendor keys). Xiaoyuanzhu draws a credential bundle from the broker; BYOK uses the
 * keys entered here. A raw key is never returned from the server (only a hint);
 * changes apply on restart.
 *
 * The account status (public `GET /api/account`) always renders — the free tier is
 * anonymous, so it's visible without a login. The credential *editor* sits behind
 * the owner gate: when auth is on and the visitor isn't signed in, that read 401s
 * and we show a sign-in prompt in place of the editor rather than an error.
 */
export function Settings() {
  const [account, setAccount] = useState<AccountStatus | null>(null);
  const [view, setView] = useState<CredentialsView | null>(null);
  // Auth is on and this visitor isn't signed in — show the sign-in prompt for the
  // (gated) editor while still rendering the public account status above it.
  const [needsSignin, setNeedsSignin] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [vendorKeys, setVendorKeys] = useState<Record<string, string>>({});
  const [vendorBaseUrls, setVendorBaseUrls] = useState<Record<string, string>>({});
  const [vendorModels, setVendorModels] = useState<Record<string, string>>({});
  const [effort, setEffort] = useState("");
  const [permissionMode, setPermissionMode] = useState("");
  const [pulse, setPulse] = useState("");
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    const ctrl = new AbortController();
    // The account status is public — always fetch it so the card renders even for
    // a signed-out visitor behind the gate.
    fetchAccount(ctrl.signal)
      .then(setAccount)
      .catch(() => {
        // Public endpoint; a failure here just leaves the card in its neutral
        // "connecting" state rather than surfacing a banner.
        if (!ctrl.signal.aborted) setAccount(null);
      });
    // The credential editor is gated: a 401 means "sign in to manage", not an error.
    fetchCredentials(ctrl.signal)
      .then((v) => {
        setNeedsSignin(false);
        setView(v);
        setBaseUrl(v.llm.base_url);
        setModel(v.llm.model ?? "");
        setApiKey(""); // never prefill a key
        setVendorKeys({});
        // Prefill the non-secret vendor overrides so edits start from the stored value.
        setVendorBaseUrls(Object.fromEntries(VENDORS.map((x) => [x.id, v[x.id].base_url])));
        setVendorModels(Object.fromEntries(VENDORS.map((x) => [x.id, v[x.id].model ?? ""])));
        setEffort(v.agent.effort ?? "");
        setPermissionMode(v.agent.permission_mode ?? "");
        setPulse(v.agent.pulse ?? "");
      })
      .catch((e) => {
        if (ctrl.signal.aborted) return;
        if (e instanceof UnauthorizedError) {
          setNeedsSignin(true);
          setView(null);
        } else {
          setError(String(e));
        }
      });
    return () => ctrl.abort();
  }, [reloadKey]);

  const mode: Mode = view?.mode ?? account?.mode ?? "xiaoyuanzhu";
  // The account card belongs to xiaoyuanzhu mode (broker credits); BYOK has no
  // broker account. Before we know the mode, default to showing it (the default).
  const showAccount = mode === "xiaoyuanzhu";

  const onSelectMode = async (m: Mode) => {
    if (m === mode) return;
    setSaving(true);
    setStatus(null);
    setError(null);
    try {
      const res = await saveCredentials({ mode: m });
      if (res.ok) {
        setStatus(m === "byok" ? "Using your own keys." : "Switched — restart hi-agent to apply.");
        setReloadKey((k) => k + 1);
      } else {
        setError(res.error ?? "failed to switch mode");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const onSave = async () => {
    setSaving(true);
    setStatus(null);
    setError(null);
    try {
      const update: CredentialsUpdate = {
        llm: {
          base_url: baseUrl.trim(),
          model: model.trim() ? model.trim() : null,
          // Omit the key when blank → the server keeps the stored one.
          ...(apiKey.trim() ? { api_key: apiKey.trim() } : {}),
        },
      };
      for (const v of VENDORS) {
        const k = (vendorKeys[v.id] ?? "").trim();
        // Send the non-secret overrides always (prefilled, so this is idempotent
        // unless edited); include the key only when one was typed.
        update[v.id] = {
          base_url: (vendorBaseUrls[v.id] ?? "").trim(),
          model: (vendorModels[v.id] ?? "").trim(),
          ...(k ? { api_key: k } : {}),
        };
      }
      const res = await saveCredentials(update);
      if (res.ok) {
        setStatus("Saved. Restart hi-agent for the new credentials to take effect.");
        setApiKey("");
        setVendorKeys({});
        setReloadKey((k) => k + 1);
      } else {
        setError(res.error ?? "save failed");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  // The agent tunables save independently of the credential fields (they apply in
  // both modes). Each is sent as-is: empty clears back to the built-in default.
  const onSaveAgent = async () => {
    setSaving(true);
    setStatus(null);
    setError(null);
    try {
      const res = await saveCredentials({
        agent: { effort: effort.trim(), permission_mode: permissionMode.trim(), pulse: pulse.trim() },
      });
      if (res.ok) {
        setStatus("Saved. Restart hi-agent for the new agent settings to take effect.");
        setReloadKey((k) => k + 1);
      } else {
        setError(res.error ?? "save failed");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const llmConfigured = view?.llm.configured ?? false;

  return (
    <div className="settings-page">
      <div className="settings-shell">
        <header className="settings-head">
          <a className="settings-back" href="/" title="back to the agent">
            ←
          </a>
          <h1>Settings</h1>
        </header>

        <p className="settings-intro">How the agent draws its model credits.</p>

        {showAccount && <AccountStatusCard account={account} />}

        {needsSignin ? (
          <SignInPanel />
        ) : (
          <>
            <div className="mode-tabs" role="tablist">
              {MODES.map((m) => (
                <button
                  key={m.id}
                  className={m.id === mode ? "mode-tab sel" : "mode-tab"}
                  disabled={saving}
                  onClick={() => onSelectMode(m.id)}
                >
                  {m.label}
                </button>
              ))}
            </div>

            {mode === "byok" && (
              <>
                <section className="settings-card">
                  <div className="settings-card-head">
                    <h2>LLM · Claude</h2>
                    {llmConfigured ? (
                      <span className="tag ok">configured · {view?.llm.key_hint}</span>
                    ) : (
                      <span className="tag warn">not configured</span>
                    )}
                  </div>

                  <label className="field">
                    <span>API key</span>
                    <input
                      type="password"
                      value={apiKey}
                      placeholder={llmConfigured ? "•••• (unchanged)" : "sk-ant-…"}
                      onChange={(e) => setApiKey(e.target.value)}
                      autoComplete="off"
                    />
                  </label>

                  <label className="field">
                    <span>Base URL</span>
                    <input
                      type="text"
                      value={baseUrl}
                      placeholder="https://api.anthropic.com"
                      onChange={(e) => setBaseUrl(e.target.value)}
                    />
                  </label>

                  <label className="field">
                    <span>
                      Model <em>optional</em>
                    </span>
                    <input
                      type="text"
                      value={model}
                      placeholder="adapter default"
                      onChange={(e) => setModel(e.target.value)}
                    />
                  </label>
                </section>

                {VENDORS.map((v) => (
                  <VendorCard
                    key={v.id}
                    label={v.label}
                    vendor={v.vendor}
                    view={view?.[v.id]}
                    value={vendorKeys[v.id] ?? ""}
                    onChange={(val) => setVendorKeys((m) => ({ ...m, [v.id]: val }))}
                    baseUrl={vendorBaseUrls[v.id] ?? ""}
                    onBaseUrlChange={(val) => setVendorBaseUrls((m) => ({ ...m, [v.id]: val }))}
                    model={vendorModels[v.id] ?? ""}
                    onModelChange={(val) => setVendorModels((m) => ({ ...m, [v.id]: val }))}
                  />
                ))}
              </>
            )}

            <div className="settings-actions">
              {mode === "byok" && (
                <button className="primary" onClick={onSave} disabled={saving}>
                  {saving ? "Saving…" : "Save"}
                </button>
              )}
              {status && <span className="note ok">{status}</span>}
              {error && <span className="note err">{error}</span>}
            </div>

            <p className="settings-intro">How the agent behaves. Applies in either mode.</p>

            <section className="settings-card">
              <div className="settings-card-head">
                <h2>Agent</h2>
              </div>
              <label className="field">
                <span>
                  Effort <em>optional</em>
                </span>
                <input
                  type="text"
                  value={effort}
                  placeholder="adapter default (e.g. low · medium · high)"
                  onChange={(e) => setEffort(e.target.value)}
                />
              </label>
              <label className="field">
                <span>
                  Permission mode <em>optional</em>
                </span>
                <input
                  type="text"
                  value={permissionMode}
                  placeholder="adapter default (e.g. acceptEdits)"
                  onChange={(e) => setPermissionMode(e.target.value)}
                />
              </label>
              <label className="field">
                <span>
                  Pulse interval <em>optional</em>
                </span>
                <input
                  type="text"
                  value={pulse}
                  placeholder="default 30m (e.g. 90s · 30m · 1h · off)"
                  onChange={(e) => setPulse(e.target.value)}
                />
              </label>
            </section>

            <div className="settings-actions">
              <button className="primary" onClick={onSaveAgent} disabled={saving}>
                {saving ? "Saving…" : "Save agent settings"}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** Xiaoyuanzhu: the broker account, always visible (the free tier is anonymous, so
 * no login is needed to see it). Driven by the public `/api/account`, in three
 * states: `connected` (tier + energy bar), `connecting` (first bootstrap in
 * flight), or `error` (last sync failed with no cached balance — surfaces why). */
function AccountStatusCard({ account }: { account: AccountStatus | null }) {
  const state = account?.state ?? "connecting";
  const total = account?.energy_total ?? 0;
  const remaining = account?.energy_remaining ?? 0;
  const pct = total > 0 ? Math.max(0, Math.min(100, Math.round((remaining / total) * 100))) : 0;
  const resets = account?.resets_at ? fmtDate(account.resets_at) : "";
  return (
    <section className="settings-card">
      <div className="settings-card-head">
        <h2>Xiaoyuanzhu</h2>
        {state === "connected" ? (
          <span className="tag ok">{account?.tier ?? "free"}</span>
        ) : state === "error" ? (
          <span className="tag warn">unavailable</span>
        ) : (
          <span className="tag off">connecting…</span>
        )}
      </div>
      {state === "connected" ? (
        <div className="account-credits">
          <div className="account-bar">
            <i style={{ width: `${pct}%` }} />
          </div>
          <p className="settings-sub">
            {remaining.toLocaleString()} / {total.toLocaleString()} energy
            {resets && <> · resets {resets}</>}
          </p>
        </div>
      ) : state === "error" ? (
        <p className="settings-sub">
          Couldn't reach the gateway to provision your free account — it keeps
          retrying in the background.
          {account?.error && (
            <>
              {" "}
              Last error: <code>{account.error}</code>
            </>
          )}
        </p>
      ) : (
        <p className="settings-sub">Provisioning your daily free energy…</p>
      )}
    </section>
  );
}

/** Shown at `/settings` when the owner login gate is on and the visitor isn't
 * signed in. The account status above is public; switching modes, entering your
 * own keys, and tuning agent behaviour are owner-only, so they live behind this
 * sign-in. Navigates to the backend login (same-origin in prod; dev Vite proxies
 * `/auth`), returning to wherever we are afterwards. */
function SignInPanel() {
  const next = encodeURIComponent(window.location.pathname + window.location.search);
  return (
    <section className="settings-card">
      <div className="settings-card-head">
        <h2>Manage settings</h2>
        <span className="tag off">sign-in required</span>
      </div>
      <p className="settings-sub">
        Your free account above needs no login. Sign in to switch modes, use your own
        API keys, or tune how the agent behaves.
      </p>
      <div className="settings-actions">
        <button
          className="primary"
          onClick={() => {
            window.location.href = `/auth/login?next=${next}`;
          }}
        >
          Sign in
        </button>
      </div>
    </section>
  );
}

function fmtDate(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleDateString();
}

/** A vendor card (speech, vision, media): API key plus optional base-URL / model
 * overrides. Unset key is "off", not an error. */
function VendorCard({
  label,
  vendor,
  view,
  value,
  onChange,
  baseUrl,
  onBaseUrlChange,
  model,
  onModelChange,
}: {
  label: string;
  vendor: string;
  view?: VendorView;
  value: string;
  onChange: (v: string) => void;
  baseUrl: string;
  onBaseUrlChange: (v: string) => void;
  model: string;
  onModelChange: (v: string) => void;
}) {
  const configured = view?.configured ?? false;
  const placeholder = configured ? "•••• (unchanged)" : "paste key to enable";
  return (
    <section className="settings-card">
      <div className="settings-card-head">
        <h2>
          {label} · {vendor}
        </h2>
        {configured ? (
          <span className="tag ok">configured · {view?.key_hint}</span>
        ) : (
          <span className="tag off">off</span>
        )}
      </div>
      <label className="field">
        <span>API key</span>
        <input
          type="password"
          value={value}
          placeholder={placeholder}
          onChange={(e) => onChange(e.target.value)}
          autoComplete="off"
        />
      </label>
      <label className="field">
        <span>
          Base URL <em>optional</em>
        </span>
        <input
          type="text"
          value={baseUrl}
          placeholder="vendor default"
          onChange={(e) => onBaseUrlChange(e.target.value)}
        />
      </label>
      <label className="field">
        <span>
          Model <em>optional</em>
        </span>
        <input
          type="text"
          value={model}
          placeholder="vendor default"
          onChange={(e) => onModelChange(e.target.value)}
        />
      </label>
    </section>
  );
}
