import { useEffect, useRef } from "react";

interface CameraPreviewProps {
  /** The live camera stream while vision is on; null hides the preview. */
  stream: MediaStream | null;
  /** Shrink to a corner thumbnail (an agent view holds the stage). */
  pip?: boolean;
}

/**
 * The user's self-view — what the camera the agent is watching sees. Not a
 * control: it just confirms the camera is live and shows framing, mirrored like
 * any self-view (CSS only; the agent still gets the true feed). It fills the
 * screen as a calm backdrop, then yields to a corner thumbnail once an agent
 * view leads. The same <video> stays mounted across that switch so the feed
 * never re-attaches and blacks out.
 */
export function CameraPreview({ stream, pip = false }: CameraPreviewProps) {
  const ref = useRef<HTMLVideoElement | null>(null);

  // srcObject can't be set as a JSX attribute — attach imperatively, detach on
  // teardown so the element releases the stream.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.srcObject = stream;
    return () => {
      el.srcObject = null;
    };
  }, [stream]);

  if (!stream) return null;

  return (
    <div className={`hi-selfview${pip ? " hi-selfview--pip" : ""}`} aria-hidden="true">
      <video ref={ref} autoPlay muted playsInline />
    </div>
  );
}
