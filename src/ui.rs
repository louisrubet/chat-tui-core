use unicode_width::UnicodeWidthStr;

use std::ops::Range;

use crate::data::{Discussion, Message, Style, Styled, attachment_label};
use crate::graphics::{Thumb, Thumbnails};

/// Columns `s` takes on screen (an emoji takes two).
pub fn width_of(s: &str) -> usize {
    s.width()
}

/// The part of `s` starting `skip` columns in, at most `width` columns wide.
pub fn columns(s: &str, skip: usize, width: usize) -> &str {
    let start = s
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| width_of(&s[..i]) >= skip)
        .unwrap_or(s.len());
    fit(&s[start..], width)
}

/// Longest prefix of `s` that fits in `width` columns.
fn fit(s: &str, width: usize) -> &str {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let next = i + c.len_utf8();
        if width_of(&s[..next]) > width {
            break;
        }
        end = next;
    }
    &s[..end]
}

#[derive(Debug)]
pub enum LineKind {
    Left(String),
    Right(String),
    Center(String),
    /// Row `row` of an image thumbnail, at the right for our messages.
    Image {
        thumb: Thumb,
        row: u16,
        right: bool,
    },
}

/// How a line, or a part of one, stands out, in colour unless `NO_COLOR` is set: the
/// formatting of its text aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tone {
    #[default]
    Normal,
    /// Secondary: times, labels, help.
    Dim,
    /// To be seen first: new messages, the title of the discussion.
    Accent,
    /// Something went wrong (a message not sent).
    Alert,
    /// Someone, by a number of their own ([`person_tone`]).
    Person(u8),
}

/// How many [`Tone::Person`] there are.
pub const PERSON_TONES: u8 = 6;

/// The tone of `author` (an id, or a name): the same on every run.
pub fn person_tone(author: &str) -> Tone {
    // FNV-1a: unlike the hasher of the standard library, the same on every run.
    let hash = author.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    Tone::Person((hash % u32::from(PERSON_TONES)) as u8)
}

pub struct RenderLine {
    /// Index of the message this line belongs to, if any (blank/date lines have None).
    pub msg_idx: Option<usize>,
    pub kind: LineKind,
    /// Formatting of the line text.
    pub styles: Vec<Styled>,
    pub tone: Tone,
}

impl RenderLine {
    pub fn new(msg_idx: Option<usize>, kind: LineKind) -> Self {
        RenderLine {
            msg_idx,
            kind,
            styles: Vec::new(),
            tone: Tone::Normal,
        }
    }

    /// The same, in `tone`.
    pub fn toned(self, tone: Tone) -> Self {
        RenderLine { tone, ..self }
    }
}

/// The parts of `styles` within `range`, moved so that `range` starts at `at`.
pub fn styles_in(styles: &[Styled], range: Range<usize>, at: usize) -> Vec<Styled> {
    styles
        .iter()
        .filter_map(|s| {
            let (start, end) = (s.start.max(range.start), s.end.min(range.end));
            (start < end).then(|| Styled {
                start: start - range.start + at,
                end: end - range.start + at,
                style: s.style,
            })
        })
        .collect()
}

pub fn trunc(s: &str, width: usize) -> String {
    if width_of(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", fit(s, width - 1))
}

pub fn pad_left(s: &str, width: usize) -> String {
    let s = trunc(s, width);
    let pad = width.saturating_sub(width_of(&s));
    format!("{s}{}", " ".repeat(pad))
}

pub fn pad_right(s: &str, width: usize) -> String {
    let s = trunc(s, width);
    let pad = width.saturating_sub(width_of(&s));
    format!("{}{s}", " ".repeat(pad))
}

fn pad_center(s: &str, width: usize) -> String {
    let s = trunc(s, width);
    let total = width.saturating_sub(width_of(&s));
    let left = total / 2;
    let right = total - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

/// The line padded to `width` columns, with its styles moved along.
pub fn render_line(line: &RenderLine, width: usize) -> (String, Vec<Styled>) {
    let (text, rendered) = match &line.kind {
        LineKind::Left(t) => (t, pad_left(t, width)),
        LineKind::Right(t) => (t, pad_right(t, width)),
        LineKind::Center(t) => (t, pad_center(t, width)),
        LineKind::Image { thumb, row, right } => {
            let shown = (thumb.cols as usize).min(width);
            let pad = " ".repeat(width - shown);
            let cells = thumb.row(*row, width);
            let rendered = if *right {
                format!("{pad}{cells}")
            } else {
                format!("{cells}{pad}")
            };
            return (rendered, Vec::new());
        }
    };
    // Where the text starts once padded, and how much of it is kept when truncated.
    let at = rendered.len() - rendered.trim_start_matches(' ').len();
    let shown = trunc(text, width);
    let kept = if shown == *text {
        text.len()
    } else {
        shown.len() - '…'.len_utf8()
    };
    (rendered, styles_in(&line.styles, 0..kept, at))
}

/// Greedy word-wrap, keeping the line breaks of `text`. Words longer than `width` are hard-broken.
#[cfg(test)]
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    wrap_ranges(text, width)
        .into_iter()
        .map(|r| text[r].to_string())
        .collect()
}

/// [`wrap_text`] as byte ranges of `text`, so that its styles can follow.
fn wrap_ranges(text: &str, width: usize) -> Vec<Range<usize>> {
    if width == 0 {
        return std::iter::once(0..text.len()).collect();
    }
    let offset = |part: &str| part.as_ptr() as usize - text.as_ptr() as usize;
    text.lines()
        .flat_map(|line| {
            wrap_line(line, width)
                .into_iter()
                .map(move |r| r.start + offset(line)..r.end + offset(line))
        })
        .collect()
}

/// One line of text wrapped, as byte ranges of it (the spaces between the words of a
/// wrapped line are kept as typed).
fn wrap_line(text: &str, width: usize) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut current: Option<Range<usize>> = None;
    for word in text.split_whitespace() {
        let start = word.as_ptr() as usize - text.as_ptr() as usize;
        let end = start + word.len();
        if width_of(word) > width {
            lines.extend(current.take());
            let mut rest = start;
            while width_of(&text[rest..end]) > width {
                // At least one char per line, even wider than `width`.
                let head = fit(&text[rest..end], width);
                let at = if head.is_empty() {
                    text[rest..]
                        .chars()
                        .next()
                        .map_or(end - rest, char::len_utf8)
                } else {
                    head.len()
                };
                lines.push(rest..rest + at);
                rest += at;
            }
            current = (rest < end).then_some(rest..end);
            continue;
        }
        current = match current.take() {
            Some(line) if width_of(&text[line.start..end]) <= width => Some(line.start..end),
            Some(line) => {
                lines.push(line);
                Some(start..end)
            }
            None => Some(start..end),
        };
    }
    lines.extend(current);
    if lines.is_empty() {
        lines.push(0..0);
    }
    lines
}

/// Lines of description shown in a link preview.
const PREVIEW_LINES: usize = 3;

/// The link previews and attachments of message `m` (number `idx`), under its text:
/// thumbnails once sent to the terminal, otherwise a line saying what is there.
fn push_media(
    out: &mut Vec<RenderLine>,
    idx: usize,
    m: &Message,
    thumbnails: &Thumbnails,
    width: usize,
    right: bool,
) {
    if m.deleted {
        return;
    }
    let text = |t: String| {
        RenderLine::new(
            Some(idx),
            if right {
                LineKind::Right(t)
            } else {
                LineKind::Left(t)
            },
        )
    };
    let image = |out: &mut Vec<RenderLine>, thumb: Thumb| {
        for row in 0..thumb.rows {
            out.push(RenderLine::new(
                Some(idx),
                LineKind::Image { thumb, row, right },
            ));
        }
    };

    for (i, preview) in m.previews.iter().enumerate() {
        if let Some(thumb) = thumbnails.preview(&m.id, i) {
            image(out, thumb);
        }
        let bar = "\u{2502} ";
        let style = |line: &str, style| {
            vec![Styled {
                start: bar.len(),
                end: line.len(),
                style,
            }]
        };
        let mut card = Vec::new();
        if !preview.title.is_empty() {
            let line = format!("{bar}{}", preview.title);
            card.push((style(&line, Style::Bold), line));
        }
        let description = &preview.description;
        for r in wrap_ranges(description, width.saturating_sub(2))
            .into_iter()
            .take(PREVIEW_LINES)
        {
            card.push((Vec::new(), format!("{bar}{}", &description[r])));
        }
        if let Some(host) = url::Url::parse(&preview.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        {
            let line = format!("{bar}{host}");
            card.push((style(&line, Style::Italic), line));
        }
        // Lines of the same width, so that the bars line up on the right too.
        let card_width = card.iter().map(|(_, l)| width_of(l)).max().unwrap_or(0);
        for (styles, line) in card {
            let pad = " ".repeat(card_width - width_of(&line));
            out.push(RenderLine {
                styles,
                ..text(format!("{line}{pad}"))
            });
        }
    }

    // Without thumbnails, the attachments are labelled in the text.
    if !thumbnails.enabled() {
        return;
    }
    for (i, attachment) in m.attachments.iter().enumerate() {
        match thumbnails.get(&m.id, i) {
            Some(Some(thumb)) => image(out, thumb),
            Some(None) => out.push(text("[error loading image]".to_string())),
            None => out.push(text(attachment_label(&attachment.content_type, None))),
        }
    }
    for content_type in &m.remote_attachments {
        let line = if !content_type.starts_with("image/") || !m.images_only() {
            attachment_label(content_type, None)
        } else if m.download_failed {
            "[error loading image]".to_string()
        } else {
            "[loading image\u{2026}]".to_string()
        };
        out.push(text(line));
    }
}

/// Builds the flat list of screen lines for a discussion: date separators,
/// sender name / message text / time lines, and blank spacers between messages.
/// `new_from` (index, count) puts a "N new messages" line above the first new message.
pub fn build_discussion_lines(
    discussion: &Discussion,
    width: usize,
    new_from: Option<(usize, usize)>,
    thumbnails: &Thumbnails,
) -> Vec<RenderLine> {
    let mut out = Vec::new();
    let mut last_date: Option<chrono::NaiveDate> = None;
    let mut next_attachment = 1;

    for (idx, m) in discussion.messages.iter().enumerate() {
        let date = m.timestamp.date();
        if last_date != Some(date) {
            out.push(
                RenderLine::new(
                    None,
                    LineKind::Center(date.format("%A, %B %-d, %Y").to_string()),
                )
                .toned(Tone::Dim),
            );
            last_date = Some(date);
        }
        if let Some((first, count)) = new_from
            && idx == first
        {
            let label = if count == 1 {
                "1 new message".to_string()
            } else {
                format!("{count} new messages")
            };
            let rule = "\u{2500}\u{2500}\u{2500}";
            out.push(
                RenderLine::new(None, LineKind::Center(format!("{rule} {label} {rule}")))
                    .toned(Tone::Accent),
            );
        }

        let time_str = m.timestamp.format("%H:%M").to_string();
        let (text, styles) = m.display((!thumbnails.enabled()).then_some(next_attachment));
        next_attachment += m.attachment_count();
        let text = text.as_str();

        // "Forwarded" label and quoted message, shown above the text (truncated to one line).
        let mut header = Vec::new();
        if m.forwarded {
            header.push("Forwarded".to_string());
        }
        if let Some(quoted) = m.reply_to.and_then(|i| discussion.messages.get(i)) {
            let name = if quoted.from_me {
                "You"
            } else {
                &quoted.sender_name
            };
            header.push(format!("\u{21b3} {name}: {}", quoted.summary()));
        }

        if m.from_me {
            for h in header {
                out.push(RenderLine::new(Some(idx), LineKind::Right(h)).toned(Tone::Dim));
            }
            for r in wrap_ranges(text, width) {
                let styles = styles_in(&styles, r.clone(), 0);
                out.push(RenderLine {
                    styles,
                    ..RenderLine::new(Some(idx), LineKind::Right(text[r].to_string()))
                });
            }
            push_media(&mut out, idx, m, thumbnails, width, true);
            if !m.reactions.is_empty() {
                out.push(RenderLine::new(
                    Some(idx),
                    LineKind::Right(m.reaction_summary()),
                ));
            }
            if m.failed {
                let warning = "\u{26a0} Not sent";
                out.push(RenderLine {
                    styles: vec![Styled {
                        start: 0,
                        end: warning.len(),
                        style: Style::Bold,
                    }],
                    tone: Tone::Alert,
                    ..RenderLine::new(
                        Some(idx),
                        LineKind::Right(format!("{warning}   {time_str}")),
                    )
                });
            } else if m.sending {
                out.push(RenderLine {
                    styles: vec![Styled {
                        start: 0,
                        end: "Sending\u{2026}".len(),
                        style: Style::Italic,
                    }],
                    tone: Tone::Dim,
                    ..RenderLine::new(Some(idx), LineKind::Right("Sending\u{2026}".to_string()))
                });
            } else {
                out.push(RenderLine::new(Some(idx), LineKind::Right(time_str)).toned(Tone::Dim));
            }
        } else {
            let name = &m.sender_name;
            out.push(RenderLine {
                styles: vec![Styled {
                    start: 0,
                    end: name.len(),
                    style: Style::Bold,
                }],
                tone: person_tone(m.author.as_deref().unwrap_or(name)),
                ..RenderLine::new(Some(idx), LineKind::Left(name.clone()))
            });
            for h in header {
                out.push(RenderLine::new(Some(idx), LineKind::Left(h)).toned(Tone::Dim));
            }
            for r in wrap_ranges(text, width) {
                let styles = styles_in(&styles, r.clone(), 0);
                out.push(RenderLine {
                    styles,
                    ..RenderLine::new(Some(idx), LineKind::Left(text[r].to_string()))
                });
            }
            push_media(&mut out, idx, m, thumbnails, width, false);
            if !m.reactions.is_empty() {
                out.push(RenderLine::new(
                    Some(idx),
                    LineKind::Left(m.reaction_summary()),
                ));
            }
            out.push(RenderLine::new(Some(idx), LineKind::Left(time_str)).toned(Tone::Dim));
        }

        // Blank spacer between messages for readability.
        out.push(RenderLine::new(None, LineKind::Left(String::new())));
    }

    if matches!(out.last(), Some(RenderLine { msg_idx: None, kind: LineKind::Left(s), .. }) if s.is_empty())
    {
        out.pop();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::wrap_text;

    #[test]
    fn emoji_take_two_columns() {
        use super::{pad_left, pad_right, trunc, width_of};
        assert_eq!(width_of("\u{1f44d} 2"), 4);
        assert_eq!(pad_right("\u{1f44d}", 4), "  \u{1f44d}");
        assert_eq!(width_of(&pad_left("\u{2764}\u{fe0f} 2  \u{1f44d}", 12)), 12);
        assert_eq!(trunc("\u{1f44d}\u{1f44d}\u{1f44d}", 4), "\u{1f44d}…");
    }

    #[test]
    fn styles_follow_wrapping_and_alignment() {
        use super::{LineKind, Thumbnails, build_discussion_lines, render_line};
        use crate::data::{Discussion, Message, Style, Styled};
        // "aa bbbb cc", "bbbb cc" bold, wrapped at 7 columns: "aa bbbb" / "cc".
        let mut message = Message::mine(1, "aa bbbb cc".to_string(), None, false);
        message.styles = vec![Styled {
            start: 3,
            end: 10,
            style: Style::Bold,
        }];
        let discussion = Discussion {
            messages: vec![message],
            ..Discussion::new("t", "t")
        };
        let lines = build_discussion_lines(&discussion, 7, None, &Thumbnails::default());
        let texts: Vec<_> = lines
            .iter()
            .map(|l| match &l.kind {
                LineKind::Right(t) | LineKind::Left(t) | LineKind::Center(t) => t.as_str(),
                LineKind::Image { .. } => "",
            })
            .collect();
        assert_eq!(&texts[1..3], ["aa bbbb", "cc"]);
        assert_eq!(
            lines[1].styles,
            [Styled {
                start: 3,
                end: 7,
                style: Style::Bold
            }]
        );
        assert_eq!(
            lines[2].styles,
            [Styled {
                start: 0,
                end: 2,
                style: Style::Bold
            }]
        );
        // Right-aligned in 10 columns: "cc" starts at 8.
        let (text, styles) = render_line(&lines[2], 10);
        assert_eq!(text, "        cc");
        assert_eq!(
            styles,
            [Styled {
                start: 8,
                end: 10,
                style: Style::Bold
            }]
        );
    }

    #[test]
    fn wrap_keeps_line_breaks() {
        assert_eq!(
            wrap_text("one two\n\nthree", 20),
            vec!["one two", "", "three"]
        );
        assert_eq!(wrap_text("aaa bbb\nccc", 4), vec!["aaa", "bbb", "ccc"]);
    }

    fn texts(lines: &[super::RenderLine]) -> Vec<&str> {
        use super::LineKind;
        lines
            .iter()
            .map(|l| match &l.kind {
                LineKind::Right(t) | LineKind::Left(t) | LineKind::Center(t) => t.as_str(),
                LineKind::Image { .. } => "<image>",
            })
            .collect()
    }

    #[test]
    fn link_previews_show_as_cards() {
        use super::{Thumbnails, build_discussion_lines, width_of};
        use crate::data::{Discussion, LinkPreview, Message};
        let mut message = Message::mine(1, "https://example.org/a".to_string(), None, false);
        message.previews = vec![LinkPreview {
            url: "https://example.org/a".to_string(),
            title: "Example".to_string(),
            description: "A page".to_string(),
            ..Default::default()
        }];
        let discussion = Discussion {
            messages: vec![message],
            ..Discussion::new("t", "t")
        };
        let lines = build_discussion_lines(&discussion, 40, None, &Thumbnails::default());
        let card = &texts(&lines)[2..5];
        assert!(
            card.iter().all(|l| width_of(l) == width_of(card[2])),
            "same width, for the bars to line up on the right"
        );
        let trimmed: Vec<&str> = texts(&lines).iter().map(|l| l.trim_end()).collect();
        assert_eq!(
            &trimmed[1..5],
            [
                "https://example.org/a",
                "\u{2502} Example",
                "\u{2502} A page",
                "\u{2502} example.org"
            ]
        );
    }

    #[test]
    fn a_message_not_sent_says_so() {
        use super::{Thumbnails, build_discussion_lines};
        use crate::data::{Discussion, Message};
        let mut message = Message::mine(1, "hello".to_string(), None, false);
        message.failed = true;
        let discussion = Discussion {
            messages: vec![message],
            ..Discussion::new("t", "t")
        };
        let lines = build_discussion_lines(&discussion, 40, None, &Thumbnails::default());
        assert!(texts(&lines)[2].starts_with("\u{26a0} Not sent   "));
    }

    #[test]
    fn each_person_keeps_a_tone_secondary_lines_are_dimmed() {
        use super::{Thumbnails, Tone, build_discussion_lines, person_tone};
        use crate::data::{Discussion, Message};
        // The same on every run: FNV-1a of "+33600000000" is 0xbb60_2f4a, 2 modulo 6.
        assert_eq!(person_tone("+33600000000"), Tone::Person(2));
        assert_eq!(person_tone("bob"), person_tone("bob"));
        let tones: std::collections::HashSet<Tone> =
            ["ann", "bob", "carl", "dora", "eve", "fred", "gus", "hal"]
                .iter()
                .map(|name| person_tone(name))
                .collect();
        assert!(tones.len() >= 4, "spread: {tones:?}");

        let mut theirs = Message::mine(1, "hi".to_string(), None, false);
        theirs.from_me = false;
        theirs.sender_name = "Ann".to_string();
        theirs.author = Some("ann-id".to_string());
        let discussion = Discussion {
            messages: vec![theirs, Message::mine(2, "hello".to_string(), None, false)],
            ..Discussion::new("t", "t")
        };
        let lines = build_discussion_lines(&discussion, 40, None, &Thumbnails::default());
        let tones: Vec<Tone> = lines.iter().map(|l| l.tone).collect();
        assert_eq!(
            tones,
            [
                Tone::Dim,             // date
                person_tone("ann-id"), // Ann, by her id
                Tone::Normal,          // hi
                Tone::Dim,             // time
                Tone::Normal,          // blank
                Tone::Normal,          // hello
                Tone::Dim,             // time
            ]
        );
    }

    #[test]
    fn images_show_as_thumbnails_or_a_status_line() {
        use super::{Thumbnails, build_discussion_lines};
        use crate::data::{Discussion, Message};
        let mut message = Message::mine(1, String::new(), None, false);
        message.remote_attachments = vec!["image/jpeg".to_string()];
        let mut discussion = Discussion {
            messages: vec![message],
            ..Discussion::new("t", "t")
        };
        let lines = build_discussion_lines(&discussion, 40, None, &Thumbnails::on());
        assert_eq!(
            texts(&lines)[1],
            "[loading image\u{2026}]",
            "no [image #1] label"
        );
        discussion.messages[0].download_failed = true;
        let lines = build_discussion_lines(&discussion, 40, None, &Thumbnails::on());
        assert_eq!(texts(&lines)[1], "[error loading image]");
        let lines = build_discussion_lines(&discussion, 40, None, &Thumbnails::default());
        assert_eq!(texts(&lines)[1], "[image #1]", "labels without thumbnails");
    }
}
