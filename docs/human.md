# hi-agent — Human cognition reference

## Goal

The guiding test for every hi-agent decision is **fidelity to the human metaphor** (see `architecture.md`). That makes *how a person actually does this* a first-class design input, not a flourish reached for after the fact. This document records the **human cognitive behaviors we've decided to model** — each as a behavior, the principle drawn from it, and the design consequence. It explains *why* the system is shaped the way it is; the mechanisms live in the subsystem docs (`memory.md`, the people-recognition design).

> The brain runs grossly desynchronized, noisy, ambiguous inputs and still produces a coherent, confident sense of the world — not by measuring precisely, but by **binding correlated evidence within tolerant windows and deferring what isn't yet clear.** Model the tolerance and the deferral, not a precise machine.

## Behaviors we model

| Human behavior | Principle | Design consequence |
|---|---|---|
| Senses arrive out of sync — vision is neurally *slower* than audio; there is no master clock | No ground-truth cross-modal timestamp exists | Keep channels asynchronous; never build a global clock |
| Audio + visual events within ~±100–200ms (asymmetric) fuse into "one event"; outside, they separate | Binding rides on a tolerance *window*, not equality | Bind by co-occurrence within a window, never by matching timestamps |
| A voice is tied to a face because lip motion correlates with the speech envelope (ventriloquism, McGurk) | Binding is causal inference over *correlated content* | Use content correlation (lip-sync) as the binding evidence, not timing alone |
| The brain continuously recalibrates its audio↔visual offset | The sync reference drifts; the offset should be *found*, not assumed | Let the correlation model search for / absorb the residual A/V offset |
| Can't place someone → we carry "someone I can't quite place" until a clearer encounter | Ambiguity is *held*, not discarded; a later clear moment resolves it | Skip = **defer, not drop**; unbound clusters hold the ambiguity until resolved |
| We recognize confidently only from clear signals, and shrug off murky ones | Abstain under ambiguity — it is normal not to know | Learn only from clear signals; gate the *bind*, let clusters accrete regardless |
| Knowing most of a group lets us place the last one by elimination | Identity resolves faster as the known world fills in | The "clear enough" bar *loosens over time* (elimination ratchet) |

## 1. Multimodal binding — "same source, same moment"

The hardest perception question hi-agent faces: a voice arrives on one channel, a face on another — are they the same person? Humans solve this constantly and effortlessly, and how they do it dictates the architecture.

**The key fact: humans have no synchronized channels and don't need them.** Raw signals are wildly out of step (light vs. sound in the air, then unequal neural transduction — vision lags audio). The brain manufactures the *feeling* of sync after the fact via a tolerance window, continuous recalibration, and — decisively — **binding on correlated content** (a mouth opening as the voice swells), not on shared timestamps. A global clock across our channels would therefore be *less* faithful, not more.

This yields three tiers of timing precision, each with a different demand:

1. **Globally, all channels, always → stay loose / asynchronous.** This is the human design. No master clock.
2. **Co-windowing to *find* candidate faces for a voice → "softly the same time" suffices** (±hundreds of ms is well inside the tolerance window).
3. **Within one binding snippet, A/V *relative* alignment → ~frame-accurate, but only locally and only at the moment of binding** — and the correlation model itself searches for and absorbs the residual offset, so nothing is hand-calibrated. The one capture requirement that falls out: at binding moments, a short *internally-synced* A/V burst, not isolated keyframes. The only global property to guarantee is that cross-channel latency is **bounded and stable**, not zero.

**Voice is the anchor.** Diarization already yields clean single-speaker turns with `[start, end]`. That downgrades the vision task from "active-speaker detection in the wild" to "does any visible face's mouth move in sync with *this* audio?" over a known window — cheaper, bounded by speech rather than video length, and fail-safe (an off-screen speaker matches no mouth, so the system declines to bind).

## 2. Learn from the clear, defer the murky

Two principles govern *when* to commit an identity, both lifted straight from human behavior:

- **Skip = defer, not discard.** A murky moment still lets the per-modality clusters accrete (the voice extends its voice cluster, the face its face cluster); only the cross-modal *bind* and the *name* are gated. The unbound cluster **is** the held ambiguity — exactly the human "someone I can't quite place." A later clean moment, or elimination once the known world is fuller, resolves it. Nothing learnable is lost; it is only postponed.

- **Loose on alignment, strict on commitment.** Be *generous* with temporal tolerance (perception physics, plus the model's own offset search) and *conservative* with the confidence bar to actually write a bind. This is justified by **cost asymmetry**: skipping a learnable moment is cheap (these are recurring people — another clean moment is coming), while a wrong bind *sticks* and needs an explicit merge/split to undo. When a mistake costs far more than waiting, abstaining is the *correct* move, not the lazy one.

## Scope

This is a reference for design intent, not a neuroscience text and not an implementation spec. It records the cognitive behaviors we've chosen to honor and the principles they imply; the embeddings, clustering, lip-sync model, and capture flow that realize them live in the people-recognition design and the memory subsystem. Add a behavior here only when it has actually shaped a decision.
