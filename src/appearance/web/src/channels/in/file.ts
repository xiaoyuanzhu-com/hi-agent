// Client for the inbound file channel.
//
// Files are handed artifacts: POST multipart bytes to /api/in/file with the
// scene header, and the backend stores them + wakes the mind on the file channel.

/**
 * Send one or more files to the agent.
 * Returns when the server has accepted the upload.
 */
export async function postInFiles(opts: {
  scene: string;
  files: File[];
  signal?: AbortSignal;
}): Promise<void> {
  const fd = new FormData();
  for (const file of opts.files) {
    fd.append("file", file, file.name || "file");
  }

  const res = await fetch("/api/in/file", {
    method: "POST",
    headers: { "X-HI-Scene": opts.scene },
    body: fd,
    signal: opts.signal,
  });

  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(
      `/api/in/file POST failed: ${res.status} ${res.statusText}${detail ? ` - ${detail}` : ""}`,
    );
  }
}
