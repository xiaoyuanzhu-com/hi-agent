import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// During dev, the browser only talks to Vite (:5173). Vite proxies every
// human-interface channel route — all under `/api/*` — to the Rust server on
// :8080.
//
// The proxy MUST NOT buffer: /api/out/text (and the /api/in/* observe streams)
// are long-poll/streaming endpoints where the body trickles in and body-close
// ends the utterance. http-proxy streams by default (selfHandleResponse stays
// false). We disable timeouts so a quiet long-poll is not killed mid-flight.
const proxy = Object.fromEntries(
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
      configure: (proxy: {
        on: (
          event: "error",
          handler: (
            err: Error,
            req: unknown,
            res: {
              headersSent?: boolean;
              writeHead?: (status: number, headers: Record<string, string>) => void;
              end?: (body?: string) => void;
            },
          ) => void,
        ) => void;
      }) => {
        // Best-effort: surface upstream errors instead of swallowing them.
        proxy.on("error", (err, _req, res) => {
          // eslint-disable-next-line no-console
          console.error("[vite proxy] upstream error:", err.message);
          if (res && !res.headersSent && res.writeHead && res.end) {
            try {
              res.writeHead(502, { "content-type": "text/plain" });
              res.end("upstream unreachable");
            } catch {
              // ignore
            }
          }
        });
      },
    },
  ]),
);

export default defineConfig({
  plugins: [react()],
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
