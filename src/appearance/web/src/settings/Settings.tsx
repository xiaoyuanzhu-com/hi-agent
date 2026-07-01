import { useEffect, useState } from "react";
import {
  fetchCredentials,
  saveCredentials,
  type Account,
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
 */
export function Settings() {
  const [view, setView] = useState<CredentialsView | null>(null);
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [vendorKeys, setVendorKeys] = useState<Record<string, string>>({});
  const [vendorBaseUrls, setVendorBaseUrls] = useState<Record<string, string>>({});
  const [vendorModels, setVendorModels] = useState<Record<string, string>>({});
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    const ctrl = new AbortController();
    fetchCredentials(ctrl.signal)
      .then((v) => {
        setView(v);
        setBaseUrl(v.llm.base_url);
        setModel(v.llm.model ?? "");
        setApiKey(""); // never prefill a key
        setVendorKeys({});
        // Prefill the non-secret vendor overrides so edits start from the stored value.
        setVendorBaseUrls(Object.fromEntries(VENDORS.map((x) => [x.id, v[x.id].base_url])));
        setVendorModels(Object.fromEntries(VENDORS.map((x) => [x.id, v[x.id].model ?? ""])));
      })
      .catch((e) => {
        if (!ctrl.signal.aborted) setError(String(e));
      });
    return () => ctrl.abort();
  }, [reloadKey]);

  const mode: Mode = view?.mode ?? "xiaoyuanzhu";

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

        {mode === "byok" ? (
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
        ) : (
          <AccountCard account={view?.account ?? null} />
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
      </div>
    </div>
  );
}

/** Xiaoyuanzhu: show the broker account — tier + remaining energy, or a connecting state. */
function AccountCard({ account }: { account: Account | null }) {
  const pct =
    account && account.energy_total > 0
      ? Math.max(0, Math.min(100, Math.round((account.energy_remaining / account.energy_total) * 100)))
      : 0;
  const resets = account?.resets_at ? fmtDate(account.resets_at) : "";
  return (
    <section className="settings-card">
      <div className="settings-card-head">
        <h2>Xiaoyuanzhu</h2>
        {account ? (
          <span className="tag ok">{account.tier}</span>
        ) : (
          <span className="tag off">not connected</span>
        )}
      </div>
      {account ? (
        <div className="account-credits">
          <div className="account-bar">
            <i style={{ width: `${pct}%` }} />
          </div>
          <p className="settings-sub">
            {account.energy_remaining.toLocaleString()} / {account.energy_total.toLocaleString()} energy
            {resets && <> · resets {resets}</>}
          </p>
        </div>
      ) : (
        <p className="settings-sub">
          Connecting to the gateway for daily free energy… Sign in at{" "}
          <code>account.xiaoyuanzhu.com</code> to draw subscription energy instead.
        </p>
      )}
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
