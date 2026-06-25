//! Agent bundle — what the running model can do, and how the gaps get filled.
//!
//! The "bundle" is the agent we actually run: a base model plus the MCP polyfill
//! tools that fill in whatever it can't do natively. Cognition is *not* uniformly
//! text-only nor uniformly multimodal — it depends on the model. This module is the
//! single place that knows, per visual modality, whether the current model
//! understands it **natively** (hand it the raw bytes as a tool-result block and let
//! it reason over the pixels) or needs a **polyfill** (route the bytes through the
//! [`super::vision`] capability and hand the model back the resulting text).
//! Everything above this treats the agent as uniformly able to understand image and
//! video; this module absorbs the per-model variation so that assumption holds.
//!
//! One bundle ships today: the Claude family understands images natively but takes
//! no video, so image is [`Handling::Native`] and video is [`Handling::Polyfill`]. A
//! text-only deployment sets `HI_AGENT_VISION_NATIVE=false`; adding another base
//! model is an additive change to [`model_is_native_image`].

const ENV_MODEL: &str = "HI_AGENT_MODEL";
const ENV_VISION_NATIVE: &str = "HI_AGENT_VISION_NATIVE";

/// A kind of visual input the agent might need to understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modality {
    Image,
    Video,
}

/// How the current bundle handles a [`Modality`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handling {
    /// The model understands it directly — hand over the raw bytes (as a tool-result
    /// content block) and let it reason over the pixels.
    Native,
    /// The model can't — understand the bytes via the vision capability and hand the
    /// model the resulting text.
    Polyfill,
}

/// What the currently-configured model can do natively. Cheap to construct.
#[derive(Debug, Clone, Copy)]
pub struct Bundle {
    native_image: bool,
}

/// The bundle for the configured model, resolved from the environment (cheap, and
/// resolved per call to dodge init-ordering). An explicit `HI_AGENT_VISION_NATIVE`
/// wins; otherwise [`model_is_native_image`] infers from `HI_AGENT_MODEL`.
pub fn current() -> Bundle {
    let native_image = parse_bool(std::env::var(ENV_VISION_NATIVE).ok().as_deref())
        .unwrap_or_else(|| model_is_native_image(std::env::var(ENV_MODEL).ok().as_deref()));
    Bundle { native_image }
}

impl Bundle {
    /// How this bundle handles `modality`. Video is always [`Handling::Polyfill`]: no
    /// model reached through the Claude adapter takes video input, so a clip is always
    /// understood by the vision capability and handed over as text.
    pub fn handling(&self, modality: Modality) -> Handling {
        match modality {
            Modality::Image if self.native_image => Handling::Native,
            _ => Handling::Polyfill,
        }
    }
}

/// Whether the named model understands images natively. We ship the Claude family
/// (native image), and unset means the adapter's default (also Claude), so the
/// default is `true`. Text-only chat models exist, but aren't reliably identifiable
/// from the id string, so they are *not* guessed here — deploy those with
/// `HI_AGENT_VISION_NATIVE=false`. This is the seam future model families extend.
fn model_is_native_image(_model: Option<&str>) -> bool {
    true
}

/// Parse an explicit boolean env override; `None` when unset/blank/unrecognized, so
/// the caller falls back to inference.
fn parse_bool(s: Option<&str>) -> Option<bool> {
    match s.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_always_polyfills() {
        for native_image in [true, false] {
            assert_eq!(Bundle { native_image }.handling(Modality::Video), Handling::Polyfill);
        }
    }

    #[test]
    fn image_follows_native_flag() {
        assert_eq!(Bundle { native_image: true }.handling(Modality::Image), Handling::Native);
        assert_eq!(Bundle { native_image: false }.handling(Modality::Image), Handling::Polyfill);
    }

    #[test]
    fn explicit_override_parses() {
        assert_eq!(parse_bool(Some("false")), Some(false));
        assert_eq!(parse_bool(Some("ON")), Some(true));
        assert_eq!(parse_bool(Some("  ")), None);
        assert_eq!(parse_bool(None), None);
    }

    #[test]
    fn unset_model_defaults_native() {
        assert!(model_is_native_image(None));
        assert!(model_is_native_image(Some("claude-opus-4-8")));
    }
}
