import { useEffect, useState } from "react";
import { fetchCredentials, saveCredentials, type CredentialsView } from "./api";
import "./settings.css";

/**
 * Settings — a top-level product page at `/settings` (distinct from the operator
 * console at `/inspect`). v1 holds BYOK: the upstream LLM credential the agent runs
 * on. The raw key is never sent back from the server (only a hint), so the key
 * field starts empty and a blank save keeps the stored key. Changes take effect on
 * the next restart.
 */
export function Settings() {
  const [view, setView] = useState<CredentialsView | null>(null);
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
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
        setApiKey(""); // never prefill the key
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
      const llm: { base_url: string; model: string | null; api_key?: string } = {
        base_url: baseUrl.trim(),
        model: model.trim() ? model.trim() : null,
      };
      // Omit the key when blank → the server keeps the stored one.
      if (apiKey.trim()) llm.api_key = apiKey.trim();
      const res = await saveCredentials({ llm });
      if (res.ok) {
        setStatus("Saved. Restart hi-agent for the new credentials to take effect.");
        setApiKey("");
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

  const configured = view?.llm.configured ?? false;
  const envFallback = view?.llm.env_fallback ?? false;

  return (
    <div className="settings-page">
      <div className="settings-shell">
        <header className="settings-head">
          <a className="settings-back" href="/" title="back to the agent">
            ←
          </a>
          <h1>Settings</h1>
        </header>

        <section className="settings-card">
          <div className="settings-card-head">
            <h2>LLM · Claude</h2>
            {configured ? (
              <span className="tag ok">configured · {view?.llm.key_hint}</span>
            ) : envFallback ? (
              <span className="tag mute">using AI_API_KEY from .env</span>
            ) : (
              <span className="tag warn">not configured</span>
            )}
          </div>
          <p className="settings-sub">
            Bring your own key. Stored locally in <code>credentials.json</code>; the
            key is never sent back to this page.
          </p>

          <label className="field">
            <span>API key</span>
            <input
              type="password"
              value={apiKey}
              placeholder={configured ? "•••• (unchanged)" : "sk-ant-…"}
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

          <div className="settings-actions">
            <button className="primary" onClick={onSave} disabled={saving}>
              {saving ? "Saving…" : "Save"}
            </button>
            {status && <span className="note ok">{status}</span>}
            {error && <span className="note err">{error}</span>}
          </div>
        </section>
      </div>
    </div>
  );
}
