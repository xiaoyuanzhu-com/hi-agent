// Built-in: 认识的人 — review who the agent has stored (faces + voices), name the
// unknown ones, pull a clip that doesn't belong out of a cluster, and auto-regroup
// a cluster that's really several people. A calm Contacts-style grid; clicking a
// card expands it in place (FLIP) into a review row — poster left, editable name +
// per-modality clip strips right/below. Naming onto an existing name merges. Every
// action posts to /api/people/*; the store is global, so no scene header is needed.
import { useState, useEffect, useRef, useCallback, useLayoutEffect } from "react";

export const captionAside = "top";

const api = {
  list: () => fetch("/api/people").then((r) => r.json()),
  name: (subject, name) =>
    fetch("/api/people/name", { method: "POST", headers: J, body: JSON.stringify({ subject, name }) }).then((r) => r.json()),
  eject: (subject, modality, stem) =>
    fetch("/api/people/eject", { method: "POST", headers: J, body: JSON.stringify({ subject, modality, stem }) }).then((r) => r.json()),
  preview: (subject, modality) =>
    fetch("/api/people/split/preview", { method: "POST", headers: J, body: JSON.stringify({ subject, modality }) }).then((r) => r.json()),
  applySplit: (subject, modality, groups) =>
    fetch("/api/people/split/apply", { method: "POST", headers: J, body: JSON.stringify({ subject, modality, groups }) }).then((r) => r.json()),
};
const J = { "Content-Type": "application/json" };
const clipUrl = (subject, modality, stem) => `/api/people/${encodeURIComponent(subject)}/${modality}/${stem}`;

export default function PeopleReview() {
  const [people, setPeople] = useState(null);
  const [openId, setOpenId] = useState(null);
  const gridRef = useRef(null);
  const rects = useRef(new Map()); // FLIP: id -> DOMRect before a change

  const reload = useCallback(async () => {
    const d = await api.list().catch(() => ({ people: [] }));
    setPeople(d.people || []);
  }, []);
  useEffect(() => { reload(); }, [reload]);

  // FLIP: snapshot every card's rect just before a layout-affecting state change.
  const snapshot = () => {
    const m = new Map();
    gridRef.current?.querySelectorAll("[data-card]").forEach((el) => m.set(el.dataset.card, el.getBoundingClientRect()));
    rects.current = m;
  };
  // After the DOM updates, invert+play the delta so cards glide to their new spots.
  useLayoutEffect(() => {
    const first = rects.current;
    if (!first.size) return;
    gridRef.current?.querySelectorAll("[data-card]").forEach((el) => {
      const f = first.get(el.dataset.card);
      if (!f) return;
      const l = el.getBoundingClientRect();
      const dx = f.left - l.left, dy = f.top - l.top, sx = f.width / l.width, sy = f.height / l.height;
      if (Math.abs(dx) < 1 && Math.abs(dy) < 1 && Math.abs(sx - 1) < 0.01 && Math.abs(sy - 1) < 0.01) return;
      el.animate(
        [{ transformOrigin: "top left", transform: `translate(${dx}px,${dy}px) scale(${sx},${sy})` }, { transform: "none" }],
        { duration: 380, easing: "cubic-bezier(.32,.72,0,1)" },
      );
    });
    rects.current = new Map();
  });

  const open = (id) => { snapshot(); setOpenId(id); };
  const close = () => { snapshot(); setOpenId(null); };

  // Promote the open card to the start of its visual row so the panel spans a row and
  // the cards ahead of it backfill upward.
  const ordered = orderForOpen(people || [], openId, gridRef.current);

  // Click outside the open card collapses it.
  useEffect(() => {
    if (!openId) return;
    const onDown = (e) => {
      const openEl = gridRef.current?.querySelector("[data-open]");
      if (openEl && !openEl.contains(e.target) && !e.target.closest("[data-card]")) close();
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, [openId]);

  if (people === null) return <div style={S.page}><div style={S.h1}>认识的人</div></div>;

  return (
    <div style={S.page}>
      <style>{"@keyframes eqpulse{0%,100%{height:10px}50%{height:26px}}"}</style>
      <div style={S.h1}>认识的人</div>
      <div style={S.grid} ref={gridRef}>
        {ordered.map((p) =>
          p.subject === openId ? (
            <Review key={p.subject} person={p} onClose={close} onChanged={reload} />
          ) : (
            <Card key={p.subject} person={p} onOpen={() => open(p.subject)} />
          ),
        )}
      </div>
    </div>
  );
}

function Card({ person, onOpen }) {
  const isFace = person.face.length > 0;
  const poster = isFace ? clipUrl(person.subject, "face", person.face[0]) : null;
  return (
    <div data-card={person.subject} style={S.card} onClick={onOpen}
      onMouseEnter={(e) => lift(e.currentTarget, true)} onMouseLeave={(e) => lift(e.currentTarget, false)}>
      {poster ? (
        <div style={{ ...S.poster, backgroundImage: `url('${poster}')` }} />
      ) : (
        <div style={{ ...S.poster, ...S.voicePoster }}><Eq /></div>
      )}
      <div style={person.named ? S.name : S.nameNone}>{person.named ? person.subject : "未命名"}</div>
    </div>
  );
}

function Review({ person, onClose, onChanged }) {
  const [name, setName] = useState(person.named ? person.subject : "");
  const [merge, setMerge] = useState("");
  const isFace = person.face.length > 0;

  const save = async () => {
    const v = name.trim();
    if (!v || v === (person.named ? person.subject : "")) return;
    await api.name(person.subject, v);
    onChanged();
  };

  return (
    <div data-card={person.subject} data-open style={S.review}>
      <div style={S.revHead}>
        {isFace ? (
          <div style={{ ...S.revPoster, backgroundImage: `url('${clipUrl(person.subject, "face", person.face[0])}')` }} />
        ) : (
          <div style={{ ...S.revPoster, ...S.voicePoster }}><Eq big /></div>
        )}
        <div style={S.revMeta}>
          <input
            style={S.nameInput}
            value={name}
            placeholder="加个名字…"
            onChange={(e) => setName(e.target.value)}
            onInput={(e) => setMerge(e.target.value.trim())}
            onBlur={save}
            onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
          />
          <div style={S.mergeHint}>{merge && merge !== person.subject ? `已经有「${merge}」的话，保存会合并到一起` : ""}</div>
        </div>
        <button style={S.close} onClick={onClose}>✕</button>
      </div>
      <div style={S.revBody}>
        {person.face.length > 0 && <ModSection person={person} modality="face" onChanged={onChanged} first />}
        {person.voice.length > 0 && <ModSection person={person} modality="voice" onChanged={onChanged} first={person.face.length === 0} />}
      </div>
    </div>
  );
}

function ModSection({ person, modality, onChanged, first }) {
  const stems = modality === "face" ? person.face : person.voice;
  const [proposal, setProposal] = useState(null);
  const messy = stems.length >= 4 || person.recurring;

  const regroup = async () => {
    const p = await api.preview(person.subject, modality);
    setProposal(p.groups && p.groups.length >= 2 ? p : { none: true });
  };

  return (
    <div style={{ ...S.modsec, ...(first ? {} : S.modsecTop) }}>
      <div style={S.secttl}>
        <span>{modality === "face" ? "人脸" : "声音"} <span style={S.cnt}>{stems.length}</span></span>
        {messy && <span style={S.regroup} onClick={regroup}>自动重新分组 ⟳</span>}
      </div>
      <div style={S.clips}>
        {stems.map((stem) => (
          <Clip key={stem} subject={person.subject} modality={modality} stem={stem} onChanged={onChanged} />
        ))}
      </div>
      {proposal && !proposal.none && (
        <Proposal person={person} modality={modality} proposal={proposal} onClose={() => setProposal(null)} onChanged={onChanged} />
      )}
      {proposal && proposal.none && <div style={S.plead}>看起来就是一个人，没必要分。<a style={S.link} onClick={() => setProposal(null)}>好</a></div>}
    </div>
  );
}

function Clip({ subject, modality, stem, onChanged }) {
  const [playing, setPlaying] = useState(false);
  const [gone, setGone] = useState(false);
  const audioRef = useRef(null);
  const url = clipUrl(subject, modality, stem);

  const play = (e) => {
    e.stopPropagation();
    if (modality === "voice") {
      if (!audioRef.current) audioRef.current = new Audio(url);
      const a = audioRef.current;
      if (playing) { a.pause(); setPlaying(false); }
      else { a.currentTime = 0; a.play().catch(() => {}); setPlaying(true); a.onended = () => setPlaying(false); }
    }
  };
  const eject = async (e) => {
    e.stopPropagation();
    setGone(true);
    setTimeout(async () => { await api.eject(subject, modality, stem); onChanged(); }, 300);
  };

  const base = modality === "face"
    ? { ...S.clip, backgroundImage: `url('${url}')`, backgroundSize: "cover", backgroundPosition: "center" }
    : { ...S.clip, ...S.voiceClip };

  return (
    <div style={{ ...base, ...(gone ? S.clipGone : {}), ...(playing ? S.clipPlaying : {}) }} onClick={play}>
      {modality === "voice" && <Eq small live={playing} />}
      <button style={S.eject} title="不是这个人" onClick={eject}>✕</button>
      <div style={S.clipPlay}>▶</div>
    </div>
  );
}

function Proposal({ person, modality, proposal, onClose, onChanged }) {
  const groups = proposal.groups;
  // Largest group keeps the name; render it flagged.
  let keepIdx = 0;
  groups.forEach((g, i) => { if (g.stems.length > groups[keepIdx].stems.length) keepIdx = i; });
  const apply = async () => {
    await api.applySplit(person.subject, modality, groups.map((g) => g.stems));
    onChanged();
  };
  return (
    <div style={S.proposal}>
      <div style={S.plead}>
        {person.named
          ? <> <b>{person.subject}</b>的{modality === "face" ? "脸" : "声音"}里像是混进了别人。大的一份还是 {person.subject}，另一份挑出来你再认。</>
          : <>这些{modality === "face" ? "脸" : "声音"}像是不止一个人，我分成了 {groups.length} 份。</>}
      </div>
      <div style={S.piles}>
        {groups.map((g, i) => (
          <div key={i} style={{ ...S.pile, ...(i === keepIdx ? S.pileKeep : {}) }}>
            <div style={S.pileHead}>
              <span>{i === keepIdx && person.named ? person.subject : `第 ${i + 1} 组`}</span>
              {i === keepIdx && <span style={S.keepTag}>保留</span>}
            </div>
            <div style={S.pileThumbs}>
              {g.stems.slice(0, 4).map((stem) =>
                modality === "face"
                  ? <div key={stem} style={{ ...S.pt, backgroundImage: `url('${clipUrl(person.subject, "face", stem)}')` }} />
                  : <div key={stem} style={{ ...S.pt, ...S.voiceClip, fontSize: 13 }}>♪</div>,
              )}
            </div>
            <div style={S.pileCnt}>{g.stems.length} 个</div>
          </div>
        ))}
      </div>
      <div style={S.pbtns}>
        <button style={S.btnGhost} onClick={onClose}>先不动</button>
        <button style={S.btnPrimary} onClick={apply}>就这样分</button>
      </div>
    </div>
  );
}

function Eq({ big, small, live }) {
  const n = 6;
  const w = big ? 5 : small ? 2.5 : 4;
  const h = big ? 22 : small ? 11 : 16;
  return (
    <div style={{ display: "flex", gap: small ? 2.5 : 4, alignItems: "center" }}>
      {Array.from({ length: n }).map((_, i) => (
        <i key={i} style={{
          width: w, height: h, borderRadius: w, display: "inline-block",
          background: live ? "#ff574d" : "var(--eqbar)", opacity: live ? 0.9 : 0.5,
          animation: live ? `eqpulse 1s ease-in-out ${i * 0.1}s infinite` : "none",
        }} />
      ))}
    </div>
  );
}

// FLIP helper: move the open card to the first slot of its visual row.
function orderForOpen(people, openId, gridEl) {
  if (!openId) return people;
  const idx = people.findIndex((p) => p.subject === openId);
  if (idx < 0) return people;
  const cols = columnsOf(gridEl);
  const rowStart = Math.floor(idx / cols) * cols;
  const arr = people.slice();
  const [openC] = arr.splice(idx, 1);
  arr.splice(rowStart, 0, openC);
  return arr;
}
function columnsOf(gridEl) {
  if (!gridEl) return 1;
  const cols = getComputedStyle(gridEl).gridTemplateColumns.split(" ").filter(Boolean).length;
  return Math.max(1, cols);
}
function lift(el, on) {
  el.style.transform = on ? "translateY(-3px)" : "none";
  el.style.boxShadow = on ? "var(--shadow-lift)" : "var(--shadow)";
}

// Inline styles keyed off host CSS vars so the view rides light/dark automatically.
const S = {
  page: { "--eqbar": "#a1a1a6", "--shadow": "0 1px 2px rgba(0,0,0,.04),0 8px 22px rgba(0,0,0,.05)",
    "--shadow-lift": "0 4px 10px rgba(0,0,0,.07),0 22px 55px rgba(0,0,0,.13)",
    padding: "8px 4px 40px", color: "var(--fg)" },
  h1: { fontSize: 30, fontWeight: 800, letterSpacing: "-.03em", marginBottom: 26 },
  grid: { display: "grid", gridTemplateColumns: "repeat(auto-fill,minmax(168px,1fr))", gap: 18, alignItems: "start" },
  card: { background: "var(--card,#fff)", borderRadius: 22, boxShadow: "var(--shadow)", overflow: "hidden",
    cursor: "pointer", transition: "transform .2s cubic-bezier(.32,.72,0,1),box-shadow .2s" },
  poster: { aspectRatio: "1/1", backgroundSize: "cover", backgroundPosition: "center", background: "#e8e8ed" },
  voicePoster: { display: "flex", alignItems: "center", justifyContent: "center",
    background: "linear-gradient(150deg,#e9e9ee,#dadae1)" },
  name: { padding: "13px 15px 15px", fontSize: 15, fontWeight: 700, letterSpacing: "-.02em" },
  nameNone: { padding: "13px 15px 15px", fontSize: 15, fontWeight: 500, color: "#a1a1a6" },

  review: { gridColumn: "1 / -1", background: "var(--card,#fff)", borderRadius: 28,
    boxShadow: "var(--shadow-lift)", overflow: "hidden" },
  revHead: { display: "flex", gap: 22, padding: "26px 28px 22px", alignItems: "center", borderBottom: "1px solid var(--line,#e8e8ed)" },
  revPoster: { width: 108, height: 108, borderRadius: 24, backgroundSize: "cover", backgroundPosition: "center", flex: "none", background: "#e8e8ed" },
  revMeta: { flex: 1, minWidth: 0 },
  nameInput: { font: "inherit", fontSize: 27, fontWeight: 800, letterSpacing: "-.03em", color: "var(--fg)",
    background: "transparent", border: "none", outline: "none", width: "100%", padding: "2px 0", borderBottom: "2px solid transparent" },
  mergeHint: { fontSize: 13, color: "#0a84ff", marginTop: 8, minHeight: 17, fontWeight: 500 },
  close: { flex: "none", width: 34, height: 34, borderRadius: "50%", border: "none", background: "var(--line,#eee)",
    color: "var(--fg)", fontSize: 16, cursor: "pointer", alignSelf: "flex-start" },
  revBody: { padding: "4px 28px 28px" },
  modsec: { paddingTop: 18 },
  modsecTop: { borderTop: "1px solid var(--line,#e8e8ed)", marginTop: 8 },
  secttl: { fontSize: 14, fontWeight: 700, margin: "14px 0", display: "flex", alignItems: "center", justifyContent: "space-between" },
  cnt: { color: "#a1a1a6", fontWeight: 500, marginLeft: 6 },
  regroup: { fontSize: 12.5, fontWeight: 600, color: "#a1a1a6", cursor: "pointer", padding: "6px 12px", borderRadius: 999 },
  clips: { display: "grid", gridTemplateColumns: "repeat(auto-fill,minmax(86px,1fr))", gap: 10 },
  clip: { position: "relative", borderRadius: 13, overflow: "hidden", background: "#e8e8ed", aspectRatio: "1/1",
    cursor: "pointer", display: "flex", alignItems: "center", justifyContent: "center",
    transition: "transform .18s,opacity .3s" },
  voiceClip: { background: "linear-gradient(150deg,#e9e9ee,#dadae1)" },
  clipGone: { opacity: 0, transform: "scale(.6)" },
  clipPlaying: { boxShadow: "0 0 0 2px #ff574d inset" },
  clipPlay: { position: "absolute", right: 6, bottom: 5, fontSize: 12, color: "#fff",
    textShadow: "0 1px 3px rgba(0,0,0,.6)", opacity: 0.9, pointerEvents: "none" },
  eject: { position: "absolute", top: 5, right: 5, width: 22, height: 22, borderRadius: "50%", border: "none",
    background: "rgba(0,0,0,.5)", color: "#fff", fontSize: 11, cursor: "pointer", display: "flex",
    alignItems: "center", justifyContent: "center", zIndex: 2 },

  proposal: { marginTop: 14, background: "var(--page,#f5f5f7)", borderRadius: 20, padding: "20px 22px" },
  plead: { fontSize: 14, color: "var(--fg)", opacity: 0.75, marginBottom: 16, lineHeight: 1.5 },
  link: { color: "#0a84ff", cursor: "pointer", marginLeft: 6, fontWeight: 600 },
  piles: { display: "flex", gap: 14, flexWrap: "wrap" },
  pile: { flex: 1, minWidth: 170, background: "var(--card,#fff)", borderRadius: 16, padding: 15, boxShadow: "var(--shadow)" },
  pileKeep: { outline: "2px solid #ff574d" },
  pileHead: { display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 11, fontSize: 14, fontWeight: 700 },
  keepTag: { fontSize: 11, fontWeight: 700, color: "#ff574d" },
  pileThumbs: { display: "flex", gap: 5, flexWrap: "wrap", marginBottom: 10 },
  pt: { width: 38, height: 38, borderRadius: 9, backgroundSize: "cover", backgroundPosition: "center",
    background: "#e8e8ed", display: "flex", alignItems: "center", justifyContent: "center", color: "#a1a1a6" },
  pileCnt: { fontSize: 12, color: "#a1a1a6" },
  pbtns: { display: "flex", gap: 11, marginTop: 18 },
  btnGhost: { padding: "11px 20px", borderRadius: 13, fontWeight: 700, fontSize: 14, cursor: "pointer", border: "none",
    background: "var(--card,#fff)", color: "var(--fg)" },
  btnPrimary: { padding: "11px 20px", borderRadius: 13, fontWeight: 700, fontSize: 14, cursor: "pointer", border: "none",
    background: "#ff574d", color: "#fff" },
};
