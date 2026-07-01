//! Server-side syntax highlighting and Markdown rendering for the web browser.
//!
//! Highlighting uses `syntect` with the pure-Rust `fancy-regex` engine (no C
//! `onig` dependency, keeping musl/Windows builds clean). Markdown is rendered
//! with `pulldown-cmark` and then **sanitized** with `ammonia`, so untrusted
//! README content can never inject scripts or event handlers.

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;

static SYNTAXES: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);
static THEME: Lazy<Theme> = Lazy::new(|| {
    // A light theme that fits the black-and-white UI.
    ThemeSet::load_defaults().themes["InspiredGitHub"].clone()
});

/// Render `code` as a line-numbered `<table class="blob">` with syntax
/// highlighting, picking the syntax from the file name. Returns `None` when no
/// syntax matches (the caller falls back to plain, escaped rendering) so we never
/// mislabel a file.
pub fn blob_table(filename: &str, code: &str) -> Option<String> {
    let syntax = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|ext| SYNTAXES.find_syntax_by_extension(ext))
        .or_else(|| SYNTAXES.find_syntax_by_first_line(code))?;

    let mut hl = HighlightLines::new(syntax, &THEME);
    let mut out = String::from("<table class=\"blob\">");
    for (i, line) in code.lines().enumerate() {
        let ranges = hl.highlight_line(line, &SYNTAXES).ok()?;
        // syntect escapes the text content of each span, so this is XSS-safe.
        let code_html = styled_line_to_highlighted_html(&ranges, IncludeBackground::No).ok()?;
        out.push_str(&format!(
            "<tr><td class=\"ln\">{}</td><td class=\"code\">{}</td></tr>",
            i + 1,
            code_html
        ));
    }
    out.push_str("</table>");
    Some(out)
}

/// Render Markdown to sanitized HTML (safe to embed untrusted README content).
pub fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, parser);
    ammonia::clean(&unsafe_html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_known_language() {
        let html = blob_table("main.rs", "fn main() {}\n").unwrap();
        assert!(html.contains("<table class=\"blob\">"));
        assert!(html.contains("style=")); // inline highlight styles present
    }

    #[test]
    fn unknown_extension_falls_back() {
        assert!(blob_table("data.unknownext", "just text").is_none());
    }

    #[test]
    fn markdown_is_sanitized() {
        let html = render_markdown("# Hi\n\n<script>alert(1)</script>\n\n**bold**");
        assert!(html.contains("<h1>Hi</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(!html.contains("<script>")); // stripped by ammonia
    }
}
