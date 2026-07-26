// The remote console page, embedded at compile time. A single UTF-8 HTML
// file (vanilla JS + CSS), served verbatim by `GET /`. `include_str!`
// (rather than the `embedded.rs` `include_bytes!` pattern) keeps it a `&str`
// the router can hand straight to a text/html response.

/// The `resources/logviewer/index.html` SPA, baked into the binary.
pub const INDEX_HTML: &str = include_str!("../../resources/logviewer/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_html_is_embedded() {
        assert!(INDEX_HTML.contains("<!DOCTYPE html>"));
        assert!(INDEX_HTML.contains("/api/session"));
    }
}
