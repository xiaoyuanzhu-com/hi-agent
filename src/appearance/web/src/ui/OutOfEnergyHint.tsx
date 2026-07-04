import { useCallback, useEffect, useState } from "react";

interface EnergyStatus {
  out_of_energy: boolean;
  resets_in?: string;
}

/**
 * The out-of-energy hint — a small host-chrome card pinned just above the channel
 * controls (and the same width, so it sits centered over them). It polls
 * `/api/account/energy`; while the account is out of energy it shows a quiet
 * reassurance — you can keep typing, nothing is lost, processing just waits — and an
 * 升级 button that opens the account page already signed in as this device account.
 * It hides itself the moment energy refills (the vendor flag drops). No spoken nudge:
 * this quiet corner card replaces the old 402 message.
 */
export function OutOfEnergyHint() {
  const [out, setOut] = useState(false);
  const [resetsIn, setResetsIn] = useState("");
  const [opening, setOpening] = useState(false);

  useEffect(() => {
    let alive = true;
    const poll = async () => {
      try {
        const r = await fetch("/api/account/energy");
        if (!r.ok) return;
        const d: EnergyStatus = await r.json();
        if (!alive) return;
        setOut(!!d.out_of_energy);
        setResetsIn(d.resets_in ?? "");
      } catch {
        /* transient — try again on the next tick */
      }
    };
    poll();
    const id = window.setInterval(poll, 15000);
    return () => {
      alive = false;
      window.clearInterval(id);
    };
  }, []);

  const upgrade = useCallback(async () => {
    setOpening(true);
    try {
      const r = await fetch("/api/account/subscribe");
      const d = await r.json();
      if (d?.url) window.open(d.url, "_blank", "noopener");
    } catch {
      /* leave the button ready to retry */
    } finally {
      setOpening(false);
    }
  }, []);

  if (!out) return null;

  return (
    <div className="hi-oe" role="status" aria-live="polite">
      <div className="hi-oe-head">
        <span className="hi-oe-spark" aria-hidden>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none">
            <path
              d="M13 2 4 14h6l-1 8 9-12h-6l1-8Z"
              fill="#fd605e"
              stroke="rgba(0,0,0,0.06)"
              strokeWidth="0.5"
            />
          </svg>
        </span>
        <span className="hi-oe-title">能量用完了</span>
      </div>
      <div className="hi-oe-body">
        可以<b>继续输入，消息不会丢</b>——等能量恢复我就接着处理。
      </div>
      <div className="hi-oe-foot">
        <span className="hi-oe-reset">{resetsIn ? `${resetsIn}恢复` : "很快恢复"}</span>
        <button className="hi-oe-btn" onClick={upgrade} disabled={opening}>
          {opening ? "打开中…" : "升级"}
        </button>
      </div>
    </div>
  );
}
