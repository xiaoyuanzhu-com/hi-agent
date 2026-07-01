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
//! no video, so image is [`Handling::Native`] and video is [`Handling::Polyfill`].
//! Adding another base model (e.g. a text-only one) is an additive change to
//! [`model_is_native_image`], threaded from the resolved LLM model.

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

/// The bundle for the currently-shipped model family (Claude → native image). The
/// single bundle today is model-independent; [`model_is_native_image`] is the seam
/// a future non-native (e.g. text-only) model extends, threaded from the resolved
/// LLM model rather than an env var.
pub fn current() -> Bundle {
    Bundle { native_image: model_is_native_image(None) }
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
/// (native image), and `None` means the adapter's default (also Claude), so the
/// default is `true`. This is the seam future model families extend: a text-only
/// model would return `false` here, threaded from the resolved LLM model.
fn model_is_native_image(_model: Option<&str>) -> bool {
    true
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
    fn unset_model_defaults_native() {
        assert!(model_is_native_image(None));
        assert!(model_is_native_image(Some("claude-opus-4-8")));
    }
}
