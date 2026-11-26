//! Renders the model's Markdown replies for chat apps: Telegram's HTML
//! subset, or tidy plain text for apps that show no formatting. Splitting
//! keeps code blocks whole across parts. See
//! `docs/develop/TelegramBridge.md#showing-progress-and-replies` and
//! `docs/develop/QQBridge.md#sending-replies`.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Markdown as Telegram HTML (`parse_mode: "HTML"`): bold, italic,
/// strikethrough, code, preformatted blocks, links, and one level of
/// quote. Headings become bold lines and lists get bullets or numbers.
pub fn to_telegram_html(markdown: &str) -> String {
    render(markdown, true)
}

/// Markdown as plain text: the markers are removed, links show their
/// address, lists get bullets or numbers, and tables keep their cells.
pub fn to_plain(markdown: &str) -> String {
    render(markdown, false)
}

/// Splits Markdown like `crate::split`, closing a code block at the end of
/// a part and opening it again at the start of the next.
pub fn split_markdown(text: &str, limit: usize) -> Vec<String> {
    // Leave room for the fence lines added below.
    let room = limit.saturating_sub(16).max(1);
    let mut parts = crate::split(text, room);
    let mut open: Option<String> = None;
    for part in &mut parts {
        let reopened = open.clone();
        for line in part.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                open = match open {
                    Some(_) => None,
                    None => Some(trimmed.to_owned()),
                };
            }
        }
        if let Some(fence) = reopened {
            *part = format!("{fence}\n{part}");
        }
        if open.is_some() {
            part.push_str("\n```");
        }
    }
    parts
}

struct List {
    next: Option<u64>,
}

struct Writer {
    html: bool,
    out: String,
    lists: Vec<List>,
    quote_depth: usize,
    link: Option<String>,
    cells_in_row: usize,
}

impl Writer {
    fn text(&mut self, text: &str) {
        if self.html {
            for c in text.chars() {
                match c {
                    '&' => self.out.push_str("&amp;"),
                    '<' => self.out.push_str("&lt;"),
                    '>' => self.out.push_str("&gt;"),
                    c => self.out.push(c),
                }
            }
        } else {
            self.out.push_str(text);
        }
    }

    fn tag(&mut self, tag: &str) {
        if self.html {
            self.out.push_str(tag);
        }
    }

    /// Ends the current block with a blank line, or a single line break
    /// inside a list.
    fn end_block(&mut self) {
        let breaks = if self.lists.is_empty() { "\n\n" } else { "\n" };
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push_str(breaks);
    }

    fn newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }
}

fn render(markdown: &str, html: bool) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut w = Writer {
        html,
        out: String::new(),
        lists: Vec::new(),
        quote_depth: 0,
        link: None,
        cells_in_row: 0,
    };
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => start(&mut w, tag),
            Event::End(tag) => end(&mut w, tag),
            Event::Text(text) => w.text(&text),
            Event::Code(code) => {
                w.tag("<code>");
                w.text(&code);
                w.tag("</code>");
            }
            Event::SoftBreak | Event::HardBreak => w.out.push('\n'),
            Event::Rule => {
                w.newline();
                w.out.push_str("———");
                w.end_block();
            }
            Event::Html(raw) | Event::InlineHtml(raw) => w.text(&raw),
            Event::TaskListMarker(done) => w.text(if done { "☑ " } else { "☐ " }),
            Event::FootnoteReference(name) => w.text(&format!("[{name}]")),
            Event::InlineMath(math) | Event::DisplayMath(math) => w.text(&math),
        }
    }
    tidy(&w.out)
}

fn start(w: &mut Writer, tag: Tag<'_>) {
    match tag {
        Tag::Paragraph => {}
        Tag::Heading { .. } => {
            w.newline();
            w.tag("<b>");
        }
        Tag::BlockQuote(_) => {
            w.newline();
            w.quote_depth += 1;
            if w.quote_depth == 1 {
                w.tag("<blockquote>");
            }
            if !w.html {
                w.out.push_str("> ");
            }
        }
        Tag::CodeBlock(kind) => {
            w.newline();
            if w.html {
                match kind {
                    CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => {
                        let lang: String = lang
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || "+-_#.".contains(*c))
                            .collect();
                        w.out
                            .push_str(&format!("<pre><code class=\"language-{lang}\">"));
                    }
                    _ => w.out.push_str("<pre><code>"),
                }
            }
        }
        Tag::List(start) => {
            w.newline();
            w.lists.push(List { next: start });
        }
        Tag::Item => {
            w.newline();
            let depth = w.lists.len().saturating_sub(1);
            w.out.push_str(&"  ".repeat(depth));
            let marker = match w.lists.last_mut() {
                Some(List { next: Some(n) }) => {
                    let marker = format!("{n}. ");
                    *n += 1;
                    marker
                }
                _ => "• ".to_owned(),
            };
            w.out.push_str(&marker);
        }
        Tag::Emphasis => w.tag("<i>"),
        Tag::Strong => w.tag("<b>"),
        Tag::Strikethrough => w.tag("<s>"),
        Tag::Link { dest_url, .. } => {
            if w.html {
                let href = dest_url
                    .replace('&', "&amp;")
                    .replace('"', "&quot;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                w.out.push_str(&format!("<a href=\"{href}\">"));
            }
            w.link = Some(dest_url.to_string());
        }
        Tag::Image { dest_url, .. } => w.link = Some(dest_url.to_string()),
        Tag::Table(_) => w.newline(),
        Tag::TableHead | Tag::TableRow => {
            w.newline();
            w.cells_in_row = 0;
        }
        Tag::TableCell => {
            if w.cells_in_row > 0 {
                w.out.push_str(" | ");
            }
            w.cells_in_row += 1;
        }
        _ => {}
    }
}

fn end(w: &mut Writer, tag: TagEnd) {
    match tag {
        TagEnd::Paragraph => {
            if w.quote_depth == 0 {
                w.end_block();
            } else {
                w.newline();
            }
        }
        TagEnd::Heading(_) => {
            w.tag("</b>");
            w.end_block();
        }
        TagEnd::BlockQuote(_) => {
            while w.out.ends_with('\n') {
                w.out.pop();
            }
            if w.quote_depth == 1 {
                w.tag("</blockquote>");
            }
            w.quote_depth -= 1;
            w.out.push_str("\n\n");
        }
        TagEnd::CodeBlock => {
            while w.out.ends_with('\n') {
                w.out.pop();
            }
            w.tag("</code></pre>");
            w.end_block();
        }
        TagEnd::List(_) => {
            w.lists.pop();
            if w.lists.is_empty() {
                w.end_block();
            }
        }
        TagEnd::Item => w.newline(),
        TagEnd::Emphasis => w.tag("</i>"),
        TagEnd::Strong => w.tag("</b>"),
        TagEnd::Strikethrough => w.tag("</s>"),
        TagEnd::Link => {
            let url = w.link.take().unwrap_or_default();
            if w.html {
                w.out.push_str("</a>");
            } else if !w.out.ends_with(&url) && !url.is_empty() {
                w.out.push_str(&format!(" ({url})"));
            }
        }
        TagEnd::Image => {
            let url = w.link.take().unwrap_or_default();
            if !url.is_empty() {
                w.text(&format!(" ({url})"));
            }
        }
        TagEnd::TableHead | TagEnd::TableRow => w.out.push('\n'),
        TagEnd::Table => w.end_block(),
        _ => {}
    }
}

/// Trims trailing spaces on each line and keeps at most one blank line.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.trim().lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Review\n\nI checked **two** files and found a *small* bug in `acp.rs`.\n\n- first ~~issue~~\n- second, see [the docs](https://example.com/a?b=1&c=2)\n\n1. one\n2. two\n\n```rust\nlet x = a < b && c > d;\n```\n\n> quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";

    #[test]
    fn telegram_html_uses_its_tags_and_escapes_text() {
        let html = to_telegram_html(SAMPLE);
        assert!(html.starts_with("<b>Review</b>\n\n"), "{html}");
        assert!(
            html.contains(
                "I checked <b>two</b> files and found a <i>small</i> bug in <code>acp.rs</code>."
            ),
            "{html}"
        );
        assert!(html.contains("• first <s>issue</s>\n• second, see <a href=\"https://example.com/a?b=1&amp;c=2\">the docs</a>"), "{html}");
        assert!(html.contains("1. one\n2. two"), "{html}");
        assert!(html.contains("<pre><code class=\"language-rust\">let x = a &lt; b &amp;&amp; c &gt; d;</code></pre>"), "{html}");
        assert!(html.contains("<blockquote>quoted</blockquote>"), "{html}");
        assert!(html.contains("a | b\n1 | 2"), "{html}");
        assert!(!html.contains("**") && !html.contains("# "), "{html}");
    }

    #[test]
    fn plain_text_drops_the_markers() {
        let plain = to_plain(SAMPLE);
        assert!(
            plain.starts_with("Review\n\nI checked two files and found a small bug in acp.rs."),
            "{plain}"
        );
        assert!(
            plain.contains("• first issue\n• second, see the docs (https://example.com/a?b=1&c=2)"),
            "{plain}"
        );
        assert!(plain.contains("let x = a < b && c > d;"), "{plain}");
        assert!(plain.contains("> quoted"), "{plain}");
        assert!(!plain.contains('<') || plain.contains("a < b"), "{plain}");
        assert!(!plain.contains("```") && !plain.contains("**"), "{plain}");
    }

    #[test]
    fn bare_links_and_nested_lists_stay_readable() {
        let plain = to_plain("See <https://example.com>.\n\n- a\n  - b\n");
        assert!(plain.starts_with("See https://example.com."), "{plain}");
        assert!(plain.contains("• a\n  • b"), "{plain}");
    }

    #[test]
    fn splitting_keeps_code_blocks_closed() {
        let code: String = (0..40)
            .map(|n| format!("line {n:02} {}\n", "x".repeat(30)))
            .collect();
        let text = format!("Intro\n\n```python\n{code}```\n\nOutro");
        let parts = split_markdown(&text, 600);
        assert!(parts.len() > 2);
        for part in &parts {
            assert!(part.chars().count() <= 600);
            assert_eq!(part.matches("```").count() % 2, 0, "{part}");
        }
        assert!(parts[1].starts_with("```python\n"), "{}", parts[1]);
        // Each part renders as a whole code block.
        assert!(to_telegram_html(&parts[1]).starts_with("<pre><code class=\"language-python\">"));
    }
}
