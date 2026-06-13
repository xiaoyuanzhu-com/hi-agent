# What good looks like

You're building a view for the agent to perform on someone's screen. Treat it as a
performance piece, not a draft: make it genuinely good to look at. This file is the
taste — the bar a view has to clear. (The mechanics — authoring, saving, refs — live
beside this file in `appearance.md`.)

Make the content carry itself — and aim high while you do:

- **Sweat the craft.** Aesthetic, rich, well-composed: thoughtful layout and
  spacing, a clear visual hierarchy, the right components, polished details. A view
  should feel designed, not dumped. A good test: picture a person building this by
  hand for someone they want to impress — what would they reach for? The form is
  yours to choose, and to vary; the bar is that it's genuinely good to look at.
- **Show, don't just tell — lead with the visual.** Almost anything worth
  presenting has a picture in it: a person has a face, a place has a photo, a trend
  has a chart, an idea has a diagram or an illustration. Reach for those *first* and
  let them carry the meaning — a view that's all text when its subject has an obvious
  image is a missed shot, not a safe default. When in doubt, find the visual. Then
  art-direct it — bring in real imagery, give it one consistent vibe, and *compose*
  with it: let a photo lead, layer the words into it, frame it — a designer's slide,
  not a caption stuck under a picture. And frame the subject whole — a crop that lops
  off a face reads as a mistake, not a style.
- **Show the story, not a table.** Pick the form that lets the data's own shape
  surface, not a grid of cells.
- **Fit the treatment to why they're looking.** Something they're curious about wants
  to seduce — big imagery, drama, and if it's a set give every item its own moment;
  something they want to understand wants to orient first — a map of the whole before
  the detail; something they need to decide wants the answer up front. Same care, a
  different shape.
- **The content is the interface.** Strip the chrome — frames, dividers, legends,
  captions — and fold the meaning into the content itself.
- **Real, then beautiful.** Get it correct first and never invent data — or fake an
  image — for a nicer picture; then make that real content as polished as you can.
  If a moment wants a face, a poster, a figure you don't have, go *find the real one*
  rather than thinning it down to what's already in hand.
- **Ship it finished, never half-baked.** What goes on screen is a performance, not
  a draft. Render it and look at it with the same eye you'd judge someone else's work —
  does it clear this bar? — and fix what doesn't before you save; the first pass is
  rarely the one to ship. The classic footgun is images that don't load — author them
  the way `appearance.md` says, and remember the fix for a risky image is to *make it
  work*, not to leave it out: dropping the visual isn't the safe choice, it's the
  bland one.

House style — there isn't a fixed one, on purpose. People can ask to see anything, so
the look should come from the subject, not from a set theme; what stays constant is the
care, not the colours. Two things hold across everything. First, don't fall into the
generic-AI defaults — the reflexive near-black canvas with a lone accent, flat system
type, a grid of bordered cards, a wall of text. That's the safe middle, and it reads as
exactly that. Make each choice — palette, type, layout, motion — deliberately and fit it
to what you're showing this time: a bright, high-key page is as valid as a dark one; a
rich, polychrome palette is right when the subject earns it, and restraint is right when
colour would just be noise; type and hierarchy are choices, never a default. Second,
respect the medium: it's a landscape screen someone glances at, so fill the frame with no
dead gaps, leave room to breathe, keep it legible (comfortable line-height, body 16px or
larger), and make sure it actually renders. The conversation's live words also share that
screen — they dock as captions over your view (`appearance.md` has the mechanics), so
compose with them in mind and leave them a quiet region rather than letting them sit on
your subject. Past that, vary freely — two views on two
topics should look like two different things made with the same care.

**Motion is for meaning, not decoration.** Use `motion/react` where movement *says*
something — a thing arriving, a card moving somewhere, a view evolving as the agent
talks through it — and let those moments feel alive rather than blinking into place.
What you avoid is motion for its own sake: a still chart can stay still, and nothing
should jitter just to look busy. Keep it soft, and honor `prefers-reduced-motion`.
