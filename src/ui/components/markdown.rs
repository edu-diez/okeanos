//! Markdown rendering widget for the conversation pane.
//!
//! Parses markdown via `pulldown-cmark` and renders to ratatui `Line`/`Span`
//! with theme-aware colors. Supports syntax highlighting for code blocks via `syntect`.
//!
//! Feature-gated behind the `markdown` feature flag. When disabled, falls back to plain text.

use crate::ui::theme::{SemanticRole, Theme};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
#[cfg(feature = "markdown")]
use syntect::highlighting::Style as SyntectStyle;

/// Render mode for markdown content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// Always render as plain text.
    Plain,
    /// Always render with markdown formatting.
    Markdown,
    /// Auto-detect markdown content.
    #[default]
    Auto,
}

impl std::str::FromStr for RenderMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "plain" => Ok(RenderMode::Plain),
            "markdown" => Ok(RenderMode::Markdown),
            "auto" => Ok(RenderMode::Auto),
            _ => Err(format!("invalid render mode: {}", s)),
        }
    }
}

/// Check if content appears to contain markdown syntax.
fn looks_like_markdown(content: &str) -> bool {
    // Heuristic: check for common markdown patterns
    content.contains("```")
        || content.contains("# ")
        || content.contains("## ")
        || content.contains("### ")
        || content.contains("**")
        || content.contains("* ")
        || content.contains("- ")
        || content.contains("> ")
        || content.contains("`")
        || content.contains("[") && content.contains("](")
}

/// Main entry point: render markdown content to ratatui Lines.
pub fn render_markdown(
    content: &str,
    theme: &Theme,
    max_width: u16,
    mode: RenderMode,
) -> Vec<Line<'static>> {
    // If markdown feature is disabled or mode is Plain, render as plain text
    #[cfg(not(feature = "markdown"))]
    {
        let _ = (theme, max_width, mode); // suppress unused warnings
        return render_plain_text(content, theme);
    }

    #[cfg(feature = "markdown")]
    {
        match mode {
            RenderMode::Plain => render_plain_text(content, theme),
            RenderMode::Markdown => render_markdown_internal(content, theme, max_width),
            RenderMode::Auto => {
                if looks_like_markdown(content) {
                    render_markdown_internal(content, theme, max_width)
                } else {
                    render_plain_text(content, theme)
                }
            }
        }
    }
}

/// Render plain text (fallback when markdown feature is disabled or mode is Plain).
fn render_plain_text(content: &str, theme: &Theme) -> Vec<Line<'static>> {
    content
        .lines()
        .map(|line| {
            Line::from(vec![Span::styled(
                line.to_string(),
                theme.style(SemanticRole::Assistant),
            )])
        })
        .collect()
}

#[cfg(feature = "markdown")]
fn render_markdown_internal(content: &str, theme: &Theme, max_width: u16) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    use syntect::{highlighting::ThemeSet, parsing::SyntaxSet};

    let parser = Parser::new_ext(content, Options::all());
    let mut lines = Vec::new();
    let mut current_line_spans: Vec<Span<'static>> = Vec::new();

    // State for nested formatting
    let mut bold_stack: usize = 0;
    let mut italic_stack: usize = 0;
    let mut code_stack: usize = 0;
    let mut heading_level: usize = 0;
    let mut in_blockquote = false;
    let mut list_depth: usize = 0;
    let mut list_counters: Vec<usize> = Vec::new();

    // Code block state
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_block_content = String::new();

    // Link state
    let mut in_link = false;
    let mut link_url = String::new();

    // Load syntect syntax set and theme once
    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    // Use a theme that works well for both light/dark - we'll map colors ourselves
    let syntect_theme = &ts.themes["base16-ocean.dark"];

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    heading_level = level as usize;
                    // Add heading prefix
                    let prefix = "█".repeat(heading_level) + " ";
                    current_line_spans.push(Span::styled(
                        prefix,
                        theme
                            .style(SemanticRole::Assistant)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                Tag::Paragraph => {
                    // Paragraph start - nothing special needed
                }
                Tag::BlockQuote(_) => {
                    in_blockquote = true;
                }
                Tag::List(_) => {
                    list_depth += 1;
                    list_counters.push(0);
                }
                Tag::Item => {
                    if list_depth > 0 {
                        *list_counters.last_mut().unwrap() += 1;
                        let counter = *list_counters.last().unwrap();
                        let indent = "  ".repeat(list_depth - 1);
                        let bullet = if counter == 1 { "• " } else { "  " }; // simplified
                        current_line_spans.push(Span::styled(
                            format!("{}{} ", indent, bullet),
                            theme.style(SemanticRole::CanvasMuted),
                        ));
                    }
                }
                Tag::CodeBlock(info) => {
                    in_code_block = true;
                    // info is CodeBlockKind which can be Fenced(String) or Indented
                    code_block_lang = match info {
                        pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                    code_block_content.clear();
                }
                Tag::Emphasis => {
                    italic_stack += 1;
                }
                Tag::Strong => {
                    bold_stack += 1;
                }
                Tag::Strikethrough => {
                    // Not directly supported in ratatui easily, treat as italic
                    italic_stack += 1;
                }
                Tag::Link { dest_url, .. } => {
                    in_link = true;
                    link_url = dest_url.to_string();
                }
                Tag::Image { .. } => {
                    // Images not supported in TUI, skip
                }
                Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => {
                    // Tables not supported, treat as plain text
                }
                Tag::FootnoteDefinition(_) | Tag::MetadataBlock(_) => {
                    // Not supported
                }
                _ => {
                    // HtmlBlock, DefinitionList, etc. - not supported
                }
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    heading_level = 0;
                    lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                }
                TagEnd::Paragraph => {
                    if !current_line_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                    }
                }
                TagEnd::BlockQuote(_) => {
                    in_blockquote = false;
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                    list_counters.pop();
                }
                TagEnd::Item => {
                    if !current_line_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                    }
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    // Render the code block with syntax highlighting
                    let code_lines = render_code_block(
                        &code_block_content,
                        &code_block_lang,
                        theme,
                        &ps,
                        syntect_theme,
                        max_width,
                    );
                    lines.extend(code_lines);
                    code_block_content.clear();
                    code_block_lang.clear();
                }
                TagEnd::Emphasis => {
                    italic_stack = italic_stack.saturating_sub(1);
                }
                TagEnd::Strong => {
                    bold_stack = bold_stack.saturating_sub(1);
                }
                TagEnd::Strikethrough => {
                    italic_stack = italic_stack.saturating_sub(1);
                }
                TagEnd::Link => {
                    in_link = false;
                    link_url.clear();
                }
                TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {
                    // Tables not supported
                }
                TagEnd::FootnoteDefinition | TagEnd::MetadataBlock(_) => {
                    // Not supported
                }
                _ => {
                    // HtmlBlock, DefinitionList, etc. - not supported
                }
            },
            Event::Text(text) => {
                if in_code_block {
                    code_block_content.push_str(&text);
                } else {
                    let mut style = theme.style(SemanticRole::Assistant);
                    if heading_level > 0 {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if bold_stack > 0 {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if italic_stack > 0 {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if code_stack > 0 {
                        style = theme.style(SemanticRole::Tool);
                    }
                    if in_blockquote {
                        style = style.fg(theme.color(SemanticRole::CanvasMuted));
                    }
                    if in_link {
                        style = style
                            .fg(theme.color(SemanticRole::Tool))
                            .add_modifier(Modifier::UNDERLINED);
                    }

                    // Handle blockquote prefix
                    if in_blockquote && current_line_spans.is_empty() {
                        current_line_spans
                            .push(Span::styled("▕ ", theme.style(SemanticRole::CanvasMuted)));
                    }

                    current_line_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(text) => {
                if !in_code_block {
                    code_stack += 1;
                    current_line_spans.push(Span::styled(
                        format!("`{}`", text),
                        theme
                            .style(SemanticRole::Tool)
                            .bg(theme.color(SemanticRole::CanvasBg)),
                    ));
                    code_stack -= 1;
                }
            }
            Event::Html(_) => {
                // Raw HTML not supported in TUI
            }
            Event::FootnoteReference(_) => {
                // Not supported
            }
            Event::SoftBreak => {
                if !in_code_block {
                    if !current_line_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                    }
                }
            }
            Event::HardBreak => {
                if !in_code_block {
                    lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                }
            }
            Event::Rule => {
                // Horizontal rule
                if !current_line_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                }
                let rule = "─".repeat(max_width as usize);
                lines.push(Line::from(vec![Span::styled(
                    rule,
                    theme.style(SemanticRole::CanvasMuted),
                )]));
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                current_line_spans
                    .push(Span::styled(marker, theme.style(SemanticRole::CanvasMuted)));
            }
            Event::InlineMath(_) | Event::DisplayMath(_) | Event::InlineHtml(_) => {
                // Not supported in TUI
            }
        }
    }

    // Flush any remaining content
    if !current_line_spans.is_empty() {
        lines.push(Line::from(current_line_spans));
    }

    lines
}

#[cfg(feature = "markdown")]
fn render_code_block(
    content: &str,
    lang: &str,
    theme: &Theme,
    ps: &syntect::parsing::SyntaxSet,
    syntect_theme: &syntect::highlighting::Theme,
    _max_width: u16,
) -> Vec<Line<'static>> {
    use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter};
    use syntect::parsing::ParseState;
    use syntect::util::LinesWithEndings;

    let mut lines = Vec::new();

    // Add language label
    if !lang.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!(" {} ", lang),
            theme
                .style(SemanticRole::CanvasMuted)
                .bg(theme.color(SemanticRole::CodeBlockBg)),
        )]));
    }

    // Find syntax definition
    let syntax = ps
        .find_syntax_by_extension(lang)
        .or_else(|| ps.find_syntax_by_name(lang))
        .unwrap_or_else(|| ps.find_syntax_plain_text());

    let highlighter = Highlighter::new(syntect_theme);
    let mut parse_state = ParseState::new(syntax);
    let mut highlight_state =
        HighlightState::new(&highlighter, syntect::parsing::ScopeStack::new());
    let mut highlighted_lines = Vec::new();

    for line in LinesWithEndings::from(content) {
        let ops = parse_state.parse_line(line, ps).unwrap_or_default();
        let iter = HighlightIterator::new(&mut highlight_state, &ops, line, &highlighter);
        let mut spans = Vec::new();

        for (style, text) in iter {
            if text.trim_end().is_empty() && text.ends_with('\n') {
                continue;
            }
            let clean_text = text.trim_end_matches('\n');
            if clean_text.is_empty() {
                continue;
            }
            spans.push(Span::styled(
                clean_text.to_string(),
                syntect_style_to_ratatui(style, theme),
            ));
        }

        if spans.is_empty() {
            // Empty line
            spans.push(Span::raw(""));
        }

        // Add indentation for code block
        let mut indented_spans = vec![Span::styled(
            "  ",
            theme
                .style(SemanticRole::CanvasMuted)
                .bg(theme.color(SemanticRole::CodeBlockBg)),
        )];
        indented_spans.extend(spans);
        highlighted_lines.push(Line::from(indented_spans));
    }

    lines.extend(highlighted_lines);

    // Add closing padding
    lines.push(Line::from(vec![Span::styled(
        "  ",
        theme
            .style(SemanticRole::CanvasMuted)
            .bg(theme.color(SemanticRole::CodeBlockBg)),
    )]));

    lines
}

#[cfg(feature = "markdown")]
fn syntect_style_to_ratatui(style: SyntectStyle, theme: &Theme) -> Style {
    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
    let bg = if style.background.r == 0 && style.background.g == 0 && style.background.b == 0 {
        theme.color(SemanticRole::CodeBlockBg)
    } else {
        Color::Rgb(style.background.r, style.background.g, style.background.b)
    };
    let mut ratatui_style = Style::default().fg(fg).bg(bg);
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
    }
    ratatui_style
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;
    use std::sync::Arc;

    fn test_theme() -> Arc<Theme> {
        Theme::default_dark()
    }

    #[test]
    fn test_render_mode_from_str() {
        assert_eq!("plain".parse::<RenderMode>(), Ok(RenderMode::Plain));
        assert_eq!("markdown".parse::<RenderMode>(), Ok(RenderMode::Markdown));
        assert_eq!("auto".parse::<RenderMode>(), Ok(RenderMode::Auto));
        assert!("invalid".parse::<RenderMode>().is_err());
    }

    #[test]
    fn test_looks_like_markdown() {
        assert!(looks_like_markdown("# Heading"));
        assert!(looks_like_markdown("**bold**"));
        assert!(looks_like_markdown("```code```"));
        assert!(looks_like_markdown("- item"));
        assert!(looks_like_markdown("> quote"));
        assert!(looks_like_markdown("[link](url)"));
        assert!(!looks_like_markdown("plain text"));
        assert!(!looks_like_markdown(""));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_plain_text_fallback() {
        let theme = test_theme();
        let lines = render_markdown("plain text", &theme, 80, RenderMode::Plain);
        assert_eq!(lines.len(), 1);
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_heading() {
        let theme = test_theme();
        let lines = render_markdown(
            "# Heading 1\n## Heading 2",
            &theme,
            80,
            RenderMode::Markdown,
        );
        assert!(lines.len() >= 2);
        // Check that heading prefix is present
        let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_text.contains("█"));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_code_block() {
        let theme = test_theme();
        let md = r#"```rust
fn main() {
    println!("hello");
}
```"#;
        let lines = render_markdown(md, &theme, 80, RenderMode::Markdown);
        assert!(lines.len() > 3); // language label + code lines + padding
        // Check language label
        let first_line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first_line.contains("rust"));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_inline_code() {
        let theme = test_theme();
        let lines = render_markdown("Use `println!` macro", &theme, 80, RenderMode::Markdown);
        let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_text.contains("println!"));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_bold_italic() {
        let theme = test_theme();
        let lines = render_markdown("**bold** and *italic*", &theme, 80, RenderMode::Markdown);
        let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_text.contains("bold"));
        assert!(line_text.contains("italic"));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_blockquote() {
        let theme = test_theme();
        let lines = render_markdown("> quoted text", &theme, 80, RenderMode::Markdown);
        let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_text.contains("▕"));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_list() {
        let theme = test_theme();
        let lines = render_markdown("- item 1\n- item 2", &theme, 80, RenderMode::Markdown);
        assert!(lines.len() >= 2);
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn test_render_link() {
        let theme = test_theme();
        let lines = render_markdown(
            "[link](https://example.com)",
            &theme,
            80,
            RenderMode::Markdown,
        );
        let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line_text.contains("link"));
    }

    #[test]
    fn test_auto_mode_plain_text() {
        let theme = test_theme();
        let lines = render_markdown("just plain text", &theme, 80, RenderMode::Auto);
        assert_eq!(lines.len(), 1);
    }
}
