// Login redirect for the owner surface.
//
// With `HI_AGENT_AUTH=on` the backend gates only the owner surface (Settings,
// inspect and their APIs) — the appearance page and its channels are public, so
// loading the SPA at `/` no longer implies a session. A 401 therefore means the
// tab reached a protected endpoint without one: the owner opened Settings/inspect
// unauthenticated, or a live session cookie expired mid-use.
//
// This installs a one-time `fetch` wrapper that catches any 401 and does a
// single full-page navigation to the login flow (which bounces through the IdP
// and back). Public-face fetches never 401, so the appearance page is never
// bounced. It is a no-op when auth is disabled (no 401s are ever produced).
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
