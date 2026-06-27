import { useEffect, useState } from "react";
import {
  fetchCredentials,
  saveCredentials,
  type CredentialsUpdate,
  type CredentialsView,
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

/**
 * Settings — a top-level product page at `/settings` (distinct from the operator
 * console at `/inspect`). Holds BYOK: the upstream LLM credential the agent runs
 * on, plus the keyed capability vendors (speech, vision, media). A raw key is
 * never sent back from the server (only a hint), so key fields start empty and a
 * blank save keeps the stored key. Changes take effect on the next restart.
 */
export function Settings() {
  const [view, setView] = useState<CredentialsView | null>(null);
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [vendorKeys, setVendorKeys] = useState<Record<string, string>>({});
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
      })
      .catch((e) => {
        if (!ctrl.signal.aborted) setError(String(e));
      });
    return () => ctrl.abort();
  }, [reloadKey]);

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
      // Only send a vendor section when its field was typed into.
      for (const v of VENDORS) {
        const k = (vendorKeys[v.id] ?? "").trim();
        if (k) update[v.id] = { api_key: k };
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
  const llmEnvFallback = view?.llm.env_fallback ?? false;

  return (
    <div className="settings-page">
      <div className="settings-shell">
        <header className="settings-head">
          <a className="settings-back" href="/" title="back to the agent">
            ←
          </a>
          <h1>Settings</h1>
        </header>

        <p className="settings-intro">
          Bring your own keys. Stored locally in <code>credentials.json</code>; keys
          are never sent back to this page. A key here also turns that capability on.
        </p>

        <section className="settings-card">
          <div className="settings-card-head">
            <h2>LLM · Claude</h2>
            {llmConfigured ? (
              <span className="tag ok">configured · {view?.llm.key_hint}</span>
            ) : llmEnvFallback ? (
              <span className="tag mute">using AI_API_KEY from .env</span>
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
          />
        ))}

        <div className="settings-actions">
          <button className="primary" onClick={onSave} disabled={saving}>
            {saving ? "Saving…" : "Save"}
          </button>
          {status && <span className="note ok">{status}</span>}
          {error && <span className="note err">{error}</span>}
        </div>
      </div>
    </div>
  );
}

/** A key-only vendor card (speech, vision, media). Unset is "off", not an error. */
function VendorCard({
  label,
  vendor,
  view,
  value,
  onChange,
}: {
  label: string;
  vendor: string;
  view?: VendorView;
  value: string;
  onChange: (v: string) => void;
}) {
  const configured = view?.configured ?? false;
  const envFallback = view?.env_fallback ?? false;
  const placeholder = configured
    ? "•••• (unchanged)"
    : envFallback
      ? "•••• (from .env)"
      : "paste key to enable";
  return (
    <section className="settings-card">
      <div className="settings-card-head">
        <h2>
          {label} · {vendor}
        </h2>
        {configured ? (
          <span className="tag ok">configured · {view?.key_hint}</span>
        ) : envFallback ? (
          <span className="tag mute">using .env key</span>
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
    </section>
  );
}
