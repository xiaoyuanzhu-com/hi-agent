// The compositor's floor: one pure pass that places every participant on the
// stage — agent views AND the host's own live surfaces (the caption words, the
// camera self-view). It is the deterministic baseline (no solver, no async
// coordinator): each participant's declared `geometry` maps to a named region by
// lookup, and an absent geometry degrades to today's centered card. A later stage
// can override these placements with a verified, screenshot-checked layout; this
// floor is what runs the common 0/1-view case and whatever the coordinator hasn't
// touched.
//
// Captions and camera are placed here but NEVER built or owned here: they stay
// host-rendered, pinned surfaces (the camera <video> keeps its srcObject; the
// caption DOM keeps streaming). This module only decides WHERE each box goes — the
// "placement, not lifecycle" line the whole design rests on.

import type { Geometry, Region, SizeClass } from "../channels/out/view";

/** Stable ids for the two host-rendered participants, so a placement map can key
 * them alongside the agent views without colliding with any view id. */
export const CAPTIONS_ID = "__captions__";
export const CAMERA_ID = "__camera__";

export type ParticipantKind = "view" | "captions" | "camera";

/** One thing competing for the stage. `view`s carry declared `geometry` from the
 * wire; the host chrome (`captions`, `camera`) carries none — the floor supplies
 * their defaults. */
export interface Participant {
  id: string;
  kind: ParticipantKind;
  geometry?: Geometry;
}

/** Where the floor decided one participant sits. `region`/`size` apply to all;
 * the three flags are kind-specific (default false): `hidden` suppresses captions
 * a view renders itself; `pip` shrinks the camera to its corner; `docked` marks
 * captions as pills over content (vs. centered as the lead). */
export interface Placement {
  id: string;
  kind: ParticipantKind;
  region: Region;
  size: SizeClass;
  hidden: boolean;
  pip: boolean;
  docked: boolean;
}

/** The whole stage resolved: how far to fade the presence while content leads,
 * plus a placement per participant id. */
export interface Layout {
  demote: number;
  placements: Map<string, Placement>;
}

/** The edge a view at `region` leaves freest for the caption pills — so the words
 * sit clear of the content rather than on top of it. A centered or full-bleed view
 * leaves the bottom; an edge view leaves the opposite edge. */
function captionComplement(viewRegion: Region): Region {
  switch (viewRegion) {
    case "top":
      return "bottom";
    case "bottom":
      return "top";
    case "left":
      return "right";
    case "right":
      return "left";
    case "top_left":
      return "bottom_right";
    case "top_right":
      return "bottom_left";
    case "bottom_left":
      return "top_right";
    case "bottom_right":
      return "top_left";
    case "center":
    case "fill":
    default:
      return "bottom";
  }
}

/**
 * Place every participant. Reproduces the host's prior hand-written placement
 * exactly when no view declares geometry (the no-regression contract, locked by
 * the unit tests): the presence demotes once any view leads, the camera fills the
 * stage alone but shrinks to a pip behind a view, and the captions dock as pills
 * whenever something fills the stage — centered as the lead otherwise.
 */
export function floorLayout(participants: Participant[]): Layout {
  const views = participants.filter((p) => p.kind === "view");
  const captions = participants.find((p) => p.kind === "captions");
  const camera = participants.find((p) => p.kind === "camera");

  const overlaid = views.length > 0;
  const demote = overlaid ? 0.72 : 0;
  const placements = new Map<string, Placement>();

  // Each view sits where it declared (floor default: centered, auto-sized).
  for (const v of views) {
    placements.set(v.id, {
      id: v.id,
      kind: "view",
      region: v.geometry?.region ?? "center",
      size: v.geometry?.size ?? "auto",
      hidden: false,
      pip: false,
      docked: false,
    });
  }

  // The camera fills the stage as a backdrop when nothing leads; behind a view it
  // tucks into the lower-left corner pip.
  const cameraFill = !!camera && !overlaid;
  if (camera) {
    placements.set(camera.id, {
      id: camera.id,
      kind: "camera",
      region: cameraFill ? "fill" : "bottom_left",
      size: cameraFill ? "fill" : "compact",
      hidden: false,
      pip: overlaid,
      docked: false,
    });
  }

  // The words dock as pills whenever something fills the stage behind them — a
  // view, or the camera-as-backdrop. Over a view the topmost one drives them: it
  // may render them itself (host stands down) or push them to the edge it leaves
  // freest. Alone, they are the lead and sit centered.
  if (captions) {
    const docked = overlaid || cameraFill;
    let region: Region = "center";
    let hidden = false;
    if (overlaid) {
      const top = views[views.length - 1];
      if (top?.geometry?.owns_captions) {
        hidden = true;
      } else {
        region = captionComplement(top?.geometry?.region ?? "center");
      }
    } else if (cameraFill) {
      // Tuck into the lower-left, clear of a centered face.
      region = "bottom_left";
    }
    placements.set(captions.id, {
      id: captions.id,
      kind: "captions",
      region,
      size: "auto",
      hidden,
      pip: false,
      docked,
    });
  }

  return { demote, placements };
}
