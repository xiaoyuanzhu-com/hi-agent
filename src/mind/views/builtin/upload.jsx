// Built-in: the file-handoff entry. Shown when the user wants to hand the agent a
// file (a contract, a passport scan). Two doors: drag-drop / pick (this device),
// and a QR a phone scans to upload. A handed file is an artifact, not something
// the agent looks at — both doors post to the `file` channel, which wakes the
// agent. Seeded at `_builtin/upload`; the agent may adapt it like any view.
import { useState, useEffect, useRef } from "react";
import { useScene } from "@hi/core";

// Our content fills the frame; let the host dock the live words up top.
export const captionAside = "top";

export default function Upload() {
  const scene = useScene();
  const [url, setUrl] = useState(null);
  const [items, setItems] = useState([]); // { key, name, state: sending|done|error }
  const [drag, setDrag] = useState(false);
  const inputRef = useRef(null);

  // Mint a scene-scoped upload link for the phone QR.
  useEffect(() => {
    let alive = true;
    fetch("/api/handoff", { method: "POST", headers: { "X-HI-Scene": scene } })
      .then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
      .then((d) => alive && setUrl(d.url))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [scene]);

  async function send(files) {
    for (const file of files) {
      const key = file.name + ":" + file.size + ":" + file.lastModified;
      setItems((xs) => [...xs.filter((x) => x.key !== key), { key, name: file.name, state: "sending" }]);
      try {
        const fd = new FormData();
        fd.append("file", file, file.name);
        const r = await fetch("/api/in/file", { method: "POST", headers: { "X-HI-Scene": scene }, body: fd });
        setItems((xs) => xs.map((x) => (x.key === key ? { ...x, state: r.ok ? "done" : "error" } : x)));
      } catch {
        setItems((xs) => xs.map((x) => (x.key === key ? { ...x, state: "error" } : x)));
      }
    }
  }

  function onDrop(e) {
    e.preventDefault();
    setDrag(false);
    const files = e.dataTransfer?.files;
    if (files?.length) send([...files]);
  }

  return (
    <div style={S.root}>
      <div style={S.title}>传文件给我</div>
      <div style={S.row}>
        <div
          onDragOver={(e) => {
            e.preventDefault();
            setDrag(true);
          }}
          onDragLeave={() => setDrag(false)}
          onDrop={onDrop}
          onClick={() => inputRef.current?.click()}
          style={{ ...S.drop, ...(drag ? S.dropActive : null) }}
        >
          <div style={{ fontSize: 40, marginBottom: 8 }}>⬆</div>
          <div style={{ fontWeight: 600, fontSize: 16 }}>把文件拖到这里</div>
          <div style={S.hint}>或点击选择 · 合同、证件照、PDF 都行</div>
          <input
            ref={inputRef}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              if (e.target.files?.length) send([...e.target.files]);
              e.target.value = "";
            }}
          />
        </div>

        <div style={S.qrCol}>
          {url ? (
            <>
              <img alt="扫码上传" width={148} height={148} style={S.qrImg} src={"/api/qr?data=" + encodeURIComponent(url)} />
              <div style={S.hint}>手机扫码传</div>
            </>
          ) : (
            <div style={S.hint}>二维码准备中…</div>
          )}
        </div>
      </div>

      {items.length > 0 && (
        <div style={S.list}>
          {items.map((x) => (
            <div key={x.key} style={S.listRow}>
              <span style={{ width: 18 }}>{x.state === "done" ? "✓" : x.state === "error" ? "⚠" : "…"}</span>
              <span style={S.name}>{x.name}</span>
              <span style={S.hint}>{x.state === "done" ? "已发送" : x.state === "error" ? "失败" : "发送中"}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// Content only — the host frames this view: it centres us in a safe-area clear of
// the captions (we dock them up top) and paints the surface behind us.
const S = {
  root: { display: "flex", flexDirection: "column", gap: 16, fontFamily: "-apple-system,system-ui,sans-serif" },
  title: { fontWeight: 600, fontSize: 18, textAlign: "center" },
  row: { display: "flex", flexWrap: "wrap", gap: 16, alignItems: "stretch" },
  drop: { flex: "1 1 240px", minHeight: 184, border: "2px dashed #9aa0a6", borderRadius: 16, display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center", textAlign: "center", padding: 20, cursor: "pointer", transition: "border-color .15s, background .15s" },
  dropActive: { borderColor: "#2563eb", background: "rgba(37,99,235,.08)" },
  qrCol: { flex: "0 0 auto", display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center", gap: 8, minWidth: 168 },
  qrImg: { borderRadius: 12, background: "#fff", padding: 8 },
  hint: { color: "#80868b", fontSize: 13 },
  list: { display: "flex", flexDirection: "column", gap: 6 },
  listRow: { display: "flex", gap: 8, alignItems: "center", fontSize: 14 },
  name: { flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" },
};
