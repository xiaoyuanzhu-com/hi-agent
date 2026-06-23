//! Channel presence — which output channels have a live client, per scene.
//!
//! The transport adapter reports connection liveness (a guard held for the life
//! of each `GET /api/out/*` subscriber); the reactor reads it back as
//! human-model facts — "they receive your words, but no screen is attached" —
//! so the mind can put actionable content on a channel the person actually
//! receives. Facts only; what to do about them is core.md's job.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::types::Scene;

/// One output channel a client can subscribe to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OutChannel {
    Text,
    Audio,
    View,
}

/// Live subscriber counts per (scene, channel). Cloneable handle over shared
/// state; counts move with [`PresenceGuard`] lifetimes, so a dropped connection
/// (timeout, client gone) un-counts itself.
#[derive(Clone, Default)]
pub struct Presence {
    inner: Arc<Mutex<HashMap<(Scene, OutChannel), usize>>>,
}

impl Presence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one live subscriber until the returned guard drops.
    pub fn connect(&self, scene: &Scene, channel: OutChannel) -> PresenceGuard {
        let key = (scene.clone(), channel);
        {
            let mut map = self.inner.lock().unwrap();
            *map.entry(key.clone()).or_insert(0) += 1;
        }
        PresenceGuard { inner: self.inner.clone(), key }
    }

    fn live(&self, scene: &Scene, channel: OutChannel) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(&(scene.clone(), channel))
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// The scene's presence as one human-model sentence for the mind's prompt.
    pub fn render(&self, scene: &Scene) -> String {
        let words = self.live(scene, OutChannel::Text) || self.live(scene, OutChannel::Audio);
        let screen = self.live(scene, OutChannel::View);
        match (words, screen) {
            (true, true) => {
                "They receive your words and have a screen in front of them.".to_owned()
            }
            (true, false) => "They receive your words, but no screen is attached — \
                              nothing shown in a view reaches them."
                .to_owned(),
            (false, true) => {
                "A screen is attached, but no words channel is open right now.".to_owned()
            }
            (false, false) => "No client is connected to this scene right now — \
                               nothing you say or show reaches anyone directly."
                .to_owned(),
        }
    }
}

/// Un-counts its subscriber on drop.
pub struct PresenceGuard {
    inner: Arc<Mutex<HashMap<(Scene, OutChannel), usize>>>,
    key: (Scene, OutChannel),
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let mut map = self.inner.lock().unwrap();
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
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

    #[test]
    fn counts_follow_guard_lifetimes() {
        let p = Presence::new();
        let s = scene("boss");
        assert!(!p.live(&s, OutChannel::Text));
        let g1 = p.connect(&s, OutChannel::Text);
        let g2 = p.connect(&s, OutChannel::Text);
        assert!(p.live(&s, OutChannel::Text));
        drop(g1);
        assert!(p.live(&s, OutChannel::Text));
        drop(g2);
        assert!(!p.live(&s, OutChannel::Text));
    }

    #[test]
    fn render_names_the_missing_screen() {
        let p = Presence::new();
        let s = scene("boss");
        let _g = p.connect(&s, OutChannel::Text);
        assert!(p.render(&s).contains("no screen"));
        let _v = p.connect(&s, OutChannel::View);
        assert!(p.render(&s).contains("screen in front"));
    }
}
