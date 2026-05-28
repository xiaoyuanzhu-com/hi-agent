import { useEffect, useState } from "react";

export type StatusKind = "connecting" | "live" | "error";

interface HUDProps {
  peer: string;
  onPeerChange: (next: string) => void;
  status: StatusKind;
  channels: Array<{ name: string; supported: boolean; active?: boolean }>;
}

/**
 * Top HUD: status pill + live clock on the left, channel readouts on the
 * right. Designed to never compete with the orb visually — small type,
 * heavy letter-spacing, narrow vertical footprint.
 */
export function HUD({ peer, onPeerChange, status, channels }: HUDProps) {
  return (
    <>
      <header
        style={{
          position: "fixed",
          top: 16,
          left: 16,
          right: 16,
          display: "flex",
          alignItems: "flex-start",
          justifyContent: "space-between",
          gap: 16,
          zIndex: 15,
          pointerEvents: "none",
        }}
      >
        <div style={{ display: "flex", flexDirection: "column", gap: 8, pointerEvents: "auto" }}>
          <Brand status={status} />
          <PeerInput peer={peer} onPeerChange={onPeerChange} />
        </div>

        <div
          style={{
            display: "flex",
            flexDirection: "column",
            alignItems: "flex-end",
            gap: 8,
            pointerEvents: "auto",
          }}
        >
          <Clock />
          <ChannelGrid channels={channels} />
        </div>
      </header>
    </>
  );
}

function Brand({ status }: { status: StatusKind }) {
  const color =
    status === "live"
      ? "var(--cyan)"
      : status === "connecting"
        ? "var(--amber)"
        : "var(--danger)";
  const glow =
    status === "live"
      ? "var(--glow-cyan-soft)"
      : status === "error"
        ? "var(--glow-magenta)"
        : "none";
  return (
    <div
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 10,
        padding: "8px 12px",
        borderRadius: 999,
        border: "1px solid var(--line-strong)",
        background:
          "linear-gradient(180deg, rgba(20,40,80,0.45), rgba(8,12,28,0.55))",
        backdropFilter: "var(--panel-blur)",
        WebkitBackdropFilter: "var(--panel-blur)",
        boxShadow: glow,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 8,
          height: 8,
          borderRadius: "50%",
          background: color,
          boxShadow: `0 0 8px ${color}, 0 0 18px ${color}`,
          animation:
            status === "connecting" ? "hi-pulse 1.4s ease-in-out infinite" : undefined,
        }}
      />
      <span
        style={{
          fontFamily: "var(--font-mono)",
          fontSize: 12,
          letterSpacing: "0.18em",
          textTransform: "uppercase",
          color: "var(--fg)",
        }}
      >
        hi-agent
      </span>
      <span
        style={{
          width: 1,
          height: 12,
          background: "var(--line-strong)",
        }}
      />
      <span
        style={{
          fontFamily: "var(--font-mono)",
          fontSize: 11,
          letterSpacing: "0.18em",
          textTransform: "uppercase",
          color,
        }}
      >
        {status}
      </span>
    </div>
  );
}

function PeerInput({
  peer,
  onPeerChange,
}: {
  peer: string;
  onPeerChange: (next: string) => void;
}) {
  const [draft, setDraft] = useState(peer);
  useEffect(() => setDraft(peer), [peer]);

  const commit = () => {
    const next = draft.trim() || "web@local";
    if (next !== peer) onPeerChange(next);
    else setDraft(next);
  };

  return (
    <label
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 8,
        padding: "6px 10px",
        borderRadius: 8,
        border: "1px solid var(--line)",
        background: "rgba(8, 12, 28, 0.55)",
        backdropFilter: "var(--panel-blur)",
        WebkitBackdropFilter: "var(--panel-blur)",
      }}
    >
      <span
        style={{
          fontFamily: "var(--font-mono)",
          fontSize: 10,
          letterSpacing: "0.24em",
          textTransform: "uppercase",
          color: "var(--fg-mute)",
        }}
      >
        peer
      </span>
      <input
        type="text"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            (e.currentTarget as HTMLInputElement).blur();
          }
        }}
        aria-label="peer identity"
        spellCheck={false}
        style={{
          padding: 0,
          background: "transparent",
          border: "none",
          outline: "none",
          color: "var(--cyan)",
          fontFamily: "var(--font-mono)",
          fontSize: 12,
          width: 150,
          letterSpacing: "0.02em",
        }}
      />
    </label>
  );
}

function Clock() {
  const [now, setNow] = useState(() => new Date());
  useEffect(() => {
    const id = window.setInterval(() => setNow(new Date()), 1000);
    return () => window.clearInterval(id);
  }, []);
  const hh = pad(now.getHours());
  const mm = pad(now.getMinutes());
  const ss = pad(now.getSeconds());
  return (
    <div
      style={{
        fontFamily: "var(--font-mono)",
        fontSize: 11,
        letterSpacing: "0.28em",
        color: "var(--fg-dim)",
        textTransform: "uppercase",
        textAlign: "right",
      }}
    >
      <span style={{ color: "var(--fg)" }}>{hh}</span>
      <span style={{ color: "var(--fg-mute)" }}>:</span>
      <span style={{ color: "var(--fg)" }}>{mm}</span>
      <span style={{ color: "var(--fg-mute)" }}>:</span>
      <span style={{ color: "var(--cyan)" }}>{ss}</span>
      <span style={{ marginLeft: 10, color: "var(--fg-mute)" }}>
        utc{tzOffset()}
      </span>
    </div>
  );
}

function ChannelGrid({
  channels,
}: {
  channels: Array<{ name: string; supported: boolean; active?: boolean }>;
}) {
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: "repeat(auto-fit, minmax(64px, max-content))",
        gap: 6,
        justifyContent: "end",
        maxWidth: 360,
      }}
    >
      {channels.map((c) => {
        const color = !c.supported
          ? "var(--fg-mute)"
          : c.active
            ? "var(--cyan)"
            : "var(--fg-dim)";
        const border = !c.supported
          ? "var(--line)"
          : c.active
            ? "var(--cyan-soft)"
            : "var(--line-strong)";
        return (
          <span
            key={c.name}
            title={
              c.supported
                ? c.active
                  ? `${c.name} · active`
                  : `${c.name} · idle`
                : `${c.name} · 501 not implemented`
            }
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
              padding: "4px 8px",
              borderRadius: 6,
              border: `1px solid ${border}`,
              color,
              fontFamily: "var(--font-mono)",
              fontSize: 10,
              letterSpacing: "0.18em",
              textTransform: "uppercase",
              background: c.active ? "rgba(90, 246, 255, 0.06)" : "transparent",
              opacity: c.supported ? 1 : 0.55,
            }}
          >
            <span
              aria-hidden
              style={{
                width: 5,
                height: 5,
                borderRadius: "50%",
                background: color,
                boxShadow: c.active ? "0 0 8px var(--cyan)" : "none",
              }}
            />
            {c.name}
          </span>
        );
      })}
    </div>
  );
}

function pad(n: number): string {
  return n.toString().padStart(2, "0");
}

function tzOffset(): string {
  const m = -new Date().getTimezoneOffset();
  const sign = m >= 0 ? "+" : "-";
  const abs = Math.abs(m);
  const h = Math.floor(abs / 60);
  return `${sign}${pad(h)}`;
}
