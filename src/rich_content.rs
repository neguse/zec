//! Terminal-adapted Markdown and image presentation.
//!
//! Markdown parsing uses the option set exported by the pinned Zed `markdown`
//! crate. Image ownership stays with Zed's `Project::open_image`; this module
//! only converts already-authorized bytes into bounded terminal protocols.

use std::{
    collections::BTreeSet,
    io::{self, Cursor, Write as _},
    ops::Range,
    sync::Arc,
};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::{DynamicImage, ImageFormat};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::UnicodeWidthChar as _;

use crate::terminal::TerminalImageProtocol;

pub(crate) const MAX_MARKDOWN_SOURCE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_MARKDOWN_RENDER_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_RICH_REFERENCES: usize = 2_000;
pub(crate) const MAX_INLINE_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_DECODED_IMAGE_PIXELS: u64 = 100_000_000;
const MAX_GRAPHIC_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const KITTY_CHUNK_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RichReferenceKind {
    Link,
    Image,
}

impl RichReferenceKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Image => "image",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RichReference {
    pub(crate) number: usize,
    pub(crate) kind: RichReferenceKind,
    pub(crate) target: String,
    pub(crate) label: String,
    pub(crate) source_range: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RichSpan {
    pub(crate) text: String,
    pub(crate) style: Style,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RichLine {
    pub(crate) spans: Vec<RichSpan>,
    pub(crate) references: Vec<usize>,
}

impl RichLine {
    fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }

    fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RenderedMarkdown {
    pub(crate) lines: Vec<RichLine>,
    pub(crate) references: Vec<RichReference>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug)]
struct PendingReference {
    kind: RichReferenceKind,
    target: String,
    title: String,
    label: String,
    source_range: Range<usize>,
}

#[derive(Default)]
struct MarkdownBuilder {
    lines: Vec<RichLine>,
    current: RichLine,
    references: Vec<RichReference>,
    list_counters: Vec<Option<u64>>,
    item_prefix: Option<String>,
    continuation_prefix: String,
    quote_depth: usize,
    heading: Option<HeadingLevel>,
    emphasis: usize,
    strong: usize,
    strikethrough: usize,
    code_block: bool,
    active_link: Option<PendingReference>,
    active_image: Option<PendingReference>,
    rendered_bytes: usize,
    truncated: bool,
}

impl MarkdownBuilder {
    fn style(&self, inline_code: bool) -> Style {
        let mut style = Style::default();
        if self.heading.is_some() || self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strikethrough > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.active_link.is_some() {
            style = style.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);
        }
        if inline_code || self.code_block {
            style = style.fg(Color::Yellow);
        }
        style
    }

    fn prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        let mut prefix = "│ ".repeat(self.quote_depth);
        if let Some(item) = self.item_prefix.take() {
            prefix.push_str(&item);
            self.continuation_prefix = format!(
                "{}{}",
                "│ ".repeat(self.quote_depth),
                " ".repeat(item.chars().count())
            );
        } else if !self.continuation_prefix.is_empty() {
            prefix = self.continuation_prefix.clone();
        }
        if !prefix.is_empty() {
            self.push_raw(prefix, Style::default().add_modifier(Modifier::DIM));
        }
    }

    fn push_raw(&mut self, text: String, style: Style) {
        if text.is_empty() || self.truncated {
            return;
        }
        let remaining = MAX_MARKDOWN_RENDER_BYTES.saturating_sub(self.rendered_bytes);
        if remaining == 0 {
            self.truncated = true;
            return;
        }
        let original_len = text.len();
        let mut end = original_len.min(remaining);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            self.truncated = true;
            return;
        }
        let text = text[..end].to_owned();
        self.rendered_bytes = self.rendered_bytes.saturating_add(text.len());
        self.current.spans.push(RichSpan { text, style });
        if end < original_len {
            self.truncated = true;
        }
    }

    fn push_text(&mut self, text: &str, inline_code: bool) {
        if let Some(image) = self.active_image.as_mut() {
            image.label.push_str(text);
            return;
        }
        self.prefix();
        let text = if self.code_block {
            text.to_owned()
        } else {
            text.replace(['\r', '\n'], " ")
        };
        self.push_raw(text, self.style(inline_code));
    }

    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.lines.push(std::mem::take(&mut self.current));
        }
    }

    fn blank(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(RichLine::default());
        }
        self.continuation_prefix.clear();
    }

    fn hard_break(&mut self) {
        self.flush();
    }

    fn push_reference(&mut self, pending: PendingReference) {
        if self.references.len() >= MAX_RICH_REFERENCES {
            self.push_text(" [reference limit reached]", false);
            self.truncated = true;
            return;
        }
        let number = self.references.len().saturating_add(1);
        let label = if pending.label.trim().is_empty() {
            pending.title.clone()
        } else {
            pending.label.trim().to_owned()
        };
        self.references.push(RichReference {
            number,
            kind: pending.kind,
            target: pending.target,
            label: label.clone(),
            source_range: pending.source_range,
        });
        let marker = match pending.kind {
            RichReferenceKind::Link => format!("[{number}]"),
            RichReferenceKind::Image => {
                self.flush();
                format!("[image {number}: {label}]")
            }
        };
        self.prefix();
        self.push_raw(
            marker,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        );
        self.current.references.push(number);
        if pending.kind == RichReferenceKind::Image {
            self.flush();
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.blank();
                self.heading = Some(level);
                self.push_text(&format!("{} ", "#".repeat(level as usize)), false);
            }
            Tag::BlockQuote(kind) => {
                self.blank();
                self.quote_depth = self.quote_depth.saturating_add(1);
                if let Some(kind) = kind {
                    self.push_text(&format!("{:?}: ", kind), false);
                }
            }
            Tag::CodeBlock(kind) => {
                self.blank();
                self.code_block = true;
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.trim().is_empty()
                {
                    self.push_text(&format!("```{language}"), false);
                    self.hard_break();
                }
            }
            Tag::List(start) => {
                self.blank();
                self.list_counters.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.list_counters.len().saturating_sub(1);
                let bullet = self
                    .list_counters
                    .last()
                    .and_then(|counter| *counter)
                    .map_or_else(|| "• ".to_owned(), |counter| format!("{counter}. "));
                self.item_prefix = Some(format!("{}{bullet}", "  ".repeat(depth)));
            }
            Tag::FootnoteDefinition(label) => {
                self.blank();
                self.item_prefix = Some(format!("[^{label}]: "));
            }
            Tag::DefinitionListTitle => {
                self.blank();
                self.strong = self.strong.saturating_add(1);
            }
            Tag::DefinitionListDefinition => {
                self.flush();
                self.item_prefix = Some(": ".to_owned());
            }
            Tag::Table(_) => self.blank(),
            Tag::TableHead | Tag::TableRow => {
                self.flush();
                self.push_text("│ ", false);
            }
            Tag::TableCell => {}
            Tag::Emphasis => self.emphasis = self.emphasis.saturating_add(1),
            Tag::Strong => self.strong = self.strong.saturating_add(1),
            Tag::Strikethrough => self.strikethrough = self.strikethrough.saturating_add(1),
            Tag::Superscript => self.push_text("^", false),
            Tag::Subscript => self.push_text("~", false),
            Tag::Link {
                dest_url, title, ..
            } => {
                self.active_link = Some(PendingReference {
                    kind: RichReferenceKind::Link,
                    target: dest_url.into_string(),
                    title: title.into_string(),
                    label: String::new(),
                    source_range: range,
                });
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                self.active_image = Some(PendingReference {
                    kind: RichReferenceKind::Image,
                    target: dest_url.into_string(),
                    title: title.into_string(),
                    label: String::new(),
                    source_range: range,
                });
            }
            Tag::HtmlBlock => {
                self.blank();
                self.push_text("[HTML block omitted in terminal preview]", false);
            }
            Tag::MetadataBlock(_) | Tag::DefinitionList => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::Paragraph => self.blank(),
            TagEnd::Heading(_) => {
                self.heading = None;
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.blank();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.flush();
                self.code_block = false;
                self.blank();
            }
            TagEnd::List(_) => {
                self.blank();
                self.list_counters.pop();
            }
            TagEnd::Item => {
                self.flush();
                if let Some(Some(counter)) = self.list_counters.last_mut() {
                    *counter = counter.saturating_add(1);
                }
                self.continuation_prefix.clear();
            }
            TagEnd::FootnoteDefinition => self.blank(),
            TagEnd::DefinitionListTitle => {
                self.strong = self.strong.saturating_sub(1);
                self.flush();
            }
            TagEnd::DefinitionListDefinition => self.flush(),
            TagEnd::DefinitionList => self.blank(),
            TagEnd::TableCell => self.push_text(" │ ", false),
            TagEnd::TableRow | TagEnd::TableHead => self.flush(),
            TagEnd::Table => self.blank(),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strikethrough = self.strikethrough.saturating_sub(1),
            TagEnd::Superscript => self.push_text("^", false),
            TagEnd::Subscript => self.push_text("~", false),
            TagEnd::Link => {
                if let Some(mut link) = self.active_link.take() {
                    link.source_range.end = range.end;
                    self.push_reference(link);
                }
            }
            TagEnd::Image => {
                if let Some(mut image) = self.active_image.take() {
                    image.source_range.end = range.end;
                    self.push_reference(image);
                }
            }
            TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn finish(mut self) -> RenderedMarkdown {
        self.flush();
        while self.lines.last().is_some_and(RichLine::is_empty) {
            self.lines.pop();
        }
        if self.truncated {
            self.lines.push(RichLine {
                spans: vec![RichSpan {
                    text: "… preview truncated at the safety limit".to_owned(),
                    style: Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                }],
                references: Vec::new(),
            });
        }
        RenderedMarkdown {
            lines: self.lines,
            references: self.references,
            truncated: self.truncated,
        }
    }
}

/// Parse Markdown with the same feature flags as the pinned Zed Markdown view.
pub(crate) fn render_markdown(source: &str) -> Result<RenderedMarkdown> {
    ensure!(
        source.len() <= MAX_MARKDOWN_SOURCE_BYTES,
        "Markdown source exceeds {MAX_MARKDOWN_SOURCE_BYTES} bytes"
    );
    let mut builder = MarkdownBuilder::default();
    for (event, range) in
        Parser::new_ext(source, markdown::parser::PARSE_OPTIONS).into_offset_iter()
    {
        match event {
            Event::Start(tag) => builder.start(tag, range),
            Event::End(tag) => builder.end(tag, range),
            Event::Text(text) => {
                if let Some(link) = builder.active_link.as_mut() {
                    link.label.push_str(&text);
                }
                builder.push_text(&text, false);
            }
            Event::Code(text) => builder.push_text(&format!("`{text}`"), true),
            Event::InlineMath(text) => builder.push_text(&format!("${text}$"), true),
            Event::DisplayMath(text) => {
                builder.blank();
                builder.push_text(&format!("$${text}$$"), true);
                builder.blank();
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                if !builder.code_block {
                    builder.push_text("[HTML omitted]", false);
                }
            }
            Event::FootnoteReference(label) => builder.push_text(&format!("[^{label}]"), false),
            Event::SoftBreak => builder.push_text(" ", false),
            Event::HardBreak => builder.hard_break(),
            Event::Rule => {
                builder.blank();
                builder.push_text("────────────────────────────────", false);
                builder.blank();
            }
            Event::TaskListMarker(checked) => {
                builder.push_text(if checked { "[x] " } else { "[ ] " }, false)
            }
        }
        if builder.truncated {
            break;
        }
    }
    Ok(builder.finish())
}

fn push_wrapped_character(
    output: &mut Vec<RichLine>,
    current: &mut RichLine,
    current_width: &mut usize,
    character: char,
    style: Style,
    references: &[usize],
    width: usize,
) {
    let character_width = character.width().unwrap_or_default();
    if character_width > 0
        && *current_width > 0
        && current_width.saturating_add(character_width) > width
    {
        output.push(std::mem::take(current));
        *current_width = 0;
    }
    if let Some(span) = current.spans.last_mut().filter(|span| span.style == style) {
        span.text.push(character);
    } else {
        current.spans.push(RichSpan {
            text: character.to_string(),
            style,
        });
    }
    for reference in references {
        if !current.references.contains(reference) {
            current.references.push(*reference);
        }
    }
    *current_width = current_width.saturating_add(character_width);
}

pub(crate) fn wrap_markdown(markdown: &RenderedMarkdown, width: usize) -> Vec<RichLine> {
    let width = width.max(1);
    let mut output = Vec::new();
    for line in &markdown.lines {
        if line.is_empty() {
            output.push(RichLine::default());
            continue;
        }
        let mut current = RichLine::default();
        let mut current_width = 0usize;
        for span in &line.spans {
            for character in span.text.chars() {
                push_wrapped_character(
                    &mut output,
                    &mut current,
                    &mut current_width,
                    character,
                    span.style,
                    &line.references,
                    width,
                );
            }
        }
        output.push(current);
    }
    output
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RichContentSnapshot {
    pub(crate) title: String,
    pub(crate) lines: Vec<RichLine>,
    pub(crate) scroll: usize,
    pub(crate) selected_reference: Option<usize>,
    pub(crate) status: String,
}

pub(crate) struct RichContentWidget<'a> {
    snapshot: &'a RichContentSnapshot,
}

#[derive(Clone, Debug)]
struct MarkdownContent {
    source: String,
    rendered: RenderedMarkdown,
}

#[derive(Clone, Debug)]
pub(crate) struct RichImagePresentation {
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) format: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) file_size: u64,
    pub(crate) bytes: Arc<[u8]>,
}

#[derive(Clone, Debug)]
enum RichContentMode {
    Markdown(MarkdownContent),
    Image {
        image: RichImagePresentation,
        back: Option<Box<MarkdownContent>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RichContentState {
    mode: RichContentMode,
    scroll: usize,
    selected_reference: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RichContentInput {
    Unhandled,
    Consumed,
    ClosePreview,
    Back,
    Activate(RichReference),
}

impl RichContentState {
    pub(crate) fn markdown(source: String) -> Result<Self> {
        let rendered = render_markdown(&source)?;
        Ok(Self {
            mode: RichContentMode::Markdown(MarkdownContent { source, rendered }),
            scroll: 0,
            selected_reference: None,
        })
    }

    pub(crate) fn image(image: RichImagePresentation) -> Self {
        Self {
            mode: RichContentMode::Image { image, back: None },
            scroll: 0,
            selected_reference: None,
        }
    }

    pub(crate) fn show_image(&mut self, image: RichImagePresentation) {
        let back = match std::mem::replace(
            &mut self.mode,
            RichContentMode::Image {
                image: image.clone(),
                back: None,
            },
        ) {
            RichContentMode::Markdown(markdown) => Some(Box::new(markdown)),
            RichContentMode::Image { back, .. } => back,
        };
        self.mode = RichContentMode::Image { image, back };
        self.scroll = 0;
        self.selected_reference = None;
    }

    pub(crate) fn is_image(&self) -> bool {
        matches!(self.mode, RichContentMode::Image { .. })
    }

    pub(crate) fn is_direct_image(&self) -> bool {
        matches!(self.mode, RichContentMode::Image { back: None, .. })
    }

    pub(crate) fn refresh_markdown(&mut self, source: String) -> Result<bool> {
        let RichContentMode::Markdown(markdown) = &mut self.mode else {
            return Ok(false);
        };
        if markdown.source == source {
            return Ok(false);
        }
        let rendered = render_markdown(&source)?;
        markdown.source = source;
        markdown.rendered = rendered;
        let reference_count = markdown.rendered.references.len();
        self.selected_reference = self
            .selected_reference
            .filter(|number| *number > 0 && *number <= reference_count);
        Ok(true)
    }

    fn rendered(&self) -> Option<&RenderedMarkdown> {
        match &self.mode {
            RichContentMode::Markdown(markdown) => Some(&markdown.rendered),
            RichContentMode::Image { .. } => None,
        }
    }

    pub(crate) fn reference(&self, number: usize) -> Option<RichReference> {
        self.rendered()?
            .references
            .iter()
            .find(|reference| reference.number == number)
            .cloned()
    }

    fn step_reference(&mut self, backwards: bool) {
        let count = self
            .rendered()
            .map_or(0, |rendered| rendered.references.len());
        if count == 0 {
            self.selected_reference = None;
            return;
        }
        self.selected_reference = Some(match (self.selected_reference, backwards) {
            (Some(1), true) | (None, true) => count,
            (Some(number), true) => number.saturating_sub(1).max(1),
            (Some(number), false) if number >= count => 1,
            (Some(number), false) => number.saturating_add(1),
            (None, false) => 1,
        });
    }

    pub(crate) fn handle_key(&mut self, event: &KeyEvent, page_rows: usize) -> RichContentInput {
        if event.kind == KeyEventKind::Release {
            return RichContentInput::Consumed;
        }
        if event.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && matches!(event.code, KeyCode::Char('v' | 'V'))
        {
            return if self.is_direct_image() {
                RichContentInput::Consumed
            } else {
                RichContentInput::ClosePreview
            };
        }
        if event.modifiers != KeyModifiers::NONE
            && !(event.code == KeyCode::BackTab && event.modifiers == KeyModifiers::SHIFT)
        {
            return RichContentInput::Unhandled;
        }
        match event.code {
            KeyCode::Esc => match &self.mode {
                RichContentMode::Markdown(_) => RichContentInput::ClosePreview,
                RichContentMode::Image { back: Some(_), .. } => RichContentInput::Back,
                RichContentMode::Image { back: None, .. } => RichContentInput::Consumed,
            },
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                RichContentInput::Consumed
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                RichContentInput::Consumed
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(page_rows.max(1));
                RichContentInput::Consumed
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(page_rows.max(1));
                RichContentInput::Consumed
            }
            KeyCode::Home => {
                self.scroll = 0;
                RichContentInput::Consumed
            }
            KeyCode::End => {
                self.scroll = usize::MAX;
                RichContentInput::Consumed
            }
            KeyCode::Tab => {
                self.step_reference(false);
                RichContentInput::Consumed
            }
            KeyCode::BackTab => {
                self.step_reference(true);
                RichContentInput::Consumed
            }
            KeyCode::Enter => self
                .selected_reference
                .and_then(|number| self.reference(number))
                .map_or(RichContentInput::Consumed, RichContentInput::Activate),
            _ => RichContentInput::Unhandled,
        }
    }

    pub(crate) fn handle_scroll(&mut self, down: bool, rows: usize) {
        if down {
            self.scroll = self.scroll.saturating_add(rows.max(1));
        } else {
            self.scroll = self.scroll.saturating_sub(rows.max(1));
        }
    }

    pub(crate) fn back(&mut self) -> bool {
        let RichContentMode::Image { back, .. } = &mut self.mode else {
            return false;
        };
        let Some(markdown) = back.take() else {
            return false;
        };
        self.mode = RichContentMode::Markdown(*markdown);
        self.scroll = 0;
        self.selected_reference = None;
        true
    }

    fn image_lines(
        image: &RichImagePresentation,
        protocol: TerminalImageProtocol,
    ) -> Vec<RichLine> {
        let rows = [
            format!("Image: {}", image.label),
            format!("Target: {}", image.target),
            format!(
                "{} · {}×{} · {} bytes",
                image.format, image.width, image.height, image.file_size
            ),
            if protocol == TerminalImageProtocol::None {
                "Inline image unavailable; metadata fallback is active.".to_owned()
            } else {
                format!("Inline image protocol: {}", protocol.label())
            },
            String::new(),
        ];
        rows.into_iter()
            .map(|text| RichLine {
                spans: (!text.is_empty())
                    .then(|| RichSpan {
                        text,
                        style: Style::default(),
                    })
                    .into_iter()
                    .collect(),
                references: Vec::new(),
            })
            .collect()
    }

    pub(crate) fn snapshot(
        &mut self,
        title: String,
        width: usize,
        body_rows: usize,
        protocol: TerminalImageProtocol,
    ) -> RichContentSnapshot {
        let lines = match &self.mode {
            RichContentMode::Markdown(markdown) => wrap_markdown(&markdown.rendered, width),
            RichContentMode::Image { image, .. } => Self::image_lines(image, protocol),
        };
        let max_scroll = lines.len().saturating_sub(body_rows.max(1));
        self.scroll = self.scroll.min(max_scroll);
        if let Some(selected) = self.selected_reference
            && let Some(row) = lines
                .iter()
                .position(|line| line.references.contains(&selected))
        {
            if row < self.scroll {
                self.scroll = row;
            } else if row >= self.scroll.saturating_add(body_rows.max(1)) {
                self.scroll = row.saturating_sub(body_rows.saturating_sub(1));
            }
        }
        let status = match &self.mode {
            RichContentMode::Markdown(markdown) => format!(
                "Markdown preview · {}/{} · Tab links · Enter open · ↑/↓/Pg scroll · Esc source{}",
                self.scroll.saturating_add(1).min(lines.len().max(1)),
                lines.len(),
                if markdown.rendered.truncated {
                    " · truncated"
                } else {
                    ""
                }
            ),
            RichContentMode::Image { back, .. } => format!(
                "Image preview · {} · {}",
                protocol.label(),
                if back.is_some() {
                    "Esc Markdown"
                } else {
                    "Ctrl-W close"
                }
            ),
        };
        RichContentSnapshot {
            title,
            lines,
            scroll: self.scroll,
            selected_reference: self.selected_reference,
            status,
        }
    }

    pub(crate) fn graphic(
        &self,
        area: Rect,
        protocol: TerminalImageProtocol,
    ) -> Option<(Arc<[u8]>, GraphicPlacement)> {
        let RichContentMode::Image { image, .. } = &self.mode else {
            return None;
        };
        if protocol == TerminalImageProtocol::None || area.width < 4 || area.height < 8 {
            return None;
        }
        Some((
            image.bytes.clone(),
            GraphicPlacement {
                row: area.y.saturating_add(5),
                column: area.x.saturating_add(1),
                width_cells: area.width.saturating_sub(2),
                height_cells: area.height.saturating_sub(7),
                image_id: 1,
            },
        ))
    }
}

impl<'a> RichContentWidget<'a> {
    pub(crate) fn new(snapshot: &'a RichContentSnapshot) -> Self {
        Self { snapshot }
    }
}

impl Widget for RichContentWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let body_height = usize::from(area.height.saturating_sub(1));
        for (screen_row, line) in self
            .snapshot
            .lines
            .iter()
            .skip(self.snapshot.scroll)
            .take(body_height)
            .enumerate()
        {
            let selected = self
                .snapshot
                .selected_reference
                .is_some_and(|selected| line.references.contains(&selected));
            let spans = line
                .spans
                .iter()
                .map(|span| {
                    Span::styled(
                        span.text.clone(),
                        if selected {
                            span.style.add_modifier(Modifier::REVERSED)
                        } else {
                            span.style
                        },
                    )
                })
                .collect::<Vec<_>>();
            Paragraph::new(Line::from(spans)).render(
                Rect::new(
                    area.x,
                    area.y.saturating_add(screen_row as u16),
                    area.width,
                    1,
                ),
                buffer,
            );
        }
        if area.height > 0 {
            let status_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
            let status = format!("{}  {}", self.snapshot.title, self.snapshot.status);
            Paragraph::new(status)
                .style(Style::default().add_modifier(Modifier::REVERSED))
                .render(status_area, buffer);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GraphicPlacement {
    pub(crate) row: u16,
    pub(crate) column: u16,
    pub(crate) width_cells: u16,
    pub(crate) height_cells: u16,
    pub(crate) image_id: u32,
}

fn checked_image(bytes: &[u8]) -> Result<(DynamicImage, ImageFormat)> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_INLINE_IMAGE_BYTES,
        "image size is outside 1..={MAX_INLINE_IMAGE_BYTES} bytes"
    );
    let format = image::guess_format(bytes).context("detect image format")?;
    let image = image::load_from_memory_with_format(bytes, format).context("decode image")?;
    let pixels = u64::from(image.width()).saturating_mul(u64::from(image.height()));
    ensure!(
        pixels > 0 && pixels <= MAX_DECODED_IMAGE_PIXELS,
        "decoded image has {pixels} pixels; limit is {MAX_DECODED_IMAGE_PIXELS}"
    );
    Ok((image, format))
}

fn png_bytes(image: &DynamicImage) -> Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, ImageFormat::Png)
        .context("encode terminal PNG")?;
    Ok(output.into_inner())
}

fn kitty_graphic(bytes: &[u8], placement: GraphicPlacement) -> Result<Vec<u8>> {
    let (image, _) = checked_image(bytes)?;
    let payload = BASE64.encode(png_bytes(&image)?);
    let mut output = Vec::with_capacity(payload.len().saturating_add(256));
    let chunks = payload.as_bytes().chunks(KITTY_CHUNK_BYTES).peekable();
    for (index, chunk) in chunks.enumerate() {
        let more = usize::from((index + 1).saturating_mul(KITTY_CHUNK_BYTES) < payload.len());
        if index == 0 {
            write!(
                output,
                "\x1b_Ga=T,f=100,t=d,i={},c={},r={},q=2,m={more};",
                placement.image_id,
                placement.width_cells.max(1),
                placement.height_cells.max(1),
            )?;
        } else {
            write!(output, "\x1b_Gm={more};")?;
        }
        output.extend_from_slice(chunk);
        output.extend_from_slice(b"\x1b\\");
    }
    ensure!(
        output.len() <= MAX_GRAPHIC_OUTPUT_BYTES,
        "Kitty image sequence exceeds {MAX_GRAPHIC_OUTPUT_BYTES} bytes"
    );
    Ok(output)
}

fn iterm2_graphic(bytes: &[u8], placement: GraphicPlacement) -> Result<Vec<u8>> {
    let _ = checked_image(bytes)?;
    let payload = BASE64.encode(bytes);
    let sequence = format!(
        "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1:{}\x07",
        placement.width_cells.max(1),
        placement.height_cells.max(1),
        payload
    )
    .into_bytes();
    ensure!(
        sequence.len() <= MAX_GRAPHIC_OUTPUT_BYTES,
        "iTerm2 image sequence exceeds {MAX_GRAPHIC_OUTPUT_BYTES} bytes"
    );
    Ok(sequence)
}

fn sixel_color(pixel: image::Rgba<u8>) -> Option<u8> {
    if pixel[3] < 128 {
        return None;
    }
    let r = pixel[0] / 51;
    let g = pixel[1] / 51;
    let b = pixel[2] / 51;
    Some(
        r.saturating_mul(36)
            .saturating_add(g.saturating_mul(6))
            .saturating_add(b),
    )
}

fn sixel_run(output: &mut Vec<u8>, value: u8, count: usize) -> io::Result<()> {
    if count >= 4 {
        write!(output, "!{count}{}", char::from(value))
    } else {
        output.extend(std::iter::repeat_n(value, count));
        Ok(())
    }
}

fn sixel_graphic(bytes: &[u8], placement: GraphicPlacement) -> Result<Vec<u8>> {
    let (image, _) = checked_image(bytes)?;
    let max_width = u32::from(placement.width_cells.max(1)).saturating_mul(8);
    let max_height = u32::from(placement.height_cells.max(1)).saturating_mul(16);
    let image = image
        .thumbnail(max_width.max(1), max_height.max(1))
        .to_rgba8();
    let (width, height) = image.dimensions();
    let mut palette = BTreeSet::new();
    for pixel in image.pixels() {
        if let Some(color) = sixel_color(*pixel) {
            palette.insert(color);
        }
    }
    let mut output = Vec::new();
    output.extend_from_slice(b"\x1bPq");
    write!(output, "\"1;1;{width};{height}")?;
    for color in &palette {
        let r = u16::from(*color / 36) * 20;
        let g = u16::from((*color / 6) % 6) * 20;
        let b = u16::from(*color % 6) * 20;
        write!(output, "#{color};2;{r};{g};{b}")?;
    }
    for band_y in (0..height).step_by(6) {
        let mut first_color = true;
        for color in &palette {
            let mut values = Vec::with_capacity(width as usize);
            for x in 0..width {
                let mut bits = 0u8;
                for bit in 0..6u32 {
                    let y = band_y.saturating_add(bit);
                    if y < height && sixel_color(*image.get_pixel(x, y)) == Some(*color) {
                        bits |= 1 << bit;
                    }
                }
                values.push(63u8.saturating_add(bits));
            }
            while values.last() == Some(&63) {
                values.pop();
            }
            if values.is_empty() {
                continue;
            }
            if !first_color {
                output.push(b'$');
            }
            first_color = false;
            write!(output, "#{color}")?;
            let mut index = 0usize;
            while index < values.len() {
                let value = values[index];
                let mut end = index.saturating_add(1);
                while end < values.len() && values[end] == value {
                    end += 1;
                }
                sixel_run(&mut output, value, end - index)?;
                index = end;
            }
        }
        if band_y.saturating_add(6) < height {
            output.push(b'-');
        }
        ensure!(
            output.len() <= MAX_GRAPHIC_OUTPUT_BYTES,
            "Sixel image sequence exceeds {MAX_GRAPHIC_OUTPUT_BYTES} bytes"
        );
    }
    output.extend_from_slice(b"\x1b\\");
    Ok(output)
}

pub(crate) fn terminal_graphic(
    protocol: TerminalImageProtocol,
    bytes: &[u8],
    placement: GraphicPlacement,
) -> Result<Option<Vec<u8>>> {
    let body = match protocol {
        TerminalImageProtocol::Kitty => kitty_graphic(bytes, placement)?,
        TerminalImageProtocol::Iterm2 => iterm2_graphic(bytes, placement)?,
        TerminalImageProtocol::Sixel => sixel_graphic(bytes, placement)?,
        TerminalImageProtocol::None => return Ok(None),
    };
    let mut output = Vec::with_capacity(body.len().saturating_add(32));
    output.extend_from_slice(b"\x1b7");
    write!(
        output,
        "\x1b[{};{}H",
        placement.row.saturating_add(1),
        placement.column.saturating_add(1)
    )?;
    output.extend_from_slice(&body);
    output.extend_from_slice(b"\x1b8");
    Ok(Some(output))
}

pub(crate) fn clear_terminal_graphics(
    writer: &mut impl io::Write,
    protocol: TerminalImageProtocol,
) -> io::Result<()> {
    match protocol {
        TerminalImageProtocol::Kitty => writer.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?,
        // iTerm2 and Sixel have no portable per-image deletion primitive.
        // Erase the alternate screen and let the caller issue a full Ratatui
        // redraw so pixels cannot remain over a Markdown/editor frame.
        TerminalImageProtocol::Iterm2 | TerminalImageProtocol::Sixel => {
            writer.write_all(b"\x1b[2J\x1b[H")?
        }
        TerminalImageProtocol::None => return Ok(()),
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView as _, ImageBuffer, Rgba};

    fn tiny_png() -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_fn(2, 2, |x, y| {
            if (x + y) % 2 == 0 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        }));
        png_bytes(&image).unwrap()
    }

    #[test]
    fn markdown_uses_zed_options_and_preserves_terminal_semantics() {
        let source = "# Title\n\n- [x] **done**\n- [ ] [link](https://example.com)\n\n![alt](image.png)\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let rendered = render_markdown(source).unwrap();
        let text = rendered
            .lines
            .iter()
            .map(RichLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("# Title"));
        assert!(text.contains("• [x] done"));
        assert!(text.contains("link[1]"));
        assert!(text.contains("[image 2: alt]"));
        assert!(text.contains("│ a │ b │"));
        assert_eq!(rendered.references.len(), 2);
        assert_eq!(rendered.references[0].kind, RichReferenceKind::Link);
        assert_eq!(rendered.references[1].kind, RichReferenceKind::Image);
    }

    #[test]
    fn markdown_wrap_is_cell_bounded_and_keeps_reference_selection() {
        let rendered = render_markdown("[wide界界](https://example.com)").unwrap();
        let wrapped = wrap_markdown(&rendered, 5);
        assert!(wrapped.len() >= 2);
        assert!(wrapped.iter().all(|line| {
            line.text()
                .chars()
                .map(|character| character.width().unwrap_or_default())
                .sum::<usize>()
                <= 5
        }));
        assert!(wrapped.iter().any(|line| line.references.contains(&1)));
    }

    #[test]
    fn all_terminal_image_protocols_are_bounded_and_well_formed() {
        let bytes = tiny_png();
        let placement = GraphicPlacement {
            row: 2,
            column: 3,
            width_cells: 10,
            height_cells: 5,
            image_id: 7,
        };
        let kitty = terminal_graphic(TerminalImageProtocol::Kitty, &bytes, placement)
            .unwrap()
            .unwrap();
        assert!(kitty.starts_with(b"\x1b7\x1b[3;4H\x1b_G"));
        assert!(kitty.windows(4).any(|window| window == b"f=10"));
        assert!(kitty.ends_with(b"\x1b8"));

        let iterm = terminal_graphic(TerminalImageProtocol::Iterm2, &bytes, placement)
            .unwrap()
            .unwrap();
        assert!(iterm.windows(15).any(|window| window == b"]1337;File=inli"));

        let sixel = terminal_graphic(TerminalImageProtocol::Sixel, &bytes, placement)
            .unwrap()
            .unwrap();
        assert!(sixel.windows(3).any(|window| window == b"\x1bPq"));
        assert!(sixel.windows(2).any(|window| window == b"\x1b\\"));
        assert_eq!(
            terminal_graphic(TerminalImageProtocol::None, &bytes, placement).unwrap(),
            None
        );
    }

    #[test]
    fn terminal_graphics_have_a_protocol_appropriate_clear_sequence() {
        let mut kitty = Vec::new();
        clear_terminal_graphics(&mut kitty, TerminalImageProtocol::Kitty).unwrap();
        assert_eq!(kitty, b"\x1b_Ga=d,d=A,q=2\x1b\\");

        for protocol in [TerminalImageProtocol::Iterm2, TerminalImageProtocol::Sixel] {
            let mut output = Vec::new();
            clear_terminal_graphics(&mut output, protocol).unwrap();
            assert_eq!(output, b"\x1b[2J\x1b[H");
        }

        let mut none = Vec::new();
        clear_terminal_graphics(&mut none, TerminalImageProtocol::None).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn image_decoder_rejects_empty_and_pixel_bomb_dimensions() {
        assert!(checked_image(&[]).is_err());
        let bytes = tiny_png();
        let (image, format) = checked_image(&bytes).unwrap();
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(format, ImageFormat::Png);
    }

    #[test]
    fn preview_input_selects_activates_and_returns_from_images() {
        let mut preview =
            RichContentState::markdown("[docs](guide.md#install) ![logo](logo.png)".to_owned())
                .unwrap();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(preview.handle_key(&tab, 10), RichContentInput::Consumed);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let RichContentInput::Activate(link) = preview.handle_key(&enter, 10) else {
            panic!("selected link did not activate");
        };
        assert_eq!(link.kind, RichReferenceKind::Link);
        assert_eq!(link.target, "guide.md#install");

        assert_eq!(preview.handle_key(&tab, 10), RichContentInput::Consumed);
        let RichContentInput::Activate(image_reference) = preview.handle_key(&enter, 10) else {
            panic!("selected image did not activate");
        };
        assert_eq!(image_reference.kind, RichReferenceKind::Image);

        let bytes: Arc<[u8]> = Arc::from(tiny_png());
        preview.show_image(RichImagePresentation {
            label: "logo".to_owned(),
            target: "logo.png".to_owned(),
            format: "PNG".to_owned(),
            width: 2,
            height: 2,
            file_size: bytes.len() as u64,
            bytes,
        });
        assert!(preview.is_image());
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(preview.handle_key(&escape, 10), RichContentInput::Back);
        assert!(preview.back());
        assert!(!preview.is_image());
    }

    #[test]
    fn live_markdown_refresh_preserves_valid_selection_and_clamps_scroll() {
        let mut preview = RichContentState::markdown("[one](one.md)\n\nold".to_owned()).unwrap();
        assert_eq!(
            preview.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 4),
            RichContentInput::Consumed
        );
        preview.handle_scroll(true, usize::MAX);
        assert!(
            preview
                .refresh_markdown("[one](one.md)\n\nnew content".to_owned())
                .unwrap()
        );
        let snapshot = preview.snapshot("README.md".to_owned(), 40, 4, TerminalImageProtocol::None);
        assert_eq!(snapshot.selected_reference, Some(1));
        assert!(
            snapshot
                .lines
                .iter()
                .any(|line| line.text().contains("new content"))
        );
        assert!(snapshot.scroll < snapshot.lines.len().max(1));
        assert!(
            !preview
                .refresh_markdown("[one](one.md)\n\nnew content".to_owned())
                .unwrap()
        );
    }
}
