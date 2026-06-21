# Building a view

You've been asked to build a *view* — a small React component the agent will show on
the person's screen while it talks them through it. This guide is the mechanics: what
a view is, how to author it, how to hand it back. The bar for how it should *look*
lives beside this file in `aesthetic.md` — read that too, and hold your work to it.

A view is a React component you write as JSX, the module's default export, importing
what you need as bare modules:

- `@hi/ui` — plain building blocks: `Card`, `Stack`, `Text`. Tasteful, no motion of
  their own.
- `@hi/core` — the live session as hooks: `usePresence()`, `useSpeech()`,
  `useChannels()`, `useSendText()`. Read or drive the conversation from inside a view
  with these.
- `motion/react` — Motion, when (and only when) a moment earns movement.
- `react` itself.

A minimal view:

```
import { Card, Stack, Text } from "@hi/ui";
export default function Spending() {
  return <Card><Stack><Text>Groceries crept up; everything else held steady.</Text></Stack></Card>;
}
```

Keep the view *light*. The agent shows it paced to a single spoken beat, so one view
is one idea — not a whole list crammed onto one slide. If the brief is a sequence
(a ranking, a timeline), build one view per item so the agent can walk them one at a
time; give each its own id.

**You declare where your content sits; the host places it.** You don't lay out the
whole screen. Everything on the stage — your view, the live caption words, the camera
self-view — is a *participant* the host arranges together so none sits on top of
another. Your part is to declare two things about your content and let the host place
it. Write them as a small `<name>.geom.json` beside your saved view (same base name as
the `.jsx`):

```
{ "region": "center", "size": "auto" }
```

- **`region`** — where your content sits: `center` (the default), an edge (`top` /
  `bottom` / `left` / `right`), a corner (`top_left` / `top_right` / `bottom_left` /
  `bottom_right`), or `fill` (you own the whole frame and its own background — a photo,
  a map, a dark composition).
- **`size`** — how wide your content wants to be: `compact`, `auto` (a comfortable
  default card), `wide`, or `fill`. Choose what makes *this* content look best — the
  host no longer caps every view at one width.

Return your content directly (a `Stack`, a `Card`, your own elements). For anything but
`fill`, don't reach for the viewport, full-screen backgrounds, or absolute positioning
to place yourself — that fights the frame. For `fill` the host steps back to a bare
full-screen layer and you own the background and layout. No sidecar at all is fine too
— you just get the centered default card.

**Images: never hotlink.** A remote URL can fail CORS, be hotlink-blocked, or 404 —
leaving an ugly broken box. Instead **download the image into your project folder**
with your own tools (find it via web/image search, then `curl`/fetch it to a file
next to your view), and reference it by its served path: anything you save in the
views tree is served at `/views/<the same relative path>`, so a file you write to
`badminton-top10/leader.jpg` is `<img src="/views/badminton-top10/leader.jpg">`.
That path always loads and keeps your source small.

**The words are a participant too.** While your view is on stage the host keeps
showing the conversation's words — the person's speech and the agent's lines — as
small caption pills, and it places them on whatever edge your `region` leaves freest,
clear of your content (you don't render them). If you'd rather fold the words into the
composition yourself, declare it in the sidecar and render them with `useSpeech()`
from `@hi/core`:

```
{ "region": "fill", "owns_captions": true }
```

With `owns_captions` the host's caption pills stand down. Only declare it if you
actually render the words — otherwise the person's speech goes invisible.

**It's theirs the moment they reach for it.** If they scroll or tap, the view should
yield — let them look, and don't fight it.

**See it before you hand it back.** You have no screen of your own, so a view you
never render is one you're shipping blind — and `aesthetic.md` holds you to *looking*
at it with the eye you'd use on someone else's work. You can: the running server (its
base URL is in `HI_AGENT_BASE_URL`) already serves, same-origin, everything a faithful
render needs — the import map injected into `GET /`, the `@hi/ui` / `@hi/core` /
`motion/react` shims it points at under `/assets/`, and any image you saved under
`/views/`. So the harness is small: compile your JSX to ESM (esbuild, bare imports
left intact, the way the host does), mount it in a headless browser — install one on
first run; it caches — against that import map, stub the live session (`@hi/core`'s
hooks: return a sample `useSpeech` line so the caption pills show), and screenshot to a
file. Then **`Read` the PNG** and fix what doesn't clear the bar before you save. Set
the harness up once in a views tool dir (say `_preview/`) and every later view
reuses it — like the browser, it resolves the first time and is ready after.

# Saving it and handing it back

When a view is ready, save it as a `.jsx` file in your views tree (your working
directory) — no special tool, just write the file. Put it in a project folder named
for the topic, with a short file name and the component as the module's default
export — e.g. `badminton-top10/leader.jsx`. Name it for what it *is*, not for today's
task, so a later you can find it by topic.

Your views tree is a workshop that accumulates across tasks — everything saved here
stays. Before authoring from scratch, glance at it (`ls`): partly so you don't
collide with an existing project, but mostly because the quickest, most consistent
build is often one you already have. If you — or an earlier you — made something
close, the same kind of card, last month's version of this very deck, start from it
and adapt rather than redrawing it cold. That reuse is how the workshop earns its
keep: the stock you build up is yours to draw on, and it keeps the house style
consistent for free.

The view's *ref* is that path without the `.jsx` — `badminton-top10/leader`. Report
every ref you saved back to the agent in your summary — that's the only way the agent
can put your view on screen (it calls `show_view` with the ref). If you built several
views for one presentation, save each as its own file under the project folder and
list all the refs in order, so the agent can walk them as a sequence.
