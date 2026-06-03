import { defineConfig, type ProxyOptions } from "vite";
import react from "@vitejs/plugin-react";
import basicSsl from "@vitejs/plugin-basic-ssl";

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
  ["/api"].map((path) => [
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
  plugins: [react(), basicSsl()],
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
  },
});
