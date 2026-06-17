import { defineConfig, type Plugin, type ProxyOptions } from "vite";
import react from "@vitejs/plugin-react";
import basicSsl from "@vitejs/plugin-basic-ssl";

// Resolve a path relative to this config file (ESM has no __dirname). Uses the
// global URL (no node:url import) so it needs no @types/node.
const r = (p: string) => new URL(p, import.meta.url).pathname;

// The shared-instance contract. Each shim entry (src/shared/*) re-exports a
// dependency that BOTH host chrome and agent-authored view modules must share a
// single instance of. Rollup dedupes the real module into a common chunk that
// the host and each shim reference; we emit an import map pointing every bare
// specifier at its shim chunk, so an agent module's `import {…} from "react"` /
// `"@hi/core"` resolves to the very same instance the host loaded.
const SHARED_SPECIFIERS: Record<string, string> = {
  "src/shared/react.ts": "react",
  "src/shared/react-dom.ts": "react-dom",
  "src/shared/jsx-runtime.ts": "react/jsx-runtime",
  "src/shared/motion.ts": "motion/react",
  "src/shared/core.ts": "@hi/core",
  "src/shared/ui.ts": "@hi/ui",
};

// After the bundle is built, write dist/importmap.json mapping each shared bare
// specifier to its emitted, content-hashed chunk URL. The Rust `index()` handler
// injects this map into the served HTML (Stage 2).
function emitImportMap(): Plugin {
  return {
    name: "hi-emit-importmap",
    generateBundle(_options, bundle) {
      const imports: Record<string, string> = {};
      for (const chunk of Object.values(bundle)) {
        if (chunk.type !== "chunk" || !chunk.isEntry || !chunk.facadeModuleId) continue;
        const facade = chunk.facadeModuleId.replace(/\\/g, "/");
        for (const [suffix, spec] of Object.entries(SHARED_SPECIFIERS)) {
          if (facade.endsWith(suffix)) imports[spec] = "/" + chunk.fileName;
        }
      }
      this.emitFile({
        type: "asset",
        fileName: "importmap.json",
        source: JSON.stringify({ imports }, null, 2),
      });
    },
  };
}

// Dev mirror of the import map. In prod the Rust `index()` handler injects a map
// pointing each shared specifier at its built `/assets/share-*` chunk; in dev
// there is no build, so we point them at the `src/shared/*` shim modules Vite
// serves. An agent view fetched raw from the backend then resolves `@hi/ui` /
// `react` to the very modules the host loaded (Vite dedupes the real dep), so
// host and view share one instance — exactly as prod. Only `apply: "serve"`:
// the build path already emits its own map.
//
// Why this only affects views: Vite pre-resolves the host's own bare imports at
// transform time, so they never consult the import map — only the backend-served
// view modules carry live bare specifiers for the browser to resolve.
function devImportMap(): Plugin {
  const imports = Object.fromEntries(
    Object.entries(SHARED_SPECIFIERS).map(([file, spec]) => [spec, "/" + file]),
  );
  return {
    name: "hi-dev-importmap",
    apply: "serve",
    transformIndexHtml() {
      return [
        {
          tag: "script",
          attrs: { type: "importmap" },
          children: JSON.stringify({ imports }, null, 2),
          injectTo: "head-prepend",
        },
      ];
    },
  };
}

// During dev, the browser only talks to Vite (:5173). Vite proxies every
// human-interface channel route — all under `/api/*` — to the Rust server on
// :8080.
//
// TLS is on (basic-ssl, a self-signed localhost cert) for one reason: HTTP/2.
// Browsers only negotiate h2 over TLS, and h2 multiplexes every request over a
// single connection — which matters here because the face holds ~6 long-lived
// streams at once (the channel long-polls + the mic socket + the inspect SSE)
// and HTTP/1.1's ~6-connections-per-origin cap would otherwise starve any
// further request (worklet fetch, a second tab, the inspect snapshot). Vite 7.2+
// keeps h2 even with `server.proxy` set (it moved off the h2-incapable
// `http-proxy` to `http-proxy3`); the upstream hop to :8080 stays HTTP/1.1,
// which is fine — the connection ceiling only bites browser-side.
//
// The proxy MUST NOT buffer: /api/out/text (and the /api/in/* observe streams)
// are long-poll/streaming endpoints where the body trickles in and body-close
// ends the utterance. http-proxy streams by default (selfHandleResponse stays
// false). We disable timeouts so a quiet long-poll is not killed mid-flight.
const proxy: Record<string, ProxyOptions> = Object.fromEntries(
  // `/api/*` — the human-interface channels. `/views/*` — compiled agent view
  // modules and images the Rust server serves from disk. The browser fetches these
  // by URL, so dev must reach the backend the same way prod (same-origin embed)
  // does, or every `show_view` 404s.
  ["/api", "/views"].map((path) => [
    path,
    {
      target: "http://127.0.0.1:8080",
      changeOrigin: false,
      // /api/in/audio/stream is a WebSocket (continuous mic → STT). Without
      // ws:true the proxy leaves the Upgrade handshake hanging and mic audio
      // never reaches the backend. Regular HTTP proxying is unaffected by this.
      ws: true,
      // Streaming-friendly: do not buffer, do not give up.
      proxyTimeout: 0,
      timeout: 0,
      configure: (proxy) => {
        // Best-effort: surface upstream errors instead of swallowing them.
        // http-proxy3's ProxyServer is a typed EventEmitter; the installed
        // @types/node doesn't surface its generic `.on`, so reach the listener
        // through a minimal structural shape. On an HTTP error `res` is a
        // ServerResponse; on a WS-upgrade error it's a raw Socket (no writeHead)
        // — narrow structurally before trying to reply.
        const emitter = proxy as unknown as {
          on(event: "error", handler: (err: Error, req: unknown, res: unknown) => void): void;
        };
        emitter.on("error", (err, _req, res) => {
          // eslint-disable-next-line no-console
          console.error("[vite proxy] upstream error:", err.message);
          const http = res as {
            headersSent?: boolean;
            writeHead?: (status: number, headers: Record<string, string>) => void;
            end?: (body?: string) => void;
          };
          if (http && !http.headersSent && http.writeHead && http.end) {
            try {
              http.writeHead(502, { "content-type": "text/plain" });
              http.end("upstream unreachable");
            } catch {
              // ignore
            }
          }
        });
      },
    } satisfies ProxyOptions,
  ]),
);

export default defineConfig({
  plugins: [react(), basicSsl(), emitImportMap(), devImportMap()],
  // `@hi/core` (session hooks) and `@hi/ui` (static primitives) are the stable
  // import surface both host chrome and agent-authored views author against.
  resolve: {
    alias: {
      "@hi/core": r("./src/core/index.ts"),
      "@hi/ui": r("./src/ui/kit/index.tsx"),
    },
  },
  server: {
    port: 5173,
    strictPort: true,
    proxy,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
    // The AudioWorklet module (imported via `?url`) must be a real, statically
    // served same-origin file: `AudioWorklet.addModule()` cannot load a `data:`
    // URL. Vite inlines assets under `assetsInlineLimit` (default 4096 B) as
    // base64 data URLs — and the worklet is small enough to be inlined, which
    // silently breaks mic capture. Force it to be emitted as a hashed file.
    assetsInlineLimit: (filePath) => (filePath.endsWith("pcmWorklet.js") ? false : undefined),
    rollupOptions: {
      // Keep each shim entry's full export surface (don't tree-shake an entry's
      // re-exports just because nothing in this build imports it).
      preserveEntrySignatures: "exports-only",
      input: {
        index: r("index.html"),
        "share-react": r("src/shared/react.ts"),
        "share-react-dom": r("src/shared/react-dom.ts"),
        "share-jsx-runtime": r("src/shared/jsx-runtime.ts"),
        "share-motion": r("src/shared/motion.ts"),
        "share-core": r("src/shared/core.ts"),
        "share-ui": r("src/shared/ui.ts"),
      },
    },
  },
});
