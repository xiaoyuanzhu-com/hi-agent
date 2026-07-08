import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent } from "react";
import { usePresence, useSpeech, useChannels, useSendText, useScene } from "../core";
import { useViews } from "../core/views";
import { floorLayout, CAPTIONS_ID, CAMERA_ID, type Participant } from "../core/layout";
import { postInFiles } from "../channels/in/file";
import { Atmosphere } from "./Atmosphere";
import { Presence } from "./Presence";
import { SpeechText } from "./SpeechText";
import { ViewSlot } from "./ViewSlot";
import { KeyboardFallback } from "./KeyboardFallback";
import { ChannelControls } from "./ChannelControls";
import { OutOfEnergyHint } from "./OutOfEnergyHint";
import { CameraPreview } from "./CameraPreview";

type FileDropState = "idle" | "hover" | "sending" | "sent" | "error";

function fileCountLabel(count: number): string {
  return count === 1 ? "1 file" : `${count} files`;
}

function hasFiles(e: DragEvent<HTMLElement>): boolean {
  const dt = e.dataTransfer;
  if (!dt) return false;
  if (Array.from(dt.types).includes("Files")) return true;
  return Array.from(dt.items).some((item) => item.kind === "file");
}

/**
 * The host chrome — a calm, breathing room — reading the session through
 * `@hi/core` hooks rather than owning it. The session lives in the providers
 * above this component, so the swappable `ViewSlot` below never tears down the
 * mic / audio / channel loops when the agent swaps a view.
 *
 *   Atmosphere · Presence (the agent) · SpeechText (its words) · ViewSlot
 *   (agent-authored views) · the channel controls / input line.
 *
 * Placement is one job: every participant — the agent views, the live captions,
 * and the camera self-view — is laid out by a single `floorLayout` pass. But that
 * unifies *placement*, never *lifecycle*: the captions `<div>` and `<CameraPreview>`
 * are mounted ONCE here, above the swappable `ViewSlot`, and the layout only flips
 * their props/classes. They must never move into `ViewSlot` or a participant
 * `.map()` — re-mounting `<CameraPreview>` re-acquires the camera and blacks out
 * the feed.
 */
export function Shell() {
  const scene = useScene();
  const presence = usePresence();
  const sentences = useSpeech();
  const ch = useChannels();
  const sendText = useSendText();
  const { views, meta, clear } = useViews();
  const [fileDropState, setFileDropState] = useState<FileDropState>("idle");
  const [fileDropCount, setFileDropCount] = useState(0);
  const dragDepthRef = useRef(0);
  const fileStatusTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearFileStatusTimer = useCallback(() => {
    if (fileStatusTimerRef.current !== null) {
      clearTimeout(fileStatusTimerRef.current);
      fileStatusTimerRef.current = null;
    }
  }, []);

  useEffect(() => {
    return clearFileStatusTimer;
  }, [clearFileStatusTimer]);

  const settleFileDrop = useCallback(
    (state: Extract<FileDropState, "sent" | "error">) => {
      setFileDropState(state);
      clearFileStatusTimer();
      fileStatusTimerRef.current = setTimeout(() => {
        setFileDropState("idle");
        setFileDropCount(0);
      }, 1800);
    },
    [clearFileStatusTimer],
  );

  const sendFiles = useCallback(
    async (files: File[]) => {
      if (files.length === 0) return;
      clearFileStatusTimer();
      setFileDropCount(files.length);
      setFileDropState("sending");
      try {
        await postInFiles({ scene, files });
        settleFileDrop("sent");
      } catch {
        settleFileDrop("error");
      }
    },
    [clearFileStatusTimer, scene, settleFileDrop],
  );

  const onFileDragEnter = useCallback((e: DragEvent<HTMLDivElement>) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = "copy";
    dragDepthRef.current += 1;
    clearFileStatusTimer();
    setFileDropState("hover");
  }, [clearFileStatusTimer]);

  const onFileDragOver = useCallback((e: DragEvent<HTMLDivElement>) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = "copy";
  }, []);

  const onFileDragLeave = useCallback((e: DragEvent<HTMLDivElement>) => {
    if (dragDepthRef.current === 0 && !hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
    if (dragDepthRef.current === 0) {
      setFileDropState("idle");
    }
  }, []);

  const onFileDrop = useCallback(
    (e: DragEvent<HTMLDivElement>) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      e.stopPropagation();
      dragDepthRef.current = 0;
      const files = Array.from(e.dataTransfer.files);
      if (files.length === 0) {
        setFileDropState("idle");
        setFileDropCount(0);
        return;
      }
      void sendFiles(files);
    },
    [sendFiles],
  );

  const fileDropText = useMemo(() => {
    if (fileDropState === "sending") return `Sending ${fileCountLabel(fileDropCount)}`;
    if (fileDropState === "sent") return `Sent ${fileCountLabel(fileDropCount)}`;
    if (fileDropState === "error") return "File send failed";
    return "Drop to send";
  }, [fileDropCount, fileDropState]);

  // Everything on screen is a participant. Views carry their declared geometry
  // (wire-authoritative; a module-self-declared fallback fills in for inline
  // `source` views with no wire geometry). The captions are always a participant;
  // the camera joins only while its stream is live.
  const participants: Participant[] = [
    ...views.map((v) => ({
      id: v.id,
      kind: "view" as const,
      geometry: v.geometry ?? meta.get(v.id)?.geometry,
    })),
    { id: CAPTIONS_ID, kind: "captions" as const },
    ...(ch.visionStream ? [{ id: CAMERA_ID, kind: "camera" as const }] : []),
  ];
  const { demote, placements } = floorLayout(participants);

  const captions = placements.get(CAPTIONS_ID);
  const camera = placements.get(CAMERA_ID);
  const captionsDocked = captions?.docked ?? false;

  return (
    <div
      className="hi-root"
      data-file-drop={fileDropState === "idle" ? undefined : fileDropState}
      onDragEnter={onFileDragEnter}
      onDragOver={onFileDragOver}
      onDragLeave={onFileDragLeave}
      onDrop={onFileDrop}
    >
      <Atmosphere />
      <Presence state={presence.state} demote={demote} />

      {/* PINNED participant — mounted once, here, across every layout. The layout
          only flips `pip` (fullscreen backdrop ↔ corner thumbnail); the same
          <video> stays mounted so the feed never re-attaches and blacks out. */}
      <CameraPreview stream={ch.visionStream} pip={camera?.pip ?? false} />

      {/* PINNED participant — the conversation's words. Docks as caption pills
          when something fills the stage behind them (a view or the camera), else
          sits centered as the lead. Hidden only when the topmost view renders the
          words itself. Stays at this mount site across every layout. */}
      {captions && !captions.hidden && (
        <div
          className={captionsDocked ? "hi-stage hi-stage--captions" : "hi-stage"}
          data-region={captionsDocked ? captions.region : undefined}
          // Tells the dock to pull its left edge past the camera pip (bottom-left)
          // so the bottom bar's three zones — pip · captions · controls — never overlap.
          data-camera={captionsDocked && camera?.pip ? "pip" : undefined}
        >
          <SpeechText items={captionsDocked ? sentences.slice(-1) : sentences} />
        </div>
      )}

      <ViewSlot placements={placements} />

      {/* A quiet card just above the controls while the account is out of energy —
          keep-typing reassurance + a signed-in 升级 link. Self-polling; renders
          nothing when energy is flowing. */}
      <OutOfEnergyHint />

      {/* Channel controls are always present — the session auto-starts, so there
          is no gate. Each control honestly reflects its channel's live state;
          one that couldn't be restored shows off, and a click enables it. */}
      <ChannelControls
        audioOn={ch.audioInput}
        onToggleAudio={ch.toggleAudio}
        audioError={ch.audioError}
        videoOn={ch.videoInput}
        onToggleVideo={ch.toggleVideo}
        videoError={ch.videoError}
        textOn={ch.textInput}
        onToggleText={() => ch.setTextChannel(!ch.textInput)}
        voiceOn={ch.audioOutput}
        onToggleVoice={ch.toggleAudioOutput}
        onCloseViews={clear}
      />
      <KeyboardFallback
        onSend={sendText}
        open={ch.textInput}
        onOpen={() => ch.setTextChannel(true)}
        onClose={() => ch.setTextChannel(false)}
      />
      {fileDropState !== "idle" && (
        <div className="hi-file-drop" data-state={fileDropState} role="status" aria-live="polite">
          <div className="hi-file-drop-box">
            <span className="hi-file-drop-icon" aria-hidden>
              {fileDropState === "sent" ? "✓" : fileDropState === "error" ? "!" : "↓"}
            </span>
            <span className="hi-file-drop-text">{fileDropText}</span>
          </div>
        </div>
      )}
    </div>
  );
}
