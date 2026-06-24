// Session-expiry guard for the login gate.
//
// When the backend runs with `HI_AGENT_AUTH=on`, the SPA's own HTML entry is
// already behind the gate — so a loaded app means an authenticated session. The
// only way to become unauthenticated mid-session is the session cookie expiring
// while the tab is open, after which the channel fetches start returning 401.
//
// This installs a one-time `fetch` wrapper that catches any 401 and does a
// single full-page navigation to the login flow (which bounces through the IdP
// and back). It is a no-op when auth is disabled (no 401s are ever produced).
let redirecting = false;

export function installAuthGate(): void {
  const original = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const res = await original(input, init);
    if (res.status === 401 && !redirecting) {
      redirecting = true;
      const next = encodeURIComponent(location.pathname + location.search);
      location.assign(`/auth/login?next=${next}`);
    }
    return res;
  };
}
