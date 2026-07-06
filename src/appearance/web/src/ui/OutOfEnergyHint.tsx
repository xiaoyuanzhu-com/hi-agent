import { useEffect, useState } from "react";

import { onNativeLifecycle } from "../lib/nativeBridge";

interface EnergyStatus {
  out_of_energy: boolean;
  resets_in?: string;
}

// Where 升级 points before the signed-in link is minted (and if minting fails):
// the plain account page, which just asks the user to sign in.
const FALLBACK_URL = "https://hi.xiaoyuanzhu.com/account";

/**
 * The out-of-energy hint — a small host-chrome card pinned just above the channel
 * controls (same width, so it sits centered over them). It polls
 * `/api/account/energy`; while the account is out of energy it shows a quiet
 * reassurance — you can keep typing, nothing is lost, processing just waits — and an
 * 升级 link to the account page **already signed in as this device account**. It hides
 * itself the moment energy refills. No spoken nudge: this quiet corner card replaces
 * the old 402 message.
 *
 * 升级 is a real `<a target="_blank">`, not a fetch-then-`window.open`: the signed-in
 * URL is minted once when the hint appears, so the click opens it directly within the
 * user gesture. That matters in the desktop face window (a `WKWebView`) — an async
 * `window.open` after `await` loses the user-activation and WebKit blocks it (nothing
 * opens); the native `WKUIDelegate` then routes the new-window request to the system
 * browser.
 */
export function OutOfEnergyHint() {
  const [out, setOut] = useState(false);
  const [resetsIn, setResetsIn] = useState("");
  const [href, setHref] = useState(FALLBACK_URL);

  // Poll the account's energy standing every 5s — but only while the face window is on
  // screen. The native shell dispatches foreground/background as the window opens and
  // closes, and the WebView keeps running while hidden, so a naive interval would keep
  // polling behind a shut window (the native poller already covers that, rarely). A
  // foreground transition re-polls at once: the user may have just returned from paying.
  // Sequential (poll → wait 5s), so a slow fetch never overlaps the next tick.
  useEffect(() => {
    let alive = true;
    let visible = true;
    let timer: number | undefined;
    const schedule = () => {
      if (alive && visible) timer = window.setTimeout(run, 5000);
    };
    const run = async () => {
      timer = undefined;
      try {
        const r = await fetch("/api/account/energy");
        if (r.ok) {
          const d: EnergyStatus = await r.json();
          if (alive) {
            setOut(!!d.out_of_energy);
            setResetsIn(d.resets_in ?? "");
          }
        }
      } catch {
        /* transient — try again on the next tick */
      }
      schedule();
    };
    run();
    const off = onNativeLifecycle((phase) => {
      const nowVisible = phase === "foreground";
      if (nowVisible === visible) return;
      visible = nowVisible;
      if (visible) {
        if (timer === undefined) run(); // came back to the front — re-check immediately
      } else if (timer !== undefined) {
        window.clearTimeout(timer); // hidden — stop until we're foregrounded again
        timer = undefined;
      }
    });
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
      off();
    };
  }, []);

  // Mint a fresh signed-in link once, when the hint appears. Keeping it pre-resolved
  // lets 升级 be a plain anchor that opens in the same click (see the note above).
  useEffect(() => {
    if (!out) return;
    let alive = true;
    fetch("/api/account/subscribe")
      .then((r) => r.json())
      .then((d) => {
        if (alive && d?.url) setHref(d.url);
      })
      .catch(() => {
        /* keep the fallback URL */
      });
    return () => {
      alive = false;
    };
  }, [out]);

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
        <a className="hi-oe-btn" href={href} target="_blank" rel="noopener noreferrer">
          升级
        </a>
      </div>
    </div>
  );
}
