//! Markdown → block tree. Parsed once per load; the renderer walks the tree every frame.

use std::collections::HashMap;
use std::sync::Arc;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub sup: bool,
    pub sub: bool,
    pub math: bool,
    pub footnote: bool,
    pub dim: bool,
    /// Index into the owning `Inline::links`.
    pub link: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Clone, Debug)]
pub struct Link {
    pub url: String,
    pub title: String,
}

/// A run of styled text: a paragraph, heading, table cell, ...
#[derive(Clone, Debug, Default)]
pub struct Inline {
    /// Unique per document; used to map search hits onto leaves.
    pub id: usize,
    pub spans: Vec<Span>,
    pub links: Vec<Link>,
    /// Concatenation of all span texts (what search operates on).
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ImageRef {
    pub url: String,
    pub alt: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub enum InlineItem {
    Text(Inline),
    Image(ImageRef),
}

#[derive(Clone, Debug)]
pub struct CodeBlock {
    pub id: usize,
    pub lang: Option<String>,
    pub text: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
    /// Tight list item (no blank lines) → smaller spacing.
    pub tight: bool,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub aligns: Vec<Alignment>,
    pub head: Vec<Inline>,
    pub rows: Vec<Vec<Inline>>,
}

#[derive(Clone, Debug)]
pub struct Footnote {
    pub number: usize,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

#[derive(Clone, Debug)]
pub enum Block {
    Heading { level: u8, inline: Inline },
    Paragraph(Vec<InlineItem>),
    Code(CodeBlock),
    Quote { kind: Option<QuoteKind>, blocks: Vec<Block> },
    List { start: Option<u64>, items: Vec<ListItem> },
    Table(Table),
    Rule,
    Image(ImageRef),
    Math(String),
    Metadata(String),
    DefList(Vec<(Inline, Vec<Block>)>),
    Footnotes(Vec<Footnote>),
}

#[derive(Clone, Debug)]
pub struct HeadingRef {
    pub level: u8,
    pub text: String,
    pub slug: String,
    /// Index of the top-level block that contains this heading.
    pub block: usize,
    /// Heading lives inside a list/quote; excluded from the outline.
    pub nested: bool,
}

#[derive(Debug, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
    pub headings: Vec<HeadingRef>,
    pub title: Option<String>,
    pub word_count: usize,
}

impl Document {
    pub fn parse(src: &str) -> Document {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_FOOTNOTES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_SMART_PUNCTUATION);
        opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
        opts.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
        opts.insert(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS);
        opts.insert(Options::ENABLE_MATH);
        opts.insert(Options::ENABLE_GFM);
        opts.insert(Options::ENABLE_DEFINITION_LIST);
        opts.insert(Options::ENABLE_SUPERSCRIPT);
        opts.insert(Options::ENABLE_SUBSCRIPT);

        // Callers strip byte order marks already (text::decode); this is a safety net.
        let src = src.strip_prefix('\u{FEFF}').unwrap_or(src);
        let mut b = Builder::default();
        for ev in Parser::new_ext(src, opts) {
            b.event(ev);
        }
        b.finish()
    }

    pub fn find_slug(&self, slug: &str) -> Option<usize> {
        let slug = slug.trim_start_matches('#');
        self.headings
            .iter()
            .find(|h| h.slug == slug)
            .or_else(|| {
                let lower = slug.to_lowercase();
                self.headings
                    .iter()
                    .find(|h| h.slug.to_lowercase() == lower || h.text.to_lowercase() == lower)
            })
            .map(|h| h.block)
    }

    /// Visit every searchable leaf: `(leaf id, top-level block index, text)`.
    pub fn for_each_leaf(&self, f: &mut dyn FnMut(usize, usize, &str)) {
        for (i, b) in self.blocks.iter().enumerate() {
            walk_leaves(b, i, f);
        }
    }
}

fn walk_leaves(b: &Block, top: usize, f: &mut dyn FnMut(usize, usize, &str)) {
    match b {
        Block::Heading { inline, .. } => f(inline.id, top, &inline.text),
        Block::Paragraph(items) => {
            for it in items {
                if let InlineItem::Text(t) = it {
                    f(t.id, top, &t.text);
                }
            }
        }
        Block::Code(c) => f(c.id, top, &c.text),
        Block::Quote { blocks, .. } => blocks.iter().for_each(|b| walk_leaves(b, top, f)),
        Block::List { items, .. } => {
            for it in items {
                it.blocks.iter().for_each(|b| walk_leaves(b, top, f));
            }
        }
        Block::Table(t) => {
            for c in &t.head {
                f(c.id, top, &c.text);
            }
            for r in &t.rows {
                for c in r {
                    f(c.id, top, &c.text);
                }
            }
        }
        Block::DefList(entries) => {
            for (term, defs) in entries {
                f(term.id, top, &term.text);
                defs.iter().for_each(|b| walk_leaves(b, top, f));
            }
        }
        Block::Footnotes(fns) => {
            for fnote in fns {
                fnote.blocks.iter().for_each(|b| walk_leaves(b, top, f));
            }
        }
        Block::Rule | Block::Image(_) | Block::Math(_) | Block::Metadata(_) => {}
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

enum Frame {
    Quote { kind: Option<QuoteKind>, blocks: Vec<Block> },
    List { start: Option<u64>, items: Vec<ListItem> },
    Item { checked: Option<bool>, blocks: Vec<Block>, tight: bool },
    Footnote { label: String, blocks: Vec<Block> },
    Table { aligns: Vec<Alignment>, head: Vec<Inline>, rows: Vec<Vec<Inline>>, cur: Vec<Inline>, in_head: bool },
    DefList { entries: Vec<(Inline, Vec<Block>)> },
    DefDef { blocks: Vec<Block> },
    Html { raw: String },
    Metadata { raw: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccKind {
    Paragraph,
    Heading(u8),
    Cell,
    DefTitle,
}

struct Acc {
    kind: AccKind,
    heading_id: Option<String>,
    items: Vec<InlineItem>,
    spans: Vec<Span>,
    links: Vec<Link>,
    /// Started by bare text inside a tight list item (no explicit Paragraph tag).
    implicit: bool,
}

#[derive(Default)]
struct Builder {
    root: Vec<Block>,
    stack: Vec<Frame>,
    acc: Option<Acc>,
    bold: u32,
    italic: u32,
    strike: u32,
    sup: u32,
    sub: u32,
    link_stack: Vec<u32>,
    image: Option<ImageRef>,
    code: Option<(Option<String>, String)>,
    next_leaf: usize,
    headings: Vec<HeadingRef>,
    slugs: HashMap<String, usize>,
    footnote_numbers: HashMap<String, usize>,
    footnotes: Vec<Footnote>,
    word_count: usize,
}

impl Builder {
    fn leaf_id(&mut self) -> usize {
        let id = self.next_leaf;
        self.next_leaf += 1;
        id
    }

    fn style(&self) -> SpanStyle {
        SpanStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            sup: self.sup > 0,
            sub: self.sub > 0,
            link: self.link_stack.last().copied(),
            ..Default::default()
        }
    }

    fn nested(&self) -> bool {
        !self.stack.is_empty()
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Frame::Quote { blocks, .. })
            | Some(Frame::Item { blocks, .. })
            | Some(Frame::Footnote { blocks, .. })
            | Some(Frame::DefDef { blocks }) => blocks.push(block),
            Some(Frame::List { items, .. }) => {
                // Block directly inside a list (malformed); attach to the last item.
                if let Some(it) = items.last_mut() {
                    it.blocks.push(block);
                } else {
                    items.push(ListItem { checked: None, blocks: vec![block], tight: false });
                }
            }
            Some(Frame::DefList { entries }) => {
                if let Some(e) = entries.last_mut() {
                    e.1.push(block);
                } else {
                    entries.push((Inline::default(), vec![block]));
                }
            }
            Some(Frame::Table { .. }) | Some(Frame::Html { .. }) | Some(Frame::Metadata { .. }) => {
                // Should not happen; drop to root to avoid losing content.
                self.root.push(block);
            }
            None => self.root.push(block),
        }
    }

    fn ensure_acc(&mut self, kind: AccKind, implicit: bool) {
        if self.acc.is_none() {
            self.acc = Some(Acc {
                kind,
                heading_id: None,
                items: Vec::new(),
                spans: Vec::new(),
                links: Vec::new(),
                implicit,
            });
        }
    }

    fn push_span(&mut self, text: &str, style: SpanStyle) {
        if text.is_empty() {
            return;
        }
        self.ensure_acc(AccKind::Paragraph, true);
        let acc = self.acc.as_mut().unwrap();
        if let Some(last) = acc.spans.last_mut() {
            if last.style == style {
                last.text.push_str(text);
                return;
            }
        }
        acc.spans.push(Span { text: text.to_owned(), style });
    }

    fn text(&mut self, s: &str) {
        if let Some(img) = &mut self.image {
            img.alt.push_str(s);
            return;
        }
        if let Some((_, code)) = &mut self.code {
            code.push_str(s);
            return;
        }
        if let Some(Frame::Metadata { raw }) = self.stack.last_mut() {
            raw.push_str(s);
            return;
        }
        if let Some(Frame::Html { raw }) = self.stack.last_mut() {
            raw.push_str(s);
            return;
        }
        self.word_count += s.split_whitespace().count();
        let st = self.style();
        self.push_span(s, st);
    }

    fn make_inline(&mut self, spans: Vec<Span>, links: Vec<Link>) -> Inline {
        let mut text = String::new();
        for s in &spans {
            text.push_str(&s.text);
        }
        Inline { id: self.leaf_id(), spans, links, text }
    }

    fn flush_spans_to_item(&mut self) {
        let Some(acc) = self.acc.as_mut() else { return };
        if acc.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut acc.spans);
        let links = acc.links.clone();
        let inline = self.make_inline(spans, links);
        self.acc.as_mut().unwrap().items.push(InlineItem::Text(inline));
    }

    fn flush_implicit(&mut self) {
        if self.acc.as_ref().is_some_and(|a| a.implicit) {
            if let Some(Frame::Item { tight, .. }) = self.stack.last_mut() {
                *tight = true;
            }
            self.flush_acc();
        }
    }

    fn flush_acc(&mut self) {
        self.flush_spans_to_item();
        let Some(acc) = self.acc.take() else { return };
        match acc.kind {
            AccKind::Paragraph => {
                let items = acc.items;
                if items.is_empty() {
                    return;
                }
                let only_images = items.iter().all(|it| match it {
                    InlineItem::Image(_) => true,
                    InlineItem::Text(t) => t.text.trim().is_empty(),
                });
                if only_images {
                    let imgs: Vec<ImageRef> = items
                        .into_iter()
                        .filter_map(|it| match it {
                            InlineItem::Image(i) => Some(i),
                            _ => None,
                        })
                        .collect();
                    if imgs.len() == 1 {
                        let img = imgs.into_iter().next().unwrap();
                        self.push_block(Block::Image(img));
                    } else {
                        self.push_block(Block::Paragraph(imgs.into_iter().map(InlineItem::Image).collect()));
                    }
                } else {
                    self.push_block(Block::Paragraph(items));
                }
            }
            AccKind::Heading(level) => {
                let inline = merge_items(acc.items);
                let text = inline.text.trim().to_owned();
                let base = acc.heading_id.unwrap_or_else(|| slugify(&text));
                let slug = self.unique_slug(base);
                let nested = self.nested();
                // Index of the top-level block this heading will end up in.
                let block = self.root.len();
                self.headings.push(HeadingRef { level, text, slug, block, nested });
                self.push_block(Block::Heading { level, inline });
            }
            AccKind::Cell => {
                let inline = merge_items(acc.items);
                if let Some(Frame::Table { cur, .. }) = self.stack.last_mut() {
                    cur.push(inline);
                }
            }
            AccKind::DefTitle => {
                let inline = merge_items(acc.items);
                if let Some(Frame::DefList { entries }) = self.stack.last_mut() {
                    entries.push((inline, Vec::new()));
                }
            }
        }
    }

    fn unique_slug(&mut self, base: String) -> String {
        let n = self.slugs.entry(base.clone()).or_insert(0);
        let slug = if *n == 0 { base.clone() } else { format!("{base}-{n}") };
        *n += 1;
        slug
    }

    fn footnote_number(&mut self, label: &str) -> usize {
        let next = self.footnote_numbers.len() + 1;
        *self.footnote_numbers.entry(label.to_owned()).or_insert(next)
    }

    fn event(&mut self, ev: Event<'_>) {
        use Event::*;
        match ev {
            Start(tag) => self.start(tag),
            End(tag) => self.end(tag),
            Text(t) => self.text(&t),
            Code(c) => {
                self.ensure_acc(AccKind::Paragraph, true);
                let mut st = self.style();
                st.code = true;
                self.push_span(&c, st);
            }
            InlineMath(m) => {
                self.ensure_acc(AccKind::Paragraph, true);
                let mut st = self.style();
                st.math = true;
                self.push_span(&m, st);
            }
            DisplayMath(m) => {
                self.flush_implicit();
                self.push_block(Block::Math(m.trim().to_owned()));
            }
            Html(h) | InlineHtml(h) => {
                if let Some(Frame::Html { raw }) = self.stack.last_mut() {
                    raw.push_str(&h);
                } else {
                    self.inline_html(&h);
                }
            }
            FootnoteReference(label) => {
                let n = self.footnote_number(&label);
                self.ensure_acc(AccKind::Paragraph, true);
                let mut st = self.style();
                st.footnote = true;
                self.push_span(&format!("[{n}]"), st);
            }
            SoftBreak => {
                let st = self.style();
                self.push_span(" ", st);
            }
            HardBreak => {
                let st = self.style();
                self.push_span("\n", st);
            }
            Rule => {
                self.flush_implicit();
                self.push_block(Block::Rule);
            }
            TaskListMarker(checked) => {
                if let Some(Frame::Item { checked: c, .. }) = self.stack.last_mut() {
                    *c = Some(checked);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_implicit();
                self.ensure_acc(AccKind::Paragraph, false);
            }
            Tag::Heading { level, id, .. } => {
                self.flush_implicit();
                self.ensure_acc(AccKind::Heading(heading_level(level)), false);
                if let Some(acc) = self.acc.as_mut() {
                    acc.heading_id = id.map(|s| s.to_string());
                }
            }
            Tag::BlockQuote(kind) => {
                self.flush_implicit();
                let kind = kind.map(|k| match k {
                    BlockQuoteKind::Note => QuoteKind::Note,
                    BlockQuoteKind::Tip => QuoteKind::Tip,
                    BlockQuoteKind::Important => QuoteKind::Important,
                    BlockQuoteKind::Warning => QuoteKind::Warning,
                    BlockQuoteKind::Caution => QuoteKind::Caution,
                });
                self.stack.push(Frame::Quote { kind, blocks: Vec::new() });
            }
            Tag::CodeBlock(kind) => {
                self.flush_implicit();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let l = info.trim();
                        if l.is_empty() {
                            None
                        } else {
                            Some(l.to_owned())
                        }
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush_implicit();
                self.stack.push(Frame::Html { raw: String::new() });
            }
            Tag::List(start) => {
                self.flush_implicit();
                self.stack.push(Frame::List { start, items: Vec::new() });
            }
            Tag::Item => {
                self.stack.push(Frame::Item { checked: None, blocks: Vec::new(), tight: false });
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_implicit();
                self.stack.push(Frame::Footnote { label: label.to_string(), blocks: Vec::new() });
            }
            Tag::DefinitionList => {
                self.flush_implicit();
                self.stack.push(Frame::DefList { entries: Vec::new() });
            }
            Tag::DefinitionListTitle => {
                self.flush_implicit();
                self.ensure_acc(AccKind::DefTitle, false);
            }
            Tag::DefinitionListDefinition => {
                self.flush_implicit();
                self.stack.push(Frame::DefDef { blocks: Vec::new() });
            }
            Tag::Table(aligns) => {
                self.flush_implicit();
                self.stack.push(Frame::Table {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    cur: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(Frame::Table { in_head, cur, .. }) = self.stack.last_mut() {
                    *in_head = true;
                    cur.clear();
                }
            }
            Tag::TableRow => {
                if let Some(Frame::Table { cur, .. }) = self.stack.last_mut() {
                    cur.clear();
                }
            }
            Tag::TableCell => {
                self.ensure_acc(AccKind::Cell, false);
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Superscript => self.sup += 1,
            Tag::Subscript => self.sub += 1,
            Tag::Link { dest_url, title, .. } => {
                self.ensure_acc(AccKind::Paragraph, true);
                let acc = self.acc.as_mut().unwrap();
                let idx = acc.links.len() as u32;
                acc.links.push(Link { url: dest_url.to_string(), title: title.to_string() });
                self.link_stack.push(idx);
            }
            Tag::Image { dest_url, title, .. } => {
                self.image = Some(ImageRef {
                    url: dest_url.to_string(),
                    alt: String::new(),
                    title: title.to_string(),
                });
            }
            Tag::MetadataBlock(_) => {
                self.flush_implicit();
                self.stack.push(Frame::Metadata { raw: String::new() });
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::TableCell | TagEnd::DefinitionListTitle => {
                self.flush_acc();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_implicit();
                if let Some(Frame::Quote { kind, blocks }) = self.stack.pop() {
                    self.push_block(Block::Quote { kind, blocks });
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    let id = self.leaf_id();
                    self.push_block(Block::Code(CodeBlock { id, lang, text: Arc::from(text) }));
                }
            }
            TagEnd::HtmlBlock => {
                if let Some(Frame::Html { raw }) = self.stack.pop() {
                    self.html_block(&raw);
                }
            }
            TagEnd::List(_) => {
                self.flush_implicit();
                if let Some(Frame::List { start, items }) = self.stack.pop() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush_implicit();
                if let Some(Frame::Item { checked, blocks, tight }) = self.stack.pop() {
                    if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                        items.push(ListItem { checked, blocks, tight });
                    }
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_implicit();
                if let Some(Frame::Footnote { label, blocks }) = self.stack.pop() {
                    let number = self.footnote_number(&label);
                    self.footnotes.push(Footnote { number, blocks });
                }
            }
            TagEnd::DefinitionList => {
                self.flush_implicit();
                if let Some(Frame::DefList { entries }) = self.stack.pop() {
                    self.push_block(Block::DefList(entries));
                }
            }
            TagEnd::DefinitionListDefinition => {
                self.flush_implicit();
                if let Some(Frame::DefDef { blocks }) = self.stack.pop() {
                    if let Some(Frame::DefList { entries }) = self.stack.last_mut() {
                        if let Some(e) = entries.last_mut() {
                            e.1.extend(blocks);
                        } else {
                            entries.push((Inline::default(), blocks));
                        }
                    }
                }
            }
            TagEnd::Table => {
                if let Some(Frame::Table { aligns, head, rows, .. }) = self.stack.pop() {
                    self.push_block(Block::Table(Table { aligns, head, rows }));
                }
            }
            TagEnd::TableHead => {
                if let Some(Frame::Table { head, cur, in_head, .. }) = self.stack.last_mut() {
                    *head = std::mem::take(cur);
                    *in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(Frame::Table { rows, cur, .. }) = self.stack.last_mut() {
                    rows.push(std::mem::take(cur));
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Superscript => self.sup = self.sup.saturating_sub(1),
            TagEnd::Subscript => self.sub = self.sub.saturating_sub(1),
            TagEnd::Link => {
                self.link_stack.pop();
            }
            TagEnd::Image => {
                if let Some(img) = self.image.take() {
                    self.ensure_acc(AccKind::Paragraph, true);
                    self.flush_spans_to_item();
                    self.acc.as_mut().unwrap().items.push(InlineItem::Image(img));
                }
            }
            TagEnd::MetadataBlock(_) => {
                if let Some(Frame::Metadata { raw }) = self.stack.pop() {
                    let raw = raw.trim().to_owned();
                    if !raw.is_empty() {
                        self.push_block(Block::Metadata(raw));
                    }
                }
            }
        }
    }

    fn inline_html(&mut self, h: &str) {
        let lower = h.trim().to_ascii_lowercase();
        if lower.starts_with("<br") {
            let st = self.style();
            self.push_span("\n", st);
        } else if lower.starts_with("<img") {
            if let Some(img) = parse_img_tag(h) {
                self.ensure_acc(AccKind::Paragraph, true);
                self.flush_spans_to_item();
                self.acc.as_mut().unwrap().items.push(InlineItem::Image(img));
            }
        }
        // Other inline tags are dropped; their text content arrives as separate Text events.
    }

    fn html_block(&mut self, raw: &str) {
        let raw = strip_comments(raw);
        let lower = raw.trim_start().to_ascii_lowercase();
        // Pull out any <img> tags as image blocks.
        let mut imgs = Vec::new();
        let mut rest = raw.as_str();
        while let Some(pos) = find_ci(rest, "<img") {
            let tag_end = rest[pos..].find('>').map(|e| pos + e + 1).unwrap_or(rest.len());
            if let Some(img) = parse_img_tag(&rest[pos..tag_end]) {
                imgs.push(img);
            }
            rest = &rest[tag_end..];
        }
        for img in imgs {
            self.push_block(Block::Image(img));
        }
        // Remaining visible text.
        let text = decode_entities(&strip_tags(&raw));
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        if lower.starts_with("<h") && lower.as_bytes().get(2).is_some_and(|b| (b'1'..=b'6').contains(b)) {
            let level = lower.as_bytes()[2] - b'0';
            let inline = self.make_inline(vec![Span { text: text.clone(), style: SpanStyle::default() }], vec![]);
            let slug = self.unique_slug(slugify(&text));
            let nested = self.nested();
            let block = self.root.len();
            self.headings.push(HeadingRef { level, text, slug, block, nested });
            self.push_block(Block::Heading { level, inline });
            return;
        }
        self.word_count += text.split_whitespace().count();
        let style = SpanStyle { dim: lower.starts_with("<details") || lower.starts_with("<summary"), ..Default::default() };
        let inline = self.make_inline(vec![Span { text, style }], vec![]);
        self.push_block(Block::Paragraph(vec![InlineItem::Text(inline)]));
    }

    fn finish(mut self) -> Document {
        self.flush_implicit();
        self.flush_acc();
        // Close anything left open (malformed input).
        while let Some(frame) = self.stack.pop() {
            match frame {
                Frame::Quote { kind, blocks } => self.push_block(Block::Quote { kind, blocks }),
                Frame::List { start, items } => self.push_block(Block::List { start, items }),
                Frame::Item { checked, blocks, tight } => {
                    if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                        items.push(ListItem { checked, blocks, tight });
                    } else {
                        self.root.extend(blocks);
                    }
                }
                Frame::Footnote { label, blocks } => {
                    let number = self.footnote_number(&label);
                    self.footnotes.push(Footnote { number, blocks });
                }
                Frame::Table { aligns, head, rows, .. } => self.push_block(Block::Table(Table { aligns, head, rows })),
                Frame::DefList { entries } => self.push_block(Block::DefList(entries)),
                Frame::DefDef { blocks } => self.root.extend(blocks),
                Frame::Html { raw } => self.html_block(&raw),
                Frame::Metadata { raw } => self.push_block(Block::Metadata(raw)),
            }
        }
        if !self.footnotes.is_empty() {
            let mut fns = std::mem::take(&mut self.footnotes);
            fns.sort_by_key(|f| f.number);
            self.root.push(Block::Footnotes(fns));
        }
        let title = self
            .headings
            .iter()
            .find(|h| h.level == 1 && !h.nested)
            .or_else(|| self.headings.first())
            .map(|h| h.text.clone());
        Document {
            blocks: self.root,
            headings: self.headings,
            title,
            word_count: self.word_count,
        }
    }
}

fn merge_items(items: Vec<InlineItem>) -> Inline {
    // Headings / cells: concatenate all text items (images become their alt text).
    let mut spans = Vec::new();
    let mut links = Vec::new();
    let mut id = None;
    for it in items {
        match it {
            InlineItem::Text(t) => {
                if id.is_none() {
                    id = Some(t.id);
                }
                let off = links.len() as u32;
                links.extend(t.links);
                for mut s in t.spans {
                    if let Some(l) = s.style.link {
                        s.style.link = Some(l + off);
                    }
                    spans.push(s);
                }
            }
            InlineItem::Image(img) => {
                if !img.alt.is_empty() {
                    spans.push(Span { text: img.alt, style: SpanStyle { italic: true, ..Default::default() } });
                }
            }
        }
    }
    let mut text = String::new();
    for s in &spans {
        text.push_str(&s.text);
    }
    Inline { id: id.unwrap_or(usize::MAX), spans, links, text }
}

fn heading_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// GitHub-style anchor slug.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        } else if c == ' ' {
            out.push('-');
        }
    }
    out
}

fn find_ci(hay: &str, needle_lower: &str) -> Option<usize> {
    let hl = hay.to_ascii_lowercase();
    hl.find(needle_lower)
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut search = 0;
    while let Some(p) = lower[search..].find(name) {
        let i = search + p;
        // must be preceded by whitespace and followed by optional spaces then '='
        let before_ok = i == 0 || lower.as_bytes()[i - 1].is_ascii_whitespace();
        let after = lower[i + name.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            let val_start = tag.len() - after.len() + 1;
            let v = tag[val_start..].trim_start();
            let (quote, v) = match v.chars().next() {
                Some(q @ ('"' | '\'')) => (Some(q), &v[1..]),
                _ => (None, v),
            };
            let end = match quote {
                Some(q) => v.find(q).unwrap_or(v.len()),
                None => v.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(v.len()),
            };
            return Some(&v[..end]);
        }
        search = i + name.len();
    }
    None
}

fn parse_img_tag(tag: &str) -> Option<ImageRef> {
    let src = attr(tag, "src")?;
    Some(ImageRef {
        url: decode_entities(src),
        alt: attr(tag, "alt").map(decode_entities).unwrap_or_default(),
        title: attr(tag, "title").map(decode_entities).unwrap_or_default(),
    })
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("<!--") {
        out.push_str(&rest[..p]);
        match rest[p..].find("-->") {
            Some(e) => rest = &rest[p + e + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p..];
        let Some(end) = after.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let ent = &after[1..end];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "copy" => Some('©'),
            "reg" => Some('®'),
            "trade" => Some('™'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "laquo" => Some('«'),
            "raquo" => Some('»'),
            "bull" => Some('•'),
            "middot" => Some('·'),
            "rarr" => Some('→'),
            "larr" => Some('←'),
            _ => ent
                .strip_prefix('#')
                .and_then(|n| {
                    if let Some(h) = n.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(h, 16).ok()
                    } else {
                        n.parse().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode %XX sequences (for local file paths written as URLs).
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    // Works on bytes: slicing the &str after a '%' could split a multi-byte character.
    let hex = |b: Option<&u8>| b.and_then(|b| (*b as char).to_digit(16)).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_escapes_and_stray_percents() {
        assert_eq!(percent_decode("a%20b%2Fc%e2%82%AC"), "a b/c\u{20AC}");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("50%x"), "50%x");
        assert_eq!(percent_decode("a%\u{20AC}b.png"), "a%\u{20AC}b.png");
        assert_eq!(percent_decode("%4\u{20AC}"), "%4\u{20AC}");
    }

    #[test]
    fn leading_bom_does_not_hide_the_first_heading() {
        let doc = Document::parse("\u{FEFF}# Title\n");
        assert_eq!(doc.headings.len(), 1);
    }
}
