//! Presence — how present the user is *to hi-agent*, built only from what the app
//! observes about its own surface and its own conversation. No system-level
//! probing (idle time, screen lock, other apps): those are privacy-ugly and, more
//! to the point, they'd measure "away from the keyboard" when what we care about
//! is "away from hi-agent" — someone typing in another app for an hour is, to us,
//! away.
//!
//! Presence is not one number and not a mode ladder; it's a point in three
//! **orthogonal** axes that combine freely (so the cases intersect — "eager but
//! screen-dark" is as real as "eager and voice-open"):
//!
//!   1. **Reach** (instantaneous) — which of the user's channels a message can
//!      land on right now: a *screen* (View out-channel), the *speaker* (Audio
//!      out-channel), and the *mic* (a live audio-in stream). Exactly observed via
//!      connection guards.
//!   2. **Expectation** (a decaying belief) — how much output the user is
//!      *awaiting*: rising when they repeatedly bring the window forward or when
//!      hi-agent owes a reply and time is passing; decaying, in the absence of any
//!      engagement, back toward *away*. Independent of which channels are open —
//!      someone can eagerly await a view with mic and speaker both off.
//!   3. **Posture** — whether a *voice conversation* is active (the mic is live),
//!      which is what licenses speaking aloud when no window is up.
//!
//! Everything here is **facts only** — a snapshot the mind reads. What to do about
//! it (report more, hold, work ahead) is the next layer's job, not this one's.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::types::Scene;

/// Recent-enough window over which repeated window activations read as "they keep
/// checking" — the eager signal. A couple of brings-to-front inside this span.
const ACTIVATION_WINDOW: Duration = Duration::from_secs(120);

/// How many activations within [`ACTIVATION_WINDOW`] count as eager.
const EAGER_ACTIVATIONS: usize = 2;

/// How long hi-agent may owe a reply before the user is read as actively waiting.
/// Short — past this they're expecting something and want progress exposed.
const OWED_EAGER: Duration = Duration::from_secs(30);

/// How long without any engagement (a message or a window activation) before the
/// user is read as away *from hi-agent*. A decayed-belief horizon: generous
/// enough that a working pause doesn't read as gone, short enough that a real
/// departure is noticed. Overrides a stale owed-reply — asked, then vanished for
/// this long, is away, not eager.
const AWAY_AFTER: Duration = Duration::from_secs(300);

/// Cap on retained activation timestamps per scene (they're pruned by age anyway).
const MAX_ACTIVATIONS: usize = 16;

/// One output channel a client can subscribe to — the reach surfaces the agent
/// can push onto.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OutChannel {
    Text,
    Audio,
    View,
}

/// How much output the user is awaiting — the decaying expectation axis.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expectation {
    /// Actively waiting: repeatedly checking, or a reply is owed and overdue.
    Eager,
    /// Around and engaged, nothing overdue.
    Present,
    /// No sign of them for a while — stepped away from hi-agent.
    Away,
}

/// Which of the user's surfaces a message can land on right now — the three the
/// user thinks in: a window, the speaker, the mic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reach {
    /// A window is open — words or a view reach them on screen. (Either the
    /// words channel or the view channel being live means a window is up.)
    pub window: bool,
    /// The speaker is on — the agent can be heard aloud.
    pub speaker: bool,
    /// The mic is live — the agent can hear them (and a voice exchange is on).
    pub mic: bool,
}

/// A read of the scene's presence across all three axes, taken at one instant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub reach: Reach,
    pub expectation: Expectation,
    /// A voice conversation is active (mirrors `reach.mic`): licenses speaking
    /// aloud even with no window up.
    pub voice_posture: bool,
}

/// Per-scene engagement bookkeeping for the expectation axis. All timestamps are
/// monotonic `Instant`s; a scene with no entry has simply never engaged.
#[derive(Default)]
struct Engagement {
    /// Last message or window activation — the recency that decays toward away.
    last_engaged: Option<Instant>,
    /// Recent window-activation instants (bring-to-front / became-foreground),
    /// pruned to [`ACTIVATION_WINDOW`].
    activations: VecDeque<Instant>,
    /// Set when a human message arrives with no reply yet delivered; the age of
    /// this is the "owed reply" clock. Cleared on delivery.
    owed_since: Option<Instant>,
}

/// Shared presence state for every scene. Cloneable handle over shared maps:
/// reach counts move with guard lifetimes (a dropped connection un-counts
/// itself), and engagement is poked by the signal sites.
#[derive(Clone, Default)]
pub struct Presence {
    /// Live out-channel subscriber counts (screen/speaker/words reach).
    channels: Arc<Mutex<HashMap<(Scene, OutChannel), usize>>>,
    /// Live mic-in stream count (mic reach / voice posture).
    mic: Arc<Mutex<HashMap<Scene, usize>>>,
    /// Expectation-axis bookkeeping.
    engagement: Arc<Mutex<HashMap<Scene, Engagement>>>,
}

impl Presence {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- Reach: connection guards -------------------------------------------

    /// Count one live out-channel subscriber until the returned guard drops.
    /// Connecting is itself a light engagement (the user showed up), so it seeds
    /// the decay clock if this scene had none.
    pub fn connect(&self, scene: &Scene, channel: OutChannel) -> PresenceGuard {
        let key = (scene.clone(), channel);
        *self.channels.lock().unwrap().entry(key.clone()).or_insert(0) += 1;
        self.touch_engaged(scene);
        PresenceGuard { channels: self.channels.clone(), key }
    }

    /// Count one live mic-in stream until the returned guard drops. Held for the
    /// life of the audio-in WebSocket, so it doubles as the voice-posture signal.
    pub fn connect_mic(&self, scene: &Scene) -> MicGuard {
        *self.mic.lock().unwrap().entry(scene.clone()).or_insert(0) += 1;
        self.touch_engaged(scene);
        MicGuard { mic: self.mic.clone(), scene: scene.clone() }
    }

    fn live(&self, scene: &Scene, channel: OutChannel) -> bool {
        self.channels.lock().unwrap().get(&(scene.clone(), channel)).copied().unwrap_or(0) > 0
    }

    fn mic_live(&self, scene: &Scene) -> bool {
        self.mic.lock().unwrap().get(scene).copied().unwrap_or(0) > 0
    }

    fn reach(&self, scene: &Scene) -> Reach {
        Reach {
            window: self.live(scene, OutChannel::View) || self.live(scene, OutChannel::Text),
            speaker: self.live(scene, OutChannel::Audio),
            mic: self.mic_live(scene),
        }
    }

    // ---- Expectation: engagement pokes --------------------------------------

    /// The window was brought forward / became foreground — the strongest eager
    /// signal ("they keep checking"). Reported first-party by the web face's own
    /// visibility/focus events.
    pub fn note_activation(&self, scene: &Scene) {
        let now = Instant::now();
        let mut map = self.engagement.lock().unwrap();
        let e = map.entry(scene.clone()).or_default();
        e.last_engaged = Some(now);
        e.activations.push_back(now);
        while e.activations.len() > MAX_ACTIVATIONS {
            e.activations.pop_front();
        }
    }

    /// A human message arrived — refresh engagement and start the owed-reply
    /// clock if nothing is owed yet.
    pub fn note_activity(&self, scene: &Scene) {
        let now = Instant::now();
        let mut map = self.engagement.lock().unwrap();
        let e = map.entry(scene.clone()).or_default();
        e.last_engaged = Some(now);
        e.owed_since.get_or_insert(now);
    }

    /// A turn finished delivering — clear the owed-reply clock. A no-op when
    /// nothing was owed.
    pub fn note_delivered(&self, scene: &Scene) {
        if let Some(e) = self.engagement.lock().unwrap().get_mut(scene) {
            e.owed_since = None;
        }
    }

    /// Seed the decay clock on first contact without starting an owed-reply.
    fn touch_engaged(&self, scene: &Scene) {
        self.engagement
            .lock()
            .unwrap()
            .entry(scene.clone())
            .or_default()
            .last_engaged
            .get_or_insert(Instant::now());
    }

    fn expectation(&self, scene: &Scene, now: Instant) -> Expectation {
        let map = self.engagement.lock().unwrap();
        let Some(e) = map.get(scene) else {
            return Expectation::Present; // connected but nothing observed yet
        };
        let engaged_ago = e.last_engaged.map(|t| now.saturating_duration_since(t));
        let recent_activations =
            e.activations.iter().filter(|t| now.saturating_duration_since(**t) < ACTIVATION_WINDOW).count();
        let owed_age = e.owed_since.map(|t| now.saturating_duration_since(t));
        classify(engaged_ago, recent_activations, owed_age)
    }

    // ---- Read ----------------------------------------------------------------

    /// The scene's presence across all three axes, right now.
    pub fn snapshot(&self, scene: &Scene) -> Snapshot {
        let reach = self.reach(scene);
        Snapshot {
            reach,
            expectation: self.expectation(scene, Instant::now()),
            voice_posture: reach.mic,
        }
    }

    /// The scene's presence as human-model facts for the mind's prompt — the
    /// expectation, then what can reach them. Facts only.
    pub fn render(&self, scene: &Scene) -> String {
        let s = self.snapshot(scene);
        format!("{} {}", render_expectation(s.expectation), render_reach(s.reach))
    }
}

/// Pure expectation policy, extracted so the fusion is unit-testable without a
/// clock. Away wins over a stale owed-reply (asked, then gone = away, not eager).
fn classify(
    engaged_ago: Option<Duration>,
    recent_activations: usize,
    owed_age: Option<Duration>,
) -> Expectation {
    if engaged_ago.is_some_and(|d| d >= AWAY_AFTER) {
        return Expectation::Away;
    }
    let eager =
        recent_activations >= EAGER_ACTIVATIONS || owed_age.is_some_and(|a| a >= OWED_EAGER);
    if eager {
        Expectation::Eager
    } else {
        Expectation::Present
    }
}

fn render_expectation(e: Expectation) -> &'static str {
    match e {
        Expectation::Eager => {
            "They're actively waiting on you right now — checking in, or expecting a reply."
        }
        Expectation::Present => "They're around.",
        Expectation::Away => {
            "No sign of them for a while — they've stepped away from hi-agent (no messages, \
             no checking in)."
        }
    }
}

fn render_reach(r: Reach) -> String {
    let mut lands = Vec::new();
    if r.window {
        lands.push("a window (words and views reach them on screen)");
    }
    if r.speaker {
        lands.push("the speaker (you can be heard aloud)");
    }
    if r.mic {
        lands.push("the mic (you're in a voice exchange)");
    }
    if lands.is_empty() {
        "Nothing is connected right now — neither words, voice, nor a view reaches them until \
         a window is up."
            .to_owned()
    } else {
        format!("Open to them: {}.", lands.join("; "))
    }
}

/// Un-counts its out-channel subscriber on drop.
pub struct PresenceGuard {
    channels: Arc<Mutex<HashMap<(Scene, OutChannel), usize>>>,
    key: (Scene, OutChannel),
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let mut map = self.channels.lock().unwrap();
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Un-counts its mic-in stream on drop.
pub struct MicGuard {
    mic: Arc<Mutex<HashMap<Scene, usize>>>,
    scene: Scene,
}

impl Drop for MicGuard {
    fn drop(&mut self) {
        let mut map = self.mic.lock().unwrap();
        if let Some(n) = map.get_mut(&self.scene) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.scene);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(s: &str) -> Scene {
        Scene(s.to_owned())
    }

    // ---- Reach ----

    #[test]
    fn out_channel_counts_follow_guard_lifetimes() {
        let p = Presence::new();
        let s = scene("boss");
        assert!(!p.live(&s, OutChannel::View));
        let g1 = p.connect(&s, OutChannel::View);
        let g2 = p.connect(&s, OutChannel::View);
        assert!(p.live(&s, OutChannel::View));
        drop(g1);
        assert!(p.live(&s, OutChannel::View));
        drop(g2);
        assert!(!p.live(&s, OutChannel::View));
    }

    #[test]
    fn mic_liveness_follows_its_guard() {
        let p = Presence::new();
        let s = scene("boss");
        assert!(!p.reach(&s).mic);
        let g = p.connect_mic(&s);
        assert!(p.reach(&s).mic);
        assert!(p.snapshot(&s).voice_posture);
        drop(g);
        assert!(!p.reach(&s).mic);
    }

    #[test]
    fn reach_reports_each_surface_independently() {
        let p = Presence::new();
        let s = scene("boss");
        let _v = p.connect(&s, OutChannel::View);
        let r = p.reach(&s);
        assert!(r.window && !r.speaker && !r.mic);
    }

    #[test]
    fn a_words_only_client_still_counts_as_a_window() {
        // A text-reply reader has a window up even with no rendered view.
        let p = Presence::new();
        let s = scene("boss");
        let _t = p.connect(&s, OutChannel::Text);
        assert!(p.reach(&s).window);
    }

    // ---- Expectation policy (pure) ----

    #[test]
    fn never_engaged_reads_present_not_away() {
        // A bare connection with nothing observed yet is neutral, not gone.
        assert_eq!(classify(None, 0, None), Expectation::Present);
    }

    #[test]
    fn stale_engagement_is_away() {
        assert_eq!(classify(Some(AWAY_AFTER), 0, None), Expectation::Away);
        assert_eq!(classify(Some(Duration::from_secs(600)), 0, None), Expectation::Away);
    }

    #[test]
    fn repeated_activation_is_eager() {
        assert_eq!(classify(Some(Duration::from_secs(2)), 2, None), Expectation::Eager);
    }

    #[test]
    fn overdue_owed_reply_is_eager() {
        assert_eq!(classify(Some(Duration::from_secs(1)), 0, Some(OWED_EAGER)), Expectation::Eager);
    }

    #[test]
    fn owed_but_fresh_is_present_not_eager() {
        // A reply owed for only a moment isn't yet "waiting".
        assert_eq!(classify(Some(Duration::from_secs(1)), 0, Some(Duration::from_secs(5))), Expectation::Present);
    }

    #[test]
    fn away_overrides_a_long_owed_reply() {
        // Asked, then vanished: staleness wins over the (huge) owed age.
        assert_eq!(
            classify(Some(Duration::from_secs(3600)), 0, Some(Duration::from_secs(3600))),
            Expectation::Away
        );
    }

    // ---- Expectation through the store ----

    #[test]
    fn fresh_activity_reads_present() {
        let p = Presence::new();
        let s = scene("boss");
        p.note_activity(&s);
        // Just messaged: engaged and owed, but not yet overdue → present.
        assert_eq!(p.snapshot(&s).expectation, Expectation::Present);
    }

    #[test]
    fn delivered_clears_the_owed_clock() {
        let p = Presence::new();
        let s = scene("boss");
        p.note_activity(&s);
        p.note_delivered(&s);
        // With nothing owed and no activations, they're just present.
        assert_eq!(p.snapshot(&s).expectation, Expectation::Present);
    }

    #[test]
    fn two_activations_read_eager() {
        let p = Presence::new();
        let s = scene("boss");
        p.note_activation(&s);
        p.note_activation(&s);
        assert_eq!(p.snapshot(&s).expectation, Expectation::Eager);
    }

    #[test]
    fn render_states_expectation_and_reach() {
        let p = Presence::new();
        let s = scene("boss");
        let _v = p.connect(&s, OutChannel::View);
        let out = p.render(&s);
        assert!(out.contains("screen"));
    }
}
