import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// During dev, the browser only talks to Vite (:5173). Vite proxies every
// human-interface channel route to the Rust server on :8080.
//
// The proxy MUST NOT buffer: /thought and /approval are long-poll endpoints
// where the response body trickles in and body-close ends the utterance.
// http-proxy streams by default (selfHandleResponse stays false). We disable
// timeouts so a quiet long-poll is not killed mid-flight.
const HI_CHANNELS = [
  "/thought",
  "/approval",
  "/vision",
  "/audio",
  "/surface",
  "/touch",
  "/smell",
  "/taste",
];

const proxy = Object.fromEntries(
  HI_CHANNELS.map((path) => [
    path,
    {
      target: "http://127.0.0.1:8080",
      changeOrigin: false,
      ws: false,
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
  },
});
