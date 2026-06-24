//! Open Graph meta tags rendered into the embedded index.html at request time.
//!
//! For v0 the tags are static. The hook is here so a future implementation
//! can vary them by current agent state (last utterance, who's online, etc.).

/// Open Graph + Twitter card metadata for the homepage.
pub struct OgTags {
    pub title: String,
    pub description: String,
    pub image: Option<String>,
    pub url: Option<String>,
}

impl OgTags {
    /// Default tags. State-aware variants would build their own `OgTags`
    /// and feed it to `render`.
    pub fn default_for_agent() -> Self {
        Self {
            title: "Hi Agent".to_string(),
            description:
                "A reference implementation of the human-interface spec.".to_string(),
            image: None,
            url: None,
        }
    }
}

/// Build the canonical tag set for the current agent state.
///
/// `_state` is the appearance module's view of the runtime. For v0 we ignore
/// it and return a constant set; the signature is here so the call site in
/// `mod.rs` doesn't have to change when state-aware OG lands.
///
/// `S` is generic so we don't depend on the still-evolving `AppState` from
/// the server module.
pub fn build<S>(_state: &S) -> OgTags {
    OgTags::default_for_agent()
}

/// Render the tags into a block of `<meta>` HTML suitable for injection just
/// before `</head>`. The output is already HTML-escaped.
pub fn render(tags: &OgTags) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("\n    <!-- hi-agent open graph -->\n");
    push_meta(&mut out, "og:type", "website");
    push_meta(&mut out, "og:title", &tags.title);
    push_meta(&mut out, "og:description", &tags.description);
    push_meta(&mut out, "og:site_name", "Hi Agent");
    if let Some(url) = &tags.url {
        push_meta(&mut out, "og:url", url);
    }
    if let Some(img) = &tags.image {
        push_meta(&mut out, "og:image", img);
    }

    push_name_meta(&mut out, "twitter:card", "summary");
    push_name_meta(&mut out, "twitter:title", &tags.title);
    push_name_meta(&mut out, "twitter:description", &tags.description);
    if let Some(img) = &tags.image {
        push_name_meta(&mut out, "twitter:image", img);
    }

    push_name_meta(&mut out, "description", &tags.description);
    out
}

fn push_meta(out: &mut String, property: &str, content: &str) {
    out.push_str("    <meta property=\"");
    out.push_str(property);
    out.push_str("\" content=\"");
    push_escaped(out, content);
    out.push_str("\" />\n");
}

fn push_name_meta(out: &mut String, name: &str, content: &str) {
    out.push_str("    <meta name=\"");
    out.push_str(name);
    out.push_str("\" content=\"");
    push_escaped(out, content);
    out.push_str("\" />\n");
}

fn push_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_default_tags() {
        let tags = OgTags::default_for_agent();
        let html = render(&tags);
        assert!(html.contains("og:title"));
        assert!(html.contains("Hi Agent"));
        assert!(html.contains("twitter:card"));
    }

    #[test]
    fn escapes_html_in_content() {
        let tags = OgTags {
            title: "a & b <c>".to_string(),
            description: "\"quoted\"".to_string(),
            image: None,
            url: None,
        };
        let html = render(&tags);
        assert!(html.contains("a &amp; b &lt;c&gt;"));
        assert!(html.contains("&quot;quoted&quot;"));
        assert!(!html.contains("<c>"));
    }
}
