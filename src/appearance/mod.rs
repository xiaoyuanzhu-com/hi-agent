//! Appearance — the agent's web surface.
//!
//! This module owns everything a browser sees: the embedded Vite SPA, the
//! Open Graph metadata, and the routes that serve them. The SPA itself lives
//! in `src/appearance/web/` and is embedded at compile time via `rust-embed`.
//!
//! ## Routes mounted by `router()`
//!
//! - `GET /`            — index.html with OG tags injected
//! - `GET /assets/*`    — hashed JS/CSS bundles from Vite
//! - `GET /favicon.ico` — favicon if present in dist/
//! - icon set + `GET /site.webmanifest` — brand icons & PWA manifest
//! - `GET /vite.svg`    — Vite's default logo if shipped
//!
//! Step 1's server module is expected to mount this router at `/` after
//! attaching all channel routes; axum matches more specific routes first, so
//! `/api/in/*` and `/api/out/*` and friends keep working.
//!
//! ## Coordination with Step 1
//!
//! Step 1 owns `src/server/mod.rs` and the `AppState` type. To stay
//! independent of that timing, `router()` is generic over `S`. When Step 1
//! lands, `crate::foundation::server::AppState` can be substituted in directly without
//! touching this file.
//!
//! ## Future seam: agent-authored, runtime-swappable skins (NOT built yet)
//!
//! The embedded SPA below is intentionally "skin 0": the default appearance.
//! The design leaves room for the agent to author and evolve its own
//! appearance at runtime without a rebuild. None of this is implemented today;
//! it is recorded here so the seam is cheap to pick up later:
//!
//! - **Storage.** Runtime skins live under `<data_dir>/appearance/skins/<id>/`
//!   (self-contained HTML/JS/CSS), with an `active.json` pointer. The embedded
//!   default is the un-deletable fallback and is served whenever no runtime
//!   skin is active.
//! - **Serving.** New routes (e.g. `GET /appearance/active`,
//!   `GET /appearance/skin/{id}/*path`) serve the active skin; a long-poll
//!   mirroring `GET /view` lets the shell hot-swap when the active skin
//!   changes.
//! - **Bridge.** A skin renders in a sandboxed iframe and talks to the host
//!   over `postMessage` only — the host streams it presence state / sentences /
//!   views and accepts a narrow `sendText`. Mic, credentials and the upstream
//!   proxy stay strictly host-side; a skin never gets same-origin.
//! - **Authoring.** The agent would emit skins the same way it already emits
//!   rich content: a streaming marker the reactor extracts (cf. the
//!   `[[view…]]` extractor in `reactor.rs`), e.g. `[[skin:register]]` /
//!   `[[skin:activate]]`, driven in the background by the heartbeat.
//! - **Safety.** Activation is gated (preview + approval) and auto-reverts to
//!   the embedded default if a skin fails to load; `GET /?skin=default` is the
//!   escape hatch. The session core (channels, presence state machine, mic) is
//!   skin-independent — today it lives in `web/src/hooks/useAgentSession.ts`,
//!   and look-and-feel is centralized in the `:root` tokens of
//!   `web/src/ui/global.css`, so a token-only re-theme needs no canvas changes.

pub mod embed;
pub mod og;

use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

/// Build the appearance router.
///
/// `S` is the server's shared state type. We don't depend on its concrete
/// shape here — none of the appearance handlers need state today. When the
/// OG layer becomes state-aware, this signature stays the same and the
/// handler bodies start using `State<S>`.
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(index))
        // The inspect console is a client-routed SPA section. Serve the same SPA
        // shell for `/inspect` and any nested path so a deep link or refresh on
        // e.g. `/inspect/sessions` boots the app, which then renders the right view.
        .route("/inspect", get(index))
        .route("/inspect/{*path}", get(index))
        // The menu-bar chat popup is another client-routed SPA surface. Serve the
        // same shell so the popover's WKWebView (and any refresh / deep link) boots
        // the app, which then renders `<Chat>` for `/chat`.
        .route("/chat", get(index))
        .route("/favicon.ico", get(favicon))
        // Brand icon set + PWA manifest. The router only serves paths it names,
        // so each root-level asset Vite copies from `public/` needs an explicit
        // route or it 404s. All are embedded from dist/ and served verbatim.
        .route("/icon.svg", get(|| async { serve_embedded("icon.svg") }))
        .route("/favicon-16x16.png", get(|| async { serve_embedded("favicon-16x16.png") }))
        .route("/favicon-32x32.png", get(|| async { serve_embedded("favicon-32x32.png") }))
        .route("/apple-touch-icon.png", get(|| async { serve_embedded("apple-touch-icon.png") }))
        .route(
            "/apple-touch-icon-precomposed.png",
            get(|| async { serve_embedded("apple-touch-icon.png") }),
        )
        .route(
            "/android-chrome-192x192.png",
            get(|| async { serve_embedded("android-chrome-192x192.png") }),
        )
        .route(
            "/android-chrome-512x512.png",
            get(|| async { serve_embedded("android-chrome-512x512.png") }),
        )
        .route("/site.webmanifest", get(|| async { serve_embedded("site.webmanifest") }))
        .route("/vite.svg", get(vite_svg))
        .route("/assets/{*path}", get(asset))
}

/// `GET /` — serve index.html with OG tags injected before `</head>`.
///
/// If the embedded `index.html` is missing (debug builds before the SPA is
/// built), respond with a small dev placeholder pointing at Vite on :5173.
async fn index() -> Response {
    let tags = og::OgTags::default_for_agent();

    match embed::get("index.html") {
        Some(file) => {
            // Inject OG tags just before </head>. Fall back to appending to
            // the document if no </head> is found (shouldn't happen with
            // Vite output, but never panic on user-driven input).
            let html = String::from_utf8_lossy(file.data.as_ref()).into_owned();
            let injection = og::render(&tags);
            let injected = match html.find("</head>") {
                Some(idx) => {
                    let mut out = String::with_capacity(html.len() + injection.len());
                    out.push_str(&html[..idx]);
                    out.push_str(&injection);
                    out.push_str(&html[idx..]);
                    out
                }
                None => format!("{html}{injection}"),
            };

            // An import map must precede the first module script — the browser
            // rejects one added after a module has begun loading — so this
            // injects ahead of Vite's `<script type="module">`, not before
            // </head> like the OG tags. It lets a runtime-imported agent view
            // module resolve `react` / `@hi/core` / `motion/react` to the same
            // shared chunks the host loaded (see web/vite.config.ts).
            let injected = inject_importmap(injected);

            html_response(injected, StatusCode::OK)
        }
        None => {
            // Debug builds without a built SPA: friendly placeholder.
            html_response(dev_placeholder(), StatusCode::OK)
        }
    }
}

/// `GET /assets/*path` — serve a hashed asset from the embedded dist.
async fn asset(Path(path): Path<String>) -> Response {
    serve_embedded(&format!("assets/{path}"))
}

/// Inject the build's `importmap.json` (if embedded) as a `<script type=
/// "importmap">` ahead of the first module script. A no-op when no map is
/// embedded (debug builds before the SPA is built).
fn inject_importmap(html: String) -> String {
    match embed::get("importmap.json") {
        Some(file) => {
            let map = String::from_utf8_lossy(file.data.as_ref());
            splice_importmap(&html, map.trim())
        }
        None => html,
    }
}

/// Splice an import map script before the first `<script type="module"` in
/// `html`. Pure (no embed access) so the ordering invariant is unit-testable.
/// Returns `html` unchanged if it has no module script.
fn splice_importmap(html: &str, map_json: &str) -> String {
    let needle = "<script type=\"module\"";
    let Some(idx) = html.find(needle) else {
        return html.to_string();
    };
    let tag = format!("<script type=\"importmap\">\n{map_json}\n    </script>\n    ");
    let mut out = String::with_capacity(html.len() + tag.len());
    out.push_str(&html[..idx]);
    out.push_str(&tag);
    out.push_str(&html[idx..]);
    out
}

async fn favicon() -> Response {
    // Some Vite setups inline a data: URI for favicon and skip the file.
    // In that case fall through to 404; the browser silently moves on.
    serve_embedded("favicon.ico")
}

async fn vite_svg() -> Response {
    serve_embedded("vite.svg")
}

fn serve_embedded(path: &str) -> Response {
    match embed::get(path) {
        Some(file) => {
            let mime = embed::content_type_for(path);
            let body = Body::from(file.data.into_owned());
            let mut resp = Response::new(body);
            if let Ok(value) = HeaderValue::from_str(mime) {
                resp.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            // Vite emits content-hashed filenames under /assets, so they are
            // safe to cache forever. Everything else gets a conservative
            // short cache — index.html is served by `index()` directly so
            // it isn't routed through here.
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=300"
            };
            if let Ok(value) = HeaderValue::from_str(cache) {
                resp.headers_mut().insert(header::CACHE_CONTROL, value);
            }
            resp
        }
        None => not_found(),
    }
}

fn html_response(body: String, status: StatusCode) -> Response {
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // index.html should never be cached — OG tags depend on runtime state.
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, must-revalidate"),
    );
    resp
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn dev_placeholder() -> String {
    // Minimal, themed to match the SPA. Mentions :5173 so a new contributor
    // knows where to look.
    r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <meta name="color-scheme" content="light dark" />
    <title>hi-agent (dev)</title>
    <style>
      :root { color-scheme: light dark; }
      body {
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto,
          Helvetica, Arial, sans-serif;
        margin: 0; padding: 48px 24px; line-height: 1.5;
        display: flex; justify-content: center;
      }
      main { max-width: 560px; }
      code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
      .hint { opacity: 0.7; font-size: 14px; margin-top: 16px; }
    </style>
  </head>
  <body>
    <main>
      <h1>hi-agent</h1>
      <p>The embedded web bundle has not been built yet.</p>
      <p>
        For development, run the Vite dev server on
        <code>http://127.0.0.1:5173</code> — it proxies the channel routes
        back to this Rust server on <code>:8080</code>.
      </p>
      <pre><code>cd src/appearance/web &amp;&amp; pnpm install &amp;&amp; pnpm dev</code></pre>
      <p class="hint">
        For a release build, <code>pnpm build</code> writes to
        <code>src/appearance/web/dist/</code> and the next
        <code>cargo build --release</code> embeds it.
      </p>
    </main>
  </body>
</html>
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_placeholder_is_self_contained_html() {
        let html = dev_placeholder();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("hi-agent"));
        assert!(html.contains(":5173"));
    }

    #[test]
    fn content_type_for_assets() {
        assert!(embed::content_type_for("x.js").starts_with("application/javascript"));
        assert!(embed::content_type_for("x.css").starts_with("text/css"));
        assert!(embed::content_type_for("x.unknown").starts_with("application/octet-stream"));
    }

    #[test]
    fn importmap_spliced_before_first_module_script() {
        let html = r#"<head><script type="module" crossorigin src="/x.js"></script></head>"#;
        let out = splice_importmap(html, r#"{"imports":{"react":"/assets/r.js"}}"#);
        let map_pos = out.find("type=\"importmap\"").expect("import map present");
        let mod_pos = out.find("type=\"module\"").expect("module script present");
        assert!(map_pos < mod_pos, "import map must precede the module script");
        assert!(out.contains(r#""react":"/assets/r.js""#));
    }

    #[test]
    fn splice_importmap_noops_without_module_script() {
        let html = "<head></head>";
        assert_eq!(splice_importmap(html, "{}"), "<head></head>");
    }
}
