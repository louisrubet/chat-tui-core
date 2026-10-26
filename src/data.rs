//! The chats and their messages, as the interface shows them.

use chrono::NaiveDateTime;

/// Identifies a chat for its backend (a Signal conversation, a Google Messages thread…).
pub type ChatId = String;
/// Identifies a message in its chat, for its backend.
pub type MessageId = String;

#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    /// When it was sent, in milliseconds since the Unix epoch.
    pub sent_ms: u64,
    /// Its author for the backend (needed to quote it, to match deletions), `None` for ours.
    pub author: Option<String>,
    pub from_me: bool,
    pub sender_name: String,
    pub text: String,
    pub timestamp: NaiveDateTime,
    /// Index, in the same discussion, of the message this one replies to.
    pub reply_to: Option<usize>,
    pub forwarded: bool,
    /// Emoji reactions, one per author.
    pub reactions: Vec<Reaction>,
    /// Deleted for everyone: shown as "This message was deleted".
    pub deleted: bool,
    /// Formatting of `text`.
    pub styles: Vec<Styled>,
    /// Attachments available locally: sent from this client, or downloaded.
    pub attachments: Vec<Attachment>,
    /// Content types of the received attachments not downloaded yet (fetched on open).
    pub remote_attachments: Vec<String>,
    /// Previews of the links of `text`, as the sender's app made them.
    pub previews: Vec<LinkPreview>,
    /// Downloading its attachments failed (expired on the servers…).
    pub download_failed: bool,
    /// Ours, not sent: given up by the phone or the service.
    pub failed: bool,
    /// Ours, being sent: shown until it is (or fails, or is cancelled).
    pub sending: bool,
}

/// Preview of a link: title, description and image of the page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LinkPreview {
    pub url: String,
    pub title: String,
    pub description: String,
    /// Its image, once downloaded.
    pub image: Option<Attachment>,
    /// It has an image not downloaded yet.
    pub remote_image: bool,
}

/// File or image data attached to a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub content_type: String,
    pub file_name: Option<String>,
    pub bytes: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Attachment {
    /// The image in the system clipboard, as PNG.
    pub fn from_clipboard() -> Option<Self> {
        use image::ImageEncoder;
        let image = arboard::Clipboard::new().ok()?.get_image().ok()?;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &image.bytes,
                image.width as u32,
                image.height as u32,
                image::ColorType::Rgba8.into(),
            )
            .ok()?;
        Some(Attachment {
            content_type: "image/png".to_string(),
            file_name: Some("clipboard.png".to_string()),
            bytes: png,
            width: Some(image.width as u32),
            height: Some(image.height as u32),
        })
    }
}

/// Text formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Bold,
    Italic,
    Strikethrough,
}

/// `style` applied to the bytes `start..end` of a text.
#[derive(Debug, Clone, PartialEq)]
pub struct Styled {
    pub start: usize,
    pub end: usize,
    pub style: Style,
}

/// `text` trimmed, with `styles` moved and cut to match.
pub fn trim_styled(text: &str, styles: &[Styled]) -> (String, Vec<Styled>) {
    let start = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    let styles = styles
        .iter()
        .map(|s| Styled {
            start: s.start.saturating_sub(start).min(trimmed.len()),
            end: s.end.saturating_sub(start).min(trimmed.len()),
            style: s.style,
        })
        .filter(|s| s.start < s.end)
        .collect();
    (trimmed.to_string(), styles)
}

/// An emoji reaction to a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Reaction {
    /// Who reacted, for the backend.
    pub author: String,
    pub emoji: String,
    /// Our own reaction (from this device or another).
    pub mine: bool,
}

impl Message {
    /// A message written here, sent now.
    pub fn mine(id: impl ToString, text: String, reply_to: Option<usize>, forwarded: bool) -> Self {
        Message {
            id: id.to_string(),
            sent_ms: chrono::Utc::now().timestamp_millis() as u64,
            author: None,
            from_me: true,
            sender_name: String::new(),
            text,
            timestamp: chrono::Local::now().naive_local(),
            reply_to,
            forwarded,
            reactions: Vec::new(),
            deleted: false,
            styles: Vec::new(),
            attachments: Vec::new(),
            remote_attachments: Vec::new(),
            previews: Vec::new(),
            download_failed: false,
            failed: false,
            sending: false,
        }
    }

    /// Number of its attachments, downloaded or not.
    pub fn attachment_count(&self) -> usize {
        self.attachments.len() + self.remote_attachments.len()
    }

    /// Content types of its attachments, downloaded or not.
    fn attachment_types(&self) -> impl Iterator<Item = &str> {
        self.attachments
            .iter()
            .map(|a| a.content_type.as_str())
            .chain(self.remote_attachments.iter().map(String::as_str))
    }

    /// Whether it has attachments or preview images not downloaded yet.
    pub fn needs_download(&self) -> bool {
        !self.remote_attachments.is_empty() || self.previews.iter().any(|p| p.remote_image)
    }

    /// Whether its downloads are only images, worth fetching for the thumbnails.
    pub fn images_only(&self) -> bool {
        self.remote_attachments
            .iter()
            .all(|t| t.starts_with("image/"))
    }

    /// Stores what was downloaded: its attachments and the images of its previews.
    pub fn set_downloaded(
        &mut self,
        attachments: Vec<Attachment>,
        images: Vec<Option<Attachment>>,
    ) {
        self.attachments = attachments;
        self.remote_attachments.clear();
        for (preview, image) in self.previews.iter_mut().zip(images) {
            preview.image = image;
            preview.remote_image = false;
        }
        self.download_failed = false;
    }

    /// Its text, with a label per attachment in front ("[image #3]") when numbered from
    /// `first` in the discussion, and the styles moved to match.
    pub fn display(&self, first: Option<usize>) -> (String, Vec<Styled>) {
        if self.deleted {
            return ("This message was deleted".to_string(), Vec::new());
        }
        let Some(first) = first else {
            return (self.text.clone(), self.styles.clone());
        };
        let labels: Vec<String> = self
            .attachment_types()
            .enumerate()
            .map(|(i, content_type)| attachment_label(content_type, Some(first + i)))
            .collect();
        self.prefixed(labels)
    }

    /// Its text in one line, for quotes and notifications: "[image] look".
    pub fn summary(&self) -> String {
        if self.deleted {
            return "This message was deleted".to_string();
        }
        let labels = self
            .attachment_types()
            .map(|t| attachment_label(t, None))
            .collect();
        self.prefixed(labels).0
    }

    /// Its text after `labels`, with the styles moved to match.
    fn prefixed(&self, mut labels: Vec<String>) -> (String, Vec<Styled>) {
        if labels.is_empty() {
            return (self.text.clone(), self.styles.clone());
        }
        if !self.text.is_empty() {
            labels.push(String::new());
        }
        let prefix = labels.join(" ");
        let styles = self
            .styles
            .iter()
            .map(|s| Styled {
                start: s.start + prefix.len(),
                end: s.end + prefix.len(),
                style: s.style,
            })
            .collect();
        (prefix + &self.text, styles)
    }

    /// Whether we can delete it for everyone at `now_ms`: ours, sent less than `max_age_ms`
    /// ago.
    pub fn deletable(&self, now_ms: u64, max_age_ms: u64) -> bool {
        self.from_me && !self.deleted && now_ms.saturating_sub(self.sent_ms) < max_age_ms
    }

    /// Deleted for everyone: its text and reactions go away.
    pub fn mark_deleted(&mut self) {
        self.deleted = true;
        self.text.clear();
        self.reactions.clear();
        self.styles.clear();
        self.previews.clear();
    }

    /// Sets the reaction of `author` (replacing theirs, or ours when `mine`), or removes it
    /// with `None`.
    pub fn set_reaction(&mut self, author: &str, mine: bool, emoji: Option<&str>) {
        self.reactions
            .retain(|r| r.author != author && !(mine && r.mine));
        if let Some(emoji) = emoji {
            self.reactions.push(Reaction {
                author: author.to_string(),
                emoji: emoji.to_string(),
                mine,
            });
        }
    }

    /// Our own reaction, if any.
    pub fn my_reaction(&self) -> Option<&str> {
        self.reactions
            .iter()
            .find(|r| r.mine)
            .map(|r| r.emoji.as_str())
    }

    /// The reactions grouped by emoji, in order of appearance: `❤️ 2  👍` (count when > 1).
    pub fn reaction_summary(&self) -> String {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for reaction in &self.reactions {
            match counts
                .iter_mut()
                .find(|(emoji, _)| *emoji == reaction.emoji)
            {
                Some((_, count)) => *count += 1,
                None => counts.push((&reaction.emoji, 1)),
            }
        }
        let parts: Vec<String> = counts
            .into_iter()
            .map(|(emoji, count)| {
                if count > 1 {
                    format!("{emoji} {count}")
                } else {
                    emoji.to_string()
                }
            })
            .collect();
        parts.join("  ")
    }
}

#[derive(Debug, Clone)]
pub struct Discussion {
    pub id: ChatId,
    pub title: String,
    /// Several people besides us: their names go with their messages.
    pub is_group: bool,
    pub messages: Vec<Message>,
    /// Number of messages received since it was last opened.
    pub unread: usize,
    /// Rank in the "Pinned" list (pin order: a chat pinned later comes after); `None` when
    /// not pinned.
    pub pinned: Option<u32>,
    /// Gone (deleted, for good): not listed; still shown while open.
    pub hidden: bool,
    /// Filed away out of the chat list, which leads to it; `None` in the inbox.
    pub folder: Option<Folder>,
}

/// Where chats are filed away, out of the chat list (in this order after it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Folder {
    Archive,
    Spam,
    Trash,
}

impl Folder {
    pub const ALL: [Folder; 3] = [Folder::Archive, Folder::Spam, Folder::Trash];

    pub fn name(self) -> &'static str {
        match self {
            Folder::Archive => "Archive",
            Folder::Spam => "Spam & blocked",
            Folder::Trash => "Trash",
        }
    }
}

impl Discussion {
    /// Our last message was not sent (the phone gave up): flagged in the chat list. One
    /// failed before another of ours went is no longer flagged.
    pub fn not_sent(&self) -> bool {
        self.messages
            .iter()
            .rev()
            .find(|m| m.from_me && !m.deleted)
            .is_some_and(|m| m.failed)
    }

    /// Index of the oldest unread message: the `unread`-th received one from the end.
    pub fn first_unread(&self) -> Option<usize> {
        if self.unread == 0 {
            return None;
        }
        let mut left = self.unread;
        for (i, m) in self.messages.iter().enumerate().rev() {
            if !m.from_me {
                left -= 1;
                if left == 0 {
                    return Some(i);
                }
            }
        }
        None
    }

    pub fn new(id: impl Into<ChatId>, title: impl Into<String>) -> Self {
        Discussion {
            id: id.into(),
            title: title.into(),
            is_group: false,
            messages: Vec::new(),
            unread: 0,
            pinned: None,
            hidden: false,
            folder: None,
        }
    }

    /// Number of the first attachment of message `idx`: attachments are numbered from 1 in
    /// the order they appear in the discussion.
    pub fn first_attachment(&self, idx: usize) -> usize {
        1 + self.messages[..idx]
            .iter()
            .map(Message::attachment_count)
            .sum::<usize>()
    }

    /// Number the next attachment will get.
    pub fn next_attachment(&self) -> usize {
        self.first_attachment(self.messages.len())
    }

    /// Index of the message with this id.
    pub fn position_of(&self, id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| m.id == id)
    }
}

/// Label of an attachment: "[image]", "[video]"…, with its `number` in the discussion
/// when given ("[image #3]").
pub fn attachment_label(content_type: &str, number: Option<usize>) -> String {
    let kind = match content_type.split('/').next().unwrap_or_default() {
        kind @ ("image" | "video" | "audio") => kind,
        _ => "file",
    };
    match number {
        Some(number) => format!("[{kind} #{number}]"),
        None => format!("[{kind}]"),
    }
}

/// Returns the links found in `text`, as written, without trailing punctuation: http(s)
/// ones, and those without a scheme, like `example.org/page` or `www.example.org` (see
/// [`url`]).
pub fn links(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .map(|word| word.trim_start_matches(['(', '<', '"', '\'']))
        .map(|word| word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '>', '"', '\'']))
        .filter(|word| {
            word.starts_with("http://") || word.starts_with("https://") || is_bare_link(word)
        })
        .collect()
}

/// The address to open for a `link` of [`links`]: https for one without a scheme.
pub fn url(link: &str) -> String {
    if link.contains("://") {
        link.to_string()
    } else {
        format!("https://{link}")
    }
}

/// Top-level domains enough to tell a link without a scheme or path (`example.org`) from a
/// file name (`notes.txt`).
const COMMON_TLDS: &[&str] = &[
    "com", "org", "net", "fr", "be", "ch", "ca", "de", "uk", "eu", "io", "info", "app", "dev",
];

/// A link without a scheme: a domain name (`www.` ahead, a known top-level domain, or a
/// path after), not an email address.
fn is_bare_link(word: &str) -> bool {
    if word.contains("://") || word.contains('@') {
        return false;
    }
    let end = word.find(['/', '?', '#']).unwrap_or(word.len());
    let (host, rest) = word.split_at(end);
    let labels: Vec<&str> = host.split('.').collect();
    let valid = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        });
    let tld = labels[labels.len() - 1];
    valid
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && (host.starts_with("www.")
            || rest.starts_with('/')
            || COMMON_TLDS.contains(&tld.to_ascii_lowercase().as_str()))
}

fn dt(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
}

fn msg(from_me: bool, sender_name: &str, text: &str, ts: &str) -> Message {
    let timestamp = dt(ts);
    let sent_ms = timestamp.and_utc().timestamp_millis() as u64;
    Message {
        id: sent_ms.to_string(),
        sent_ms,
        author: None,
        from_me,
        sender_name: sender_name.to_string(),
        text: text.to_string(),
        timestamp,
        reply_to: None,
        forwarded: false,
        reactions: Vec::new(),
        deleted: false,
        styles: Vec::new(),
        attachments: Vec::new(),
        remote_attachments: Vec::new(),
        previews: Vec::new(),
        download_failed: false,
        failed: false,
        sending: false,
    }
}

pub fn mock_discussions() -> Vec<Discussion> {
    vec![
        Discussion {
            id: "alice".to_string(),
            title: "Alice Martin".to_string(),
            is_group: false,
            unread: 0,
            pinned: Some(0),
            hidden: false,
            folder: None,
            messages: vec![
                msg(false, "Alice Martin", "Hey! Are we still on for tomorrow?", "2026-07-15 09:12"),
                msg(true, "", "Yes, absolutely. What time works for you?", "2026-07-15 09:15"),
                msg(false, "Alice Martin", "How about 10am at the usual place?", "2026-07-15 09:16"),
                msg(true, "", "Works for me.", "2026-07-15 09:17"),
                msg(false, "Alice Martin", "Great, see you then! Also, do you remember the name of that restaurant we went to last month with the really long menu and the amazing dessert selection?", "2026-07-16 18:40"),
                msg(true, "", "Ah yes, it was called \"La Petite Table\", I think.", "2026-07-16 18:45"),
                msg(false, "Alice Martin", "That's the one, thanks!", "2026-07-16 18:46"),
                msg(true, "", "Morning!", "2026-07-17 08:02"),
            ],
        },
        Discussion {
            id: "family".to_string(),
            title: "Family Group".to_string(),
            is_group: true,
            unread: 2,
            pinned: None,
            hidden: false,
            folder: None,
            messages: vec![
                msg(false, "Mom", "Don't forget dinner on Sunday.", "2026-07-13 12:00"),
                msg(false, "Dad", "I'll bring the wine.", "2026-07-13 12:03"),
                msg(true, "", "Sounds good, I'll bring dessert.", "2026-07-13 12:05"),
                msg(false, "Mom", "Perfect, see you all then.", "2026-07-14 20:10"),
            ],
        },
        Discussion {
            id: "bob".to_string(),
            title: "Bob Dupont".to_string(),
            is_group: false,
            unread: 0,
            pinned: None,
            hidden: false,
            folder: None,
            messages: vec![
                msg(false, "Bob Dupont", "Did you push the fix?", "2026-07-14 10:00"),
                msg(true, "", "Yep, just pushed it.", "2026-07-14 10:01"),
                msg(false, "Bob Dupont", "Nice, testing now.", "2026-07-14 10:02"),
                msg(
                    false,
                    "Bob Dupont",
                    "Found the root cause: https://github.com/whisperfish/presage/issues and the docs (https://docs.rs/presage).",
                    "2026-07-14 10:30",
                ),
                Message { reply_to: Some(3), ..msg(true, "", "Thanks, having a look.", "2026-07-14 10:35") },
            ]
            .into_iter()
            .enumerate()
            .map(|(i, mut m)| {
                // A few reactions for the demo.
                if i == 3 {
                    m.set_reaction("me", true, Some("\u{1f44d}"));
                    m.set_reaction("bob", false, Some("\u{1f44d}"));
                }
                if i == 4 {
                    m.set_reaction("bob", false, Some("\u{2764}\u{fe0f}"));
                }
                m
            })
            .collect(),
        },
        Discussion {
            id: "chloe".to_string(),
            title: "Chloe Renard".to_string(),
            is_group: false,
            unread: 0,
            pinned: None,
            hidden: false,
            folder: None,
            messages: vec![
                msg(false, "Chloe Renard", "Happy birthday!! 🎉", "2026-06-01 07:30"),
                msg(true, "", "Thank you so much!", "2026-06-01 08:15"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_links() {
        assert_eq!(
            links("See https://a.example/x, and (http://b.example/y). Not ftp://c.example"),
            vec!["https://a.example/x", "http://b.example/y"]
        );
        assert!(links("no link here").is_empty());
    }

    #[test]
    fn extracts_links_without_a_scheme() {
        assert_eq!(
            links("RDV: doctolib.fr/a/g3UFiHh2?o=a, www.example.co.jp et example.org."),
            vec![
                "doctolib.fr/a/g3UFiHh2?o=a",
                "www.example.co.jp",
                "example.org"
            ]
        );
        assert!(
            links("notes.txt e.g. i.e. 3.14 a.b/c mail@example.org ...").is_empty(),
            "file names, abbreviations, numbers and email addresses are not links"
        );
        assert_eq!(url("doctolib.fr/a/x?o=a"), "https://doctolib.fr/a/x?o=a");
        assert_eq!(url("http://example.org"), "http://example.org");
    }

    #[test]
    fn attachments_are_labelled_in_discussion_order() {
        let mut discussion = Discussion::new("alice", "Alice");
        let mut photo = Message::mine(1, String::new(), None, false);
        photo.remote_attachments = vec!["image/jpeg".to_string(), "application/pdf".to_string()];
        let mut captioned = Message::mine(2, "look".to_string(), None, false);
        captioned.remote_attachments = vec!["image/png".to_string()];
        captioned.styles = vec![Styled {
            start: 0,
            end: 4,
            style: Style::Bold,
        }];
        discussion.messages = vec![photo, captioned];

        let first = discussion.first_attachment(0);
        assert_eq!(
            discussion.messages[0].display(Some(first)).0,
            "[image #1] [file #2]"
        );
        assert_eq!(discussion.messages[0].summary(), "[image] [file]");
        assert_eq!(discussion.messages[1].display(None).0, "look");
        let (text, styles) = discussion.messages[1].display(Some(discussion.first_attachment(1)));
        assert_eq!(text, "[image #3] look");
        assert_eq!(&text[styles[0].start..styles[0].end], "look");
        assert_eq!(discussion.next_attachment(), 4);
    }
}
