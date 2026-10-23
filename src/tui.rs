//! The terminal interface: discussion list, discussion view, compose and forward screens.
//!
//! [`App`] holds the state and draws it; [`App::handle_key`] turns key presses into
//! [`Action`]s, that [`crate::run`] carries out through the backend.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write, stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{Attribute, Color, Print, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::backend::Capabilities;
use crate::data::{self, Discussion, Folder, Message, MessageId, Styled};
use crate::editor::Editor;
use crate::find::{self, Found};
use crate::fuzzy;
use crate::graphics::Thumbnails;
use crate::recipients::{self, Contact};
use crate::settings::Settings;
use crate::ui;

/// What the caller has to do after a key press.
#[derive(Debug, PartialEq)]
pub enum Action {
    Quit,
    /// Discussion opened: its messages are now read.
    Opened(usize),
    /// Send `text` in `discussion`, quoting message `reply_to` of that discussion.
    Send {
        discussion: usize,
        text: String,
        styles: Vec<Styled>,
        reply_to: Option<usize>,
    },
    /// Send the text of message `msg` of discussion `from` to discussion `to`.
    Forward {
        from: usize,
        msg: usize,
        to: usize,
    },
    /// [`App::settings`] changed: save them.
    SettingsChanged,
    /// The discussions were pinned or unpinned ([`Discussion::pinned`]): save them.
    PinToggled(Vec<usize>),
    /// The pinned discussions were reordered ([`Discussion::pinned`]): save their order.
    PinsReordered,
    /// Set our reaction `emoji` to message `msg` of `discussion`, or remove it.
    React {
        discussion: usize,
        msg: usize,
        emoji: String,
        remove: bool,
    },
    /// Archive the discussions.
    Archive(Vec<usize>),
    /// Delete the discussions (confirmed).
    DeleteChat(Vec<usize>),
    /// Move the discussions back to the inbox, from their folder.
    Restore(Vec<usize>),
    /// Delete for good every chat of the trash (confirmed).
    PurgeTrash,
    /// Pick whom to start a chat with: the contacts are needed ([`App::new_chat`]).
    NewChat,
    /// Start the chat with the people at these numbers.
    StartChat(Vec<String>),
    /// Delete our message `msg` of `discussion` for everyone (confirmed).
    Delete {
        discussion: usize,
        msg: usize,
    },
    /// Paste an image from the system clipboard as a textual placeholder.
    PasteImageFromClipboard,
    /// Download the received attachments of message `msg` of `discussion`, then open them.
    FetchAttachments {
        discussion: usize,
        msg: usize,
    },
    /// Send message `msg` of `discussion` again, not sent: the new one replaces it.
    Resend {
        discussion: usize,
        msg: usize,
    },
    /// Stop sending message `msg` of `discussion` (it may have left already).
    CancelSend {
        discussion: usize,
        msg: usize,
    },
    /// Send a message with uploaded attachments.
    SendWithAttachments {
        discussion: usize,
        text: String,
        attachments: Vec<data::Attachment>,
        reply_to: Option<usize>,
    },
}

enum Screen {
    /// The chats. `selected` is a discussion index (here and in `Forward`).
    List { selected: usize, scroll: usize },
    Discussion {
        discussion_idx: usize,
        /// The selected message, or with no selection the one kept in view.
        selected_msg: usize,
        /// Whether a message is selected: from the first Up, until Esc.
        selecting: bool,
        scroll: usize,
        /// `Some` while the user is typing a new message.
        /// Boxed: the draft (with its formatting undo) is much larger than the other states.
        composing: Option<Box<Compose>>,
        /// `Some` while picking a reaction.
        reacting: Option<Picker>,
        /// Waiting for y/n to delete the selected message.
        deleting: bool,
        /// Index and number of the messages that were new when it was opened.
        new_from: Option<(usize, usize)>,
    },
    /// Picking the discussion to forward a message to.
    Forward {
        discussion_idx: usize,
        msg_idx: usize,
        selected: usize,
        scroll: usize,
    },
    /// The parameters, opened from the discussion list (`back_to` is its selection).
    Settings { selected: usize, back_to: usize },
    /// Finding chats and messages, from the chat list.
    Find(Finding),
    /// Picking whom to start a chat with, from the chat list.
    NewChat(Recipients),
}

/// The people a new chat is for, as picked.
#[derive(Debug, Clone)]
struct Recipients {
    /// The address book.
    contacts: Vec<Contact>,
    /// Picked already.
    picked: Vec<Contact>,
    /// A name or a number being typed.
    query: String,
    /// Index in [`Recipients::entries`].
    selected: usize,
    /// Whether the user moved among the entries: `Enter` then picks rather than starts.
    browsing: bool,
    scroll: usize,
    /// The chat list selection, to go back to.
    back_to: usize,
}

impl Recipients {
    /// What can be picked: the number typed, if it is one, then the contacts matching.
    fn entries(&self) -> Vec<Contact> {
        let typed = recipients::number(&self.query).map(|number| Contact {
            name: String::new(),
            number,
        });
        typed
            .into_iter()
            .chain(
                recipients::matching(&self.contacts, &self.query)
                    .into_iter()
                    .cloned(),
            )
            .collect()
    }

    /// Picks `contact` (once), and clears the query.
    fn pick(&mut self, contact: Contact) {
        if !self.picked.iter().any(|p| p.number == contact.number) {
            self.picked.push(contact);
        }
        self.query.clear();
        (self.selected, self.scroll, self.browsing) = (0, 0, false);
    }
}

/// A search of chats and messages, as typed.
#[derive(Debug, Clone)]
struct Finding {
    query: String,
    results: Vec<Found>,
    /// Index in `results`.
    selected: usize,
    scroll: usize,
    /// Whether the keys go to the results (actions on chats) rather than to the query.
    browsing: bool,
    /// The selection of the chat list, to go back to.
    back_to: usize,
}

/// A parameter shown on the parameters screen: label and the setting it toggles.
type Toggle = (&'static str, fn(&mut Settings) -> &mut bool);

/// The parameters shown, in order.
const SETTINGS: [Toggle; 1] = [("Desktop notifications for new messages", |s| {
    &mut s.notifications
})];

struct Compose {
    editor: Editor,
    attachments: Vec<data::Attachment>,
    /// Message being replied to, in the current discussion.
    reply_to: Option<usize>,
    /// `Some` while completing a `:shortcode`.
    completion: Option<Completion>,
}

/// Emoji completion of the `:shortcode` being typed, in a list like fzf's.
struct Completion {
    /// Byte index of its `:` in the draft.
    start: usize,
    /// Match under the cursor, 0 being the best (at the bottom).
    cursor: usize,
}

/// Matches shown by the completion list.
const COMPLETION_ROWS: usize = 10;

impl Compose {
    fn new(text: &str, reply_to: Option<usize>) -> Box<Self> {
        Box::new(Compose {
            editor: Editor::new(text),
            attachments: Vec::new(),
            reply_to,
            completion: None,
        })
    }

    /// The shortcode typed after the `:` of the completion, lowercased like the table.
    fn completion_filter(&self) -> Option<String> {
        let completion = self.completion.as_ref()?;
        Some(
            self.editor
                .text()
                .get(completion.start + 1..self.editor.cursor())?
                .to_ascii_lowercase(),
        )
    }

    /// Handles a key while completing; `false` when the key is for the editor instead
    /// (the completion is then over).
    fn complete(&mut self, key: KeyEvent) -> bool {
        let Some(filter) = self.completion_filter() else {
            return false;
        };
        let matches = fuzzy::emoji_matches(&filter);
        let Some(completion) = &mut self.completion else {
            return false;
        };
        let last = matches.len().saturating_sub(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Leaves the text as typed, `:` included.
            KeyCode::Esc => self.completion = None,
            KeyCode::Char('c' | 'g') if ctrl => self.completion = None,
            // The best match is at the bottom: up goes to the next ones, as in fzf.
            KeyCode::Up => completion.cursor = (completion.cursor + 1).min(last),
            KeyCode::Char('p' | 'k') if ctrl => {
                completion.cursor = (completion.cursor + 1).min(last)
            }
            KeyCode::Down => completion.cursor = completion.cursor.saturating_sub(1),
            KeyCode::Char('n' | 'j') if ctrl => {
                completion.cursor = completion.cursor.saturating_sub(1)
            }
            KeyCode::PageUp => completion.cursor = (completion.cursor + COMPLETION_ROWS).min(last),
            KeyCode::PageDown => {
                completion.cursor = completion.cursor.saturating_sub(COMPLETION_ROWS)
            }
            // Shift/Alt+Enter still adds a line (and ends the completion).
            KeyCode::Enter | KeyCode::Tab
                if !key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                if let Some(m) = matches.get(completion.cursor) {
                    let start = completion.start;
                    self.editor.replace_to_cursor(start, m.emoji);
                }
                self.completion = None;
            }
            KeyCode::Char(c)
                if is_shortcode_char(c) && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.insert(c.encode_utf8(&mut [0; 4]));
                completion.cursor = 0;
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                completion.cursor = 0;
                if self.editor.cursor() <= completion.start {
                    self.completion = None;
                }
            }
            _ => {
                self.completion = None;
                return false;
            }
        }
        true
    }

    /// After typing a `:`: starts a completion when it begins a word (not in `10:30`).
    fn maybe_start_completion(&mut self) {
        let before = &self.editor.text()[..self.editor.cursor()];
        let Some(rest) = before.strip_suffix(':') else {
            return;
        };
        if rest.chars().next_back().is_none_or(char::is_whitespace) {
            self.completion = Some(Completion {
                start: rest.len(),
                cursor: 0,
            });
        }
    }
}

fn is_shortcode_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_+-".contains(c)
}

/// The reactions Signal offers first, picked with 1 to 6.
const QUICK_REACTIONS: [&str; 6] = [
    "\u{2764}\u{fe0f}",
    "\u{1f44d}",
    "\u{1f44e}",
    "\u{1f602}",
    "\u{1f62e}",
    "\u{1f622}",
];

/// The reaction picker: the quick reactions, then once a shortcode is typed its matches
/// in a list like fzf's, with the one under the cursor (0 being the best, at the bottom).
#[derive(Default)]
struct Picker {
    filter: String,
    cursor: usize,
}

/// Without news, someone is no longer shown typing after this (as Signal clients do).
const TYPING_TIMEOUT: Duration = Duration::from_secs(15);

/// Where attachments are written to be opened.
fn attachments_dir() -> PathBuf {
    std::env::temp_dir().join("chat-tui")
}

/// Someone typing in a discussion, until `until`.
struct Typing {
    discussion: usize,
    author: String,
    name: String,
    until: Instant,
}

pub struct App {
    /// Name of the application, shown in bold above the chat list; none if empty.
    pub name: String,
    /// Version of the application, after its name; none if empty.
    pub version: String,
    pub discussions: Vec<Discussion>,
    pub settings: Settings,
    typing: Vec<Typing>,
    screen: Screen,
    /// One-shot feedback shown in the footer until the next key press.
    status: Option<String>,
    /// Text to put in the clipboard on the next draw.
    clipboard: Option<String>,
    /// Chats shown by a page of the chat list, as last drawn (PageUp / PageDown).
    list_page: usize,
    /// Scroll of the chat list when it was left: back to it, the view is the same.
    list_scroll: usize,
    /// Messages to jump by a page in a discussion, from the last draw (PageUp / PageDown).
    message_page: usize,
    /// Image thumbnails shown in the discussions (off unless the terminal supports them).
    pub thumbnails: Thumbnails,
    /// What the backend can do.
    pub capabilities: Capabilities,
    /// The chats whose deletion is asked to confirm, in the chat list.
    deleting_chat: Option<Vec<usize>>,
    /// The chats marked in the chat list (`Space`), for the next action on chats.
    marked: Vec<usize>,
    /// The search a chat was opened from, shown again when it is left.
    finding: Option<Finding>,
    /// Whether purging the trash is asked to confirm.
    purging: bool,
    /// The folder shown by the chat list; `None` for the inbox.
    folder: Option<Folder>,
}

impl App {
    pub fn new(discussions: Vec<Discussion>) -> Self {
        let first = entries(&discussions, None, true)
            .first()
            .copied()
            .unwrap_or(0);
        App {
            name: String::new(),
            version: String::new(),
            discussions,
            settings: Settings::default(),
            typing: Vec::new(),
            screen: Screen::List {
                selected: first,
                scroll: 0,
            },
            status: None,
            clipboard: None,
            list_page: 10,
            list_scroll: 0,
            message_page: 5,
            thumbnails: Thumbnails::default(),
            capabilities: Capabilities::default(),
            deleting_chat: None,
            marked: Vec::new(),
            finding: None,
            purging: false,
            folder: None,
        }
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    fn selected_message_with_quote(&self) -> Option<(&Message, Option<&Message>)> {
        let Screen::Discussion {
            discussion_idx,
            selected_msg,
            ..
        } = self.screen
        else {
            return None;
        };
        let messages = &self.discussions.get(discussion_idx)?.messages;
        let message = messages.get(selected_msg)?;
        let quoted = message.reply_to.and_then(|i| messages.get(i));
        Some((message, quoted))
    }

    fn attachment_path(message_id: &str, index: usize, attachment: &data::Attachment) -> PathBuf {
        // Ids come from the backend: only safe characters in the file name.
        let message_id: String = message_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c))
            .collect();
        let from_name = attachment
            .file_name
            .as_deref()
            .and_then(|name| Path::new(name).extension())
            .and_then(|ext| ext.to_str());
        let extension = match attachment.content_type.as_str() {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/png" => "png",
            _ => from_name.unwrap_or("bin"),
        };
        attachments_dir().join(format!("{message_id}-{index}.{extension}"))
    }

    fn write_openable_attachments(message: &Message) -> Vec<String> {
        if fs::create_dir_all(attachments_dir()).is_err() {
            return Vec::new();
        }
        message
            .attachments
            .iter()
            .enumerate()
            .filter_map(|(index, attachment)| {
                let path = Self::attachment_path(&message.id, index, attachment);
                fs::write(&path, &attachment.bytes).ok()?;
                Some(path.to_string_lossy().into_owned())
            })
            .collect()
    }

    fn open_targets(message: &Message, quoted: Option<&Message>) -> Vec<String> {
        let mut targets: Vec<String> = data::links(&message.text)
            .into_iter()
            .map(str::to_string)
            .collect();
        for link in quoted.map_or(Vec::new(), |q| data::links(&q.text)) {
            if !targets.iter().any(|target| target == link) {
                targets.push(link.to_string());
            }
        }
        targets.extend(Self::write_openable_attachments(message));
        if let Some(quoted) = quoted {
            targets.extend(Self::write_openable_attachments(quoted));
        }
        targets
    }

    /// Opens the links and attachments of the selected message (and of the one it quotes).
    pub fn open_selection(&mut self) {
        let targets = self
            .selected_message_with_quote()
            .map(|(message, quoted)| Self::open_targets(message, quoted))
            .unwrap_or_default();
        let failed = targets
            .iter()
            .filter(|target| open::that_detached(target).is_err())
            .count();
        self.status = Some(match (targets.len(), failed) {
            (0, _) => "No link or attachment in this message".to_string(),
            (n, 0) => format!("Opened {n} link(s)/attachment(s)"),
            (n, f) => format!("Could not open {f} of {n} link(s)/attachment(s)"),
        });
    }

    #[cfg(test)]
    fn open_targets_for_selection(&self) -> Vec<String> {
        self.selected_message_with_quote()
            .map(|(message, quoted)| Self::open_targets(message, quoted))
            .unwrap_or_default()
    }

    /// Adds a discussion at the end of the list and returns its index.
    pub fn push_discussion(&mut self, discussion: Discussion) -> usize {
        self.discussions.push(discussion);
        self.discussions.len() - 1
    }

    /// The title, kind, folder and visibility of `discussion` changed to those of `update`.
    /// Out of the list shown, it is no longer selected there (but stays shown while open).
    pub fn update_discussion(&mut self, discussion: usize, update: Discussion) {
        let before = self.shown_entries();
        let target = &mut self.discussions[discussion];
        target.title = update.title;
        target.is_group = update.is_group;
        target.hidden = update.hidden;
        target.folder = update.folder;
        self.keep_selection(&before);
    }

    /// Hides `discussion` (gone): no longer selected in the chat list, but still shown while
    /// open.
    pub fn hide_discussion(&mut self, discussion: usize) {
        let before = self.shown_entries();
        self.discussions[discussion].hidden = true;
        self.keep_selection(&before);
    }

    /// Moves `discussion` to `folder` (`None`: the inbox).
    pub fn file_discussion(&mut self, discussion: usize, folder: Option<Folder>) {
        let before = self.shown_entries();
        self.discussions[discussion].folder = folder;
        self.keep_selection(&before);
    }

    /// The chats of the trash.
    pub fn trash(&self) -> Vec<usize> {
        order(&self.discussions, Some(Folder::Trash))
    }

    /// The entries of the list shown (chats, then folders), if any.
    fn shown_entries(&self) -> Vec<usize> {
        match &self.screen {
            Screen::List { .. } => entries(&self.discussions, self.folder, true),
            Screen::Forward { .. } => entries(&self.discussions, None, false),
            _ => Vec::new(),
        }
    }

    /// A selection of the chat list gone from it (chat moved, archived, deleted) goes to the
    /// entry taking its place among those shown `before`, the next ones moving up by one
    /// (the last one when it was the last); an empty folder shown goes back to the inbox.
    fn keep_selection(&mut self, before: &[usize]) {
        if let Some(folder) = self.folder
            && order(&self.discussions, Some(folder)).is_empty()
        {
            self.folder = None;
            if let Screen::List { selected, .. } = &mut self.screen {
                *selected = folder_entry(folder);
            }
        }
        let (selected, entries) = match &mut self.screen {
            Screen::List { selected, .. } => {
                (selected, entries(&self.discussions, self.folder, true))
            }
            Screen::Forward { selected, .. } => (selected, entries(&self.discussions, None, false)),
            _ => return,
        };
        self.marked.retain(|m| entries.contains(m));
        if !entries.contains(selected)
            && let Some(&last) = entries.last()
        {
            let place = before.iter().position(|e| e == selected).unwrap_or(0);
            *selected = entries.get(place).copied().unwrap_or(last);
        }
    }

    /// `author` (named `name`) started or stopped typing in `discussion`.
    pub fn set_typing(
        &mut self,
        discussion: usize,
        author: &str,
        name: &str,
        started: bool,
        now: Instant,
    ) {
        self.typing
            .retain(|t| !(t.discussion == discussion && t.author == author));
        if started {
            let (author, name) = (author.to_string(), name.to_string());
            self.typing.push(Typing {
                discussion,
                author,
                name,
                until: now + TYPING_TIMEOUT,
            });
        }
    }

    /// When the next typing indicator expires, to redraw then.
    pub fn next_typing_expiry(&self) -> Option<Instant> {
        self.typing.iter().map(|t| t.until).min()
    }

    /// Drops the typing indicators expired at `now`.
    pub fn expire_typing(&mut self, now: Instant) {
        self.typing.retain(|t| t.until > now);
    }

    /// The chat being written to, and the text typed so far.
    pub fn draft(&self) -> Option<(&crate::data::ChatId, &str)> {
        match &self.screen {
            Screen::Discussion {
                discussion_idx,
                composing: Some(compose),
                ..
            } => Some((
                &self.discussions.get(*discussion_idx)?.id,
                compose.editor.text(),
            )),
            _ => None,
        }
    }

    /// Whether `discussion` is the one shown (also while forwarding from it).
    pub fn is_open(&self, discussion: usize) -> bool {
        matches!(self.screen, Screen::Discussion { discussion_idx, .. } if discussion_idx == discussion)
    }

    /// Removes message `msg` of `discussion`: the replies to it lose their quote, the
    /// selection stays on the same message (or the one after).
    pub fn remove_message(&mut self, discussion: usize, msg: usize) {
        let target = &mut self.discussions[discussion];
        if msg >= target.messages.len() {
            return;
        }
        target.messages.remove(msg);
        for m in &mut target.messages {
            m.reply_to = match m.reply_to {
                Some(i) if i == msg => None,
                Some(i) if i > msg => Some(i - 1),
                other => other,
            };
        }
        let last = target.messages.len().saturating_sub(1);
        if let Screen::Discussion {
            discussion_idx,
            selected_msg,
            ..
        } = &mut self.screen
            && *discussion_idx == discussion
        {
            if *selected_msg > msg {
                *selected_msg -= 1;
            }
            *selected_msg = (*selected_msg).min(last);
        }
    }

    /// Appends a message to a discussion. If that discussion is open, the selection follows
    /// the new message when it is ours (and is then hidden) or when the previous last message
    /// was selected; otherwise a received message marks it unread.
    /// Adds `message` to `discussion`; one it has already (same id: sent here and received
    /// back, updated…) is replaced, not counted as new.
    pub fn push_message(&mut self, discussion: usize, message: Message) {
        let open = self.is_open(discussion);
        let target = &mut self.discussions[discussion];
        if let Some(known) = target.position_of(&message.id) {
            target.messages[known] = message;
            return;
        }
        let was_last = target.messages.len().saturating_sub(1);
        let from_me = message.from_me;
        // Their message arrived: they are done typing it.
        if let Some(author) = &message.author {
            self.typing
                .retain(|t| !(t.discussion == discussion && t.author == *author));
        }
        target.messages.push(message);
        let last = target.messages.len() - 1;
        if !open && !from_me {
            target.unread += 1;
        }
        if let Screen::Discussion {
            discussion_idx,
            selected_msg,
            selecting,
            ..
        } = &mut self.screen
            && *discussion_idx == discussion
            && (from_me || *selected_msg == was_last)
        {
            *selected_msg = last;
            // Once ours is sent, back to reading the end of the discussion.
            *selecting &= !from_me;
        }
    }

    /// Merges into `discussion` its `history`, each message with the id of the one it quotes:
    /// messages not known yet are added, known ones replaced, all in send order. What points
    /// to a message (selection, quotes, reply being written) keeps pointing to it. Not
    /// counted as unread here (see [`crate::store::ReadMarks::unread_count`]).
    pub fn merge_history(&mut self, discussion: usize, history: Vec<(Message, Option<MessageId>)>) {
        let target = &mut self.discussions[discussion];
        let old_len = target.messages.len();
        let quote_of = |messages: &[Message], m: &Message| {
            m.reply_to
                .and_then(|i| messages.get(i))
                .map(|q| q.id.clone())
        };
        // (index before, message, id of the quoted one)
        let mut entries: Vec<(Option<usize>, Message, Option<MessageId>)> = target
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| (Some(i), m.clone(), quote_of(&target.messages, m)))
            .collect();
        let mut known: HashMap<MessageId, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.1.id.clone(), i))
            .collect();
        for (message, quote) in history {
            match known.get(&message.id) {
                Some(&i) => {
                    entries[i].1 = message;
                    entries[i].2 = quote;
                }
                None => {
                    known.insert(message.id.clone(), entries.len());
                    entries.push((None, message, quote));
                }
            }
        }
        entries.sort_by_key(|e| e.1.sent_ms);

        let mut moved = vec![0; old_len];
        for (new, entry) in entries.iter().enumerate() {
            if let Some(old) = entry.0 {
                moved[old] = new;
            }
        }
        let index: HashMap<MessageId, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.1.id.clone(), i))
            .collect();
        target.messages = entries
            .into_iter()
            .map(|(_, mut message, quote)| {
                message.reply_to = quote.and_then(|id| index.get(&id).copied());
                message
            })
            .collect();
        let last = target.messages.len().saturating_sub(1);
        let remap = |i: usize| moved.get(i).copied().unwrap_or(last);

        match &mut self.screen {
            Screen::Discussion {
                discussion_idx,
                selected_msg,
                selecting,
                composing,
                new_from,
                ..
            } if *discussion_idx == discussion => {
                // Reading the end: still the end.
                *selected_msg = if !*selecting && *selected_msg + 1 >= old_len {
                    last
                } else {
                    remap(*selected_msg)
                };
                if let Some((first, _)) = new_from {
                    *first = remap(*first);
                }
                if let Some(compose) = composing
                    && let Some(reply_to) = &mut compose.reply_to
                {
                    *reply_to = remap(*reply_to);
                }
            }
            Screen::Forward {
                discussion_idx,
                msg_idx,
                ..
            } if *discussion_idx == discussion => *msg_idx = remap(*msg_idx),
            _ => {}
        }
    }

    /// Opens discussion `idx`, on its first new message, below a "N new messages" line.
    pub fn open(&mut self, idx: usize) -> Action {
        if let Screen::List { scroll, .. } = self.screen {
            self.list_scroll = scroll;
        }
        let discussion = &mut self.discussions[idx];
        let new_from = discussion
            .first_unread()
            .map(|first| (first, discussion.unread));
        let selected_msg = new_from
            .map_or(discussion.messages.len().saturating_sub(1), |(first, _)| {
                first
            });
        discussion.unread = 0;
        self.screen = Screen::Discussion {
            discussion_idx: idx,
            selected_msg,
            selecting: false,
            scroll: 0,
            composing: None,
            reacting: None,
            deleting: false,
            new_from,
        };
        Action::Opened(idx)
    }

    /// Moves the pinned chat `chat` one place `up` (or down) among the pinned chats of the
    /// inbox: whether it moved.
    fn move_pinned(&mut self, chat: usize, up: bool) -> bool {
        if self.folder.is_some()
            || self
                .discussions
                .get(chat)
                .is_none_or(|d| d.pinned.is_none())
        {
            return false;
        }
        let pinned: Vec<usize> = order(&self.discussions, None)
            .into_iter()
            .filter(|&i| self.discussions[i].pinned.is_some())
            .collect();
        let Some(at) = pinned.iter().position(|&i| i == chat) else {
            return false;
        };
        let Some(&other) = (if up {
            at.checked_sub(1).and_then(|at| pinned.get(at))
        } else {
            pinned.get(at + 1)
        }) else {
            return false;
        };
        let rank = self.discussions[chat].pinned;
        self.discussions[chat].pinned = self.discussions[other].pinned;
        self.discussions[other].pinned = rank;
        true
    }

    /// `P` (pin), `a` (archive), `r` (restore) or `Del` (delete, once confirmed) on the
    /// chats marked among `list`, else on the `selected` one: `None` for another key, else
    /// the action to do, if any. Marks are cleared once acted on.
    fn chat_command(
        &mut self,
        code: KeyCode,
        list: &[usize],
        selected: usize,
    ) -> Option<Option<Action>> {
        let chats = targets(&self.marked, list, selected);
        let actions = self.capabilities.chat_actions;
        let folder_is = |pred: fn(Option<Folder>) -> bool| -> Vec<usize> {
            chats
                .iter()
                .copied()
                .filter(|&i| pred(self.discussions[i].folder))
                .collect()
        };
        let action = match code {
            KeyCode::Char('P') if !chats.is_empty() => {
                // Pinned unless all are: pinning again puts the chats at the end of the
                // pinned list.
                let pin = chats.iter().any(|&i| self.discussions[i].pinned.is_none());
                let mut next_rank = self
                    .discussions
                    .iter()
                    .filter_map(|d| d.pinned)
                    .max()
                    .map_or(0, |r| r + 1);
                for &i in &chats {
                    let discussion = &mut self.discussions[i];
                    if !pin {
                        discussion.pinned = None;
                    } else if discussion.pinned.is_none() {
                        discussion.pinned = Some(next_rank);
                        next_rank += 1;
                    }
                }
                Action::PinToggled(chats)
            }
            // Archived from the inbox, restored from a folder.
            KeyCode::Char('a') if actions && !folder_is(|f| f.is_none()).is_empty() => {
                Action::Archive(folder_is(|f| f.is_none()))
            }
            KeyCode::Char('r') if actions && !folder_is(|f| f.is_some()).is_empty() => {
                Action::Restore(folder_is(|f| f.is_some()))
            }
            KeyCode::Delete if actions && !chats.is_empty() => {
                self.deleting_chat = Some(chats);
                return Some(None);
            }
            _ => return None,
        };
        self.marked.clear();
        Some(Some(action))
    }

    /// A key on the search: typed in the query, until `Enter` (or `↓`) goes to the results;
    /// there, moving among them, acting on the chats found as in the chat list, opening one
    /// (`Enter`).
    fn find_key(&mut self, key: KeyEvent) -> Option<Action> {
        let Screen::Find(finding) = &mut self.screen else {
            return None;
        };
        let last = finding.results.len().saturating_sub(1);
        // The chats found and shown, and the one selected (none on a message).
        let chats: Vec<usize> = finding
            .results
            .iter()
            .filter_map(|f| match f {
                Found::Chat(i) if !self.discussions[*i].hidden => Some(*i),
                _ => None,
            })
            .collect();
        let selected_chat = match finding.results.get(finding.selected) {
            Some(Found::Chat(i)) => *i,
            _ => usize::MAX,
        };
        let typed = |key: &KeyEvent| {
            !key.modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        };
        match key.code {
            KeyCode::Esc if finding.browsing && !self.marked.is_empty() => self.marked.clear(),
            KeyCode::Esc => {
                self.screen = Screen::List {
                    selected: finding.back_to,
                    scroll: self.list_scroll,
                };
            }
            KeyCode::Up if finding.browsing && finding.selected == 0 => finding.browsing = false,
            KeyCode::Up => finding.selected = finding.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Enter if !finding.browsing => {
                finding.browsing = !finding.results.is_empty()
            }
            KeyCode::Down => finding.selected = (finding.selected + 1).min(last),
            KeyCode::PageUp => finding.selected = finding.selected.saturating_sub(self.list_page),
            KeyCode::PageDown => finding.selected = (finding.selected + self.list_page).min(last),
            KeyCode::Enter => {
                let found = finding.results.get(finding.selected)?.clone();
                self.finding = Some(finding.clone());
                let (chat, msg) = match found {
                    Found::Chat(chat) => (chat, None),
                    Found::Message(chat, id) => (chat, self.discussions[chat].position_of(&id)),
                };
                let action = self.open(chat);
                if let (
                    Some(msg),
                    Screen::Discussion {
                        selected_msg,
                        selecting,
                        ..
                    },
                ) = (msg, &mut self.screen)
                {
                    *selected_msg = msg;
                    *selecting = true;
                }
                return Some(action);
            }
            KeyCode::Char('/') if finding.browsing => finding.browsing = false,
            KeyCode::Char(' ') if finding.browsing => {
                if selected_chat != usize::MAX {
                    toggle(&mut self.marked, selected_chat);
                }
            }
            KeyCode::Char('P' | 'a' | 'r') | KeyCode::Delete if finding.browsing => {
                return self.chat_command(key.code, &chats, selected_chat).flatten();
            }
            KeyCode::Char(_) if finding.browsing => {}
            KeyCode::Backspace => {
                finding.browsing = false;
                finding.query.pop();
                finding.results = find::find(&self.discussions, &finding.query);
                (finding.selected, finding.scroll) = (0, 0);
            }
            KeyCode::Char(c) if typed(&key) => {
                finding.query.push(c);
                finding.results = find::find(&self.discussions, &finding.query);
                (finding.selected, finding.scroll) = (0, 0);
            }
            _ => {}
        }
        None
    }

    /// Opens the choice of whom to start a chat with, among `contacts`.
    pub fn new_chat(&mut self, contacts: Vec<Contact>) {
        let back_to = match self.screen {
            Screen::List { selected, .. } => selected,
            _ => 0,
        };
        self.screen = Screen::NewChat(Recipients {
            contacts,
            picked: Vec::new(),
            query: String::new(),
            selected: 0,
            browsing: false,
            scroll: 0,
            back_to,
        });
    }

    /// The chat `idx` was started: opened (from its folder), a message to type.
    pub fn started(&mut self, idx: usize) -> Action {
        self.folder = self.discussions[idx].folder;
        self.finding = None;
        let action = self.open(idx);
        if let Screen::Discussion { composing, .. } = &mut self.screen {
            *composing = Some(Compose::new("", None));
        }
        action
    }

    /// A key on the choice of whom to start a chat with: a name or a number typed, a
    /// contact picked from those matching (`↑`/`↓`, `Enter`), numbers one after the other
    /// (`,`); `Enter` again starts the chat.
    fn new_chat_key(&mut self, key: KeyEvent) -> Option<Action> {
        let Screen::NewChat(new) = &mut self.screen else {
            return None;
        };
        let entries = new.entries();
        let last = entries.len().saturating_sub(1);
        let typed = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::List {
                    selected: new.back_to,
                    scroll: self.list_scroll,
                };
            }
            KeyCode::Up => {
                new.selected = new.selected.saturating_sub(1);
                new.browsing = true;
            }
            KeyCode::Down => {
                new.selected = (new.selected + 1).min(last);
                new.browsing = true;
            }
            KeyCode::PageUp => {
                new.selected = new.selected.saturating_sub(self.list_page);
                new.browsing = true;
            }
            KeyCode::PageDown => {
                new.selected = (new.selected + self.list_page).min(last);
                new.browsing = true;
            }
            KeyCode::Enter if new.query.trim().is_empty() && !new.browsing => {
                if new.picked.is_empty() {
                    self.status = Some("Type a name or a number first".to_string());
                    return None;
                }
                let numbers = new.picked.iter().map(|p| p.number.clone()).collect();
                return Some(Action::StartChat(numbers));
            }
            KeyCode::Enter => match entries.into_iter().nth(new.selected) {
                Some(contact) => new.pick(contact),
                None => self.status = Some("No contact matches, nor a number".to_string()),
            },
            KeyCode::Char(',' | ';') if typed => {
                if let Some(number) = recipients::number(&new.query) {
                    new.pick(Contact {
                        name: String::new(),
                        number,
                    });
                }
            }
            KeyCode::Backspace => {
                if new.query.pop().is_none() {
                    new.picked.pop();
                }
                (new.selected, new.scroll, new.browsing) = (0, 0, false);
            }
            KeyCode::Char(c) if typed => {
                new.query.push(c);
                (new.selected, new.scroll, new.browsing) = (0, 0, false);
            }
            _ => {}
        }
        None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.status = None;
        if let Some(chats) = self.deleting_chat.take() {
            if key.code == KeyCode::Char('y') {
                self.marked.clear();
                return Some(Action::DeleteChat(chats));
            }
            self.status = Some("Not deleted".to_string());
            return None;
        }
        if std::mem::take(&mut self.purging) {
            if key.code == KeyCode::Char('y') {
                return Some(Action::PurgeTrash);
            }
            self.status = Some("Not purged".to_string());
            return None;
        }
        if matches!(self.screen, Screen::Find(_)) {
            return self.find_key(key);
        }
        if matches!(self.screen, Screen::NewChat(_)) {
            return self.new_chat_key(key);
        }
        if let Screen::List { selected, scroll } = &mut self.screen
            && key.code == KeyCode::Enter
        {
            if *selected < self.discussions.len() {
                let selected = *selected;
                return Some(self.open(selected));
            }
            if let Some(folder) = entry_folder(*selected) {
                self.marked.clear();
                self.folder = Some(folder);
                *selected = order(&self.discussions, self.folder)
                    .first()
                    .copied()
                    .unwrap_or(0);
                *scroll = 0;
            }
            return None;
        }
        let discussions = &self.discussions;
        let list = entries(discussions, self.folder, true);
        let forward_list = entries(discussions, None, false);

        match &mut self.screen {
            Screen::List { selected, scroll } => match key.code {
                // A pinned chat moved among the pinned ones.
                KeyCode::Up | KeyCode::Down
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    let selected = *selected;
                    let up = key.code == KeyCode::Up;
                    return self
                        .move_pinned(selected, up)
                        .then_some(Action::PinsReordered);
                }
                KeyCode::Up => *selected = step(&list, *selected, -1),
                KeyCode::Down => *selected = step(&list, *selected, 1),
                KeyCode::PageUp => *selected = step(&list, *selected, -(self.list_page as isize)),
                KeyCode::PageDown => *selected = step(&list, *selected, self.list_page as isize),
                KeyCode::Char('/') => {
                    self.list_scroll = *scroll;
                    self.screen = Screen::Find(Finding {
                        query: String::new(),
                        results: Vec::new(),
                        selected: 0,
                        scroll: 0,
                        browsing: false,
                        back_to: *selected,
                    });
                }
                KeyCode::Char(' ') if *selected < discussions.len() => {
                    toggle(&mut self.marked, *selected)
                }
                KeyCode::Char('P' | 'a' | 'r') | KeyCode::Delete => {
                    let selected = *selected;
                    return self.chat_command(key.code, &list, selected).flatten();
                }
                KeyCode::Char('X')
                    if self.capabilities.chat_actions && self.folder == Some(Folder::Trash) =>
                {
                    self.purging = true;
                }
                KeyCode::Char('n') if self.capabilities.new_chat && self.folder.is_none() => {
                    self.list_scroll = *scroll;
                    return Some(Action::NewChat);
                }
                KeyCode::Char('p') => {
                    let back_to = *selected;
                    self.list_scroll = *scroll;
                    self.screen = Screen::Settings {
                        selected: 0,
                        back_to,
                    };
                }
                KeyCode::Esc if !self.marked.is_empty() => self.marked.clear(),
                KeyCode::Esc if self.folder.is_some() => {
                    *selected = self.folder.take().map_or(0, folder_entry);
                    *scroll = 0;
                }
                KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Quit),
                _ => {}
            },
            Screen::Discussion {
                discussion_idx,
                selected_msg,
                selecting,
                composing,
                reacting,
                deleting,
                ..
            } => {
                let messages = &discussions[*discussion_idx].messages;
                if *deleting {
                    *deleting = false;
                    if key.code == KeyCode::Char('y') {
                        return Some(Action::Delete {
                            discussion: *discussion_idx,
                            msg: *selected_msg,
                        });
                    }
                    self.status = Some("Not deleted".to_string());
                    return None;
                }
                if let Some(picker) = reacting {
                    // Browsing only goes through the matches of a typed shortcode.
                    let matches = if picker.filter.is_empty() {
                        Vec::new()
                    } else {
                        fuzzy::emoji_matches(&picker.filter)
                    };
                    let last = matches.len().saturating_sub(1);
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let pick = match key.code {
                        KeyCode::Esc => {
                            *reacting = None;
                            None
                        }
                        KeyCode::Char('c' | 'g') if ctrl => {
                            *reacting = None;
                            None
                        }
                        KeyCode::Char(c @ '1'..='6') if picker.filter.is_empty() => {
                            Some(QUICK_REACTIONS[c as usize - '1' as usize])
                        }
                        // The best match is at the bottom: up goes to the next ones, as in fzf.
                        KeyCode::Up => {
                            picker.cursor = (picker.cursor + 1).min(last);
                            None
                        }
                        KeyCode::Char('p' | 'k') if ctrl => {
                            picker.cursor = (picker.cursor + 1).min(last);
                            None
                        }
                        KeyCode::Down => {
                            picker.cursor = picker.cursor.saturating_sub(1);
                            None
                        }
                        KeyCode::Char('n' | 'j') if ctrl => {
                            picker.cursor = picker.cursor.saturating_sub(1);
                            None
                        }
                        KeyCode::PageUp => {
                            picker.cursor = (picker.cursor + COMPLETION_ROWS).min(last);
                            None
                        }
                        KeyCode::PageDown => {
                            picker.cursor = picker.cursor.saturating_sub(COMPLETION_ROWS);
                            None
                        }
                        KeyCode::Char(c) if is_shortcode_char(c) && !ctrl => {
                            picker.filter.push(c.to_ascii_lowercase());
                            picker.cursor = 0;
                            None
                        }
                        KeyCode::Backspace => {
                            picker.filter.pop();
                            picker.cursor = 0;
                            None
                        }
                        KeyCode::Enter | KeyCode::Tab => {
                            matches.get(picker.cursor).map(|m| m.emoji)
                        }
                        _ => None,
                    };
                    let emoji = pick?;
                    // Picking our current reaction again removes it.
                    let remove = messages[*selected_msg].my_reaction() == Some(emoji);
                    *reacting = None;
                    return Some(Action::React {
                        discussion: *discussion_idx,
                        msg: *selected_msg,
                        emoji: emoji.to_string(),
                        remove,
                    });
                }
                if let Some(compose) = composing {
                    if compose.complete(key) {
                        return None;
                    }
                    let editor = &mut compose.editor;
                    match key.code {
                        // Esc first undoes the formatting just applied, then closes the draft.
                        KeyCode::Esc => {
                            if !editor.undo_format() {
                                *composing = None
                            }
                        }
                        // Shift+Enter needs the kitty keyboard protocol; Alt+Enter works everywhere.
                        KeyCode::Enter
                            if key
                                .modifiers
                                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                        {
                            editor.insert("\n")
                        }
                        KeyCode::Enter => {
                            let (text, styles) = data::trim_styled(editor.text(), editor.styles());
                            if !compose.attachments.is_empty() {
                                // The labels stand for the attachments: shown, not sent.
                                let first = self.discussions[*discussion_idx].next_attachment();
                                let text = compose.attachments.iter().enumerate().fold(
                                    text,
                                    |text, (i, attachment)| {
                                        let label = data::attachment_label(
                                            &attachment.content_type,
                                            Some(first + i),
                                        );
                                        text.replace(&label, "")
                                    },
                                );
                                let action = Action::SendWithAttachments {
                                    discussion: *discussion_idx,
                                    text: text.trim().to_string(),
                                    attachments: compose.attachments.clone(),
                                    reply_to: compose.reply_to,
                                };
                                *composing = None;
                                return Some(action);
                            }
                            if !text.is_empty() {
                                let action = Action::Send {
                                    discussion: *discussion_idx,
                                    text,
                                    styles,
                                    reply_to: compose.reply_to,
                                };
                                *composing = None;
                                return Some(action);
                            }
                        }
                        KeyCode::Backspace => editor.backspace(),
                        KeyCode::Delete => editor.delete(),
                        // Word by word with Alt (macOS habit) or Ctrl (Linux, Windows).
                        KeyCode::Left
                            if key
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                        {
                            editor.word_left()
                        }
                        KeyCode::Right
                            if key
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                        {
                            editor.word_right()
                        }
                        KeyCode::Left => editor.left(),
                        KeyCode::Right => editor.right(),
                        KeyCode::Up => editor.up(),
                        KeyCode::Down => editor.down(),
                        KeyCode::Home => editor.home(),
                        KeyCode::End => editor.end(),
                        KeyCode::Char('v' | 'V')
                            if key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                        {
                            return Some(Action::PasteImageFromClipboard);
                        }
                        // Ctrl+letter is a shortcut, not text (Ctrl+C must not type a "c").
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            editor.insert(c.encode_utf8(&mut [0; 4]));
                            if c == ':' {
                                compose.maybe_start_completion();
                            }
                        }
                        _ => {}
                    }
                } else {
                    match key.code {
                        // The first Up selects the message in view, at the bottom.
                        KeyCode::Up if !*selecting => *selecting = true,
                        KeyCode::Up => *selected_msg = selected_msg.saturating_sub(1),
                        // With no selection, Down, PageUp and PageDown scroll.
                        KeyCode::Down => {
                            if *selected_msg + 1 < messages.len() {
                                *selected_msg += 1;
                            }
                        }
                        KeyCode::PageUp => {
                            *selected_msg = selected_msg.saturating_sub(self.message_page)
                        }
                        KeyCode::PageDown => {
                            *selected_msg = (*selected_msg + self.message_page)
                                .min(messages.len().saturating_sub(1))
                        }
                        KeyCode::Enter => *composing = Some(Compose::new("", None)),
                        KeyCode::Esc if *selecting => *selecting = false,
                        KeyCode::Esc => {
                            let back_to = *discussion_idx;
                            self.screen = match self.finding.take() {
                                Some(finding) => Screen::Find(finding),
                                None => Screen::List {
                                    selected: back_to,
                                    scroll: self.list_scroll,
                                },
                            };
                        }
                        KeyCode::Char('q') => return Some(Action::Quit),
                        // Actions need a selected message, and there is nothing to act on in a
                        // deleted one. With no selection, their letters start a new message.
                        KeyCode::Delete if !*selecting => {}
                        KeyCode::Char('c')
                            if *selecting
                                && messages.get(*selected_msg).is_some_and(|m| m.sending) =>
                        {
                            return Some(Action::CancelSend {
                                discussion: *discussion_idx,
                                msg: *selected_msg,
                            });
                        }
                        // Nor in one being sent: it has no id yet.
                        KeyCode::Char('r' | 'f' | 'c' | 'o' | 'e' | 's') | KeyCode::Delete
                            if *selecting
                                && messages
                                    .get(*selected_msg)
                                    .is_none_or(|m| m.deleted || m.sending) => {}
                        KeyCode::Delete if *selecting && self.capabilities.delete_for_me => {
                            *deleting = true;
                        }
                        KeyCode::Delete if *selecting => {
                            let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                            self.status = match self.capabilities.delete_max_age {
                                None => Some("Messages cannot be deleted here".to_string()),
                                Some(max_age) => {
                                    let max_age_ms = max_age.as_millis() as u64;
                                    if messages[*selected_msg].deletable(now_ms, max_age_ms) {
                                        *deleting = true;
                                        None
                                    } else {
                                        Some(format!(
                                            "Only your messages of the last {} h can be deleted",
                                            max_age.as_secs() / 3600
                                        ))
                                    }
                                }
                            };
                        }
                        KeyCode::Char('s') if *selecting && messages[*selected_msg].failed => {
                            return Some(Action::Resend {
                                discussion: *discussion_idx,
                                msg: *selected_msg,
                            });
                        }
                        KeyCode::Char('e') if *selecting => *reacting = Some(Picker::default()),
                        KeyCode::Char('r') if *selecting => {
                            *composing = Some(Compose::new("", Some(*selected_msg)))
                        }
                        KeyCode::Char('f') if *selecting => {
                            let (discussion_idx, msg_idx) = (*discussion_idx, *selected_msg);
                            self.screen = Screen::Forward {
                                discussion_idx,
                                msg_idx,
                                selected: discussion_idx,
                                scroll: 0,
                            };
                        }
                        KeyCode::Char('c') if *selecting => {
                            self.clipboard = Some(messages[*selected_msg].text.clone());
                            self.status = Some("Message copied".to_string());
                        }
                        KeyCode::Char('o') if *selecting => {
                            if !messages[*selected_msg].remote_attachments.is_empty() {
                                return Some(Action::FetchAttachments {
                                    discussion: *discussion_idx,
                                    msg: *selected_msg,
                                });
                            }
                            self.open_selection();
                        }
                        KeyCode::Char('v' | 'V')
                            if key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                        {
                            return Some(Action::PasteImageFromClipboard);
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            *composing = Some(Compose::new(c.encode_utf8(&mut [0; 4]), None))
                        }
                        _ => {}
                    }
                }
            }
            Screen::Forward {
                discussion_idx,
                msg_idx,
                selected,
                ..
            } => match key.code {
                KeyCode::Up => *selected = step(&forward_list, *selected, -1),
                KeyCode::Down => *selected = step(&forward_list, *selected, 1),
                KeyCode::PageUp => {
                    *selected = step(&forward_list, *selected, -(self.list_page as isize))
                }
                KeyCode::PageDown => {
                    *selected = step(&forward_list, *selected, self.list_page as isize)
                }
                KeyCode::Enter => {
                    let action = Action::Forward {
                        from: *discussion_idx,
                        msg: *msg_idx,
                        to: *selected,
                    };
                    let (discussion_idx, selected_msg) = (*discussion_idx, *msg_idx);
                    self.screen = Screen::Discussion {
                        discussion_idx,
                        selected_msg,
                        selecting: true,
                        scroll: 0,
                        composing: None,
                        reacting: None,
                        deleting: false,
                        new_from: None,
                    };
                    return Some(action);
                }
                KeyCode::Esc => {
                    let (discussion_idx, selected_msg) = (*discussion_idx, *msg_idx);
                    self.screen = Screen::Discussion {
                        discussion_idx,
                        selected_msg,
                        selecting: true,
                        scroll: 0,
                        composing: None,
                        reacting: None,
                        deleting: false,
                        new_from: None,
                    };
                }
                _ => {}
            },
            Screen::Settings { selected, back_to } => match key.code {
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(SETTINGS.len() - 1),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let value = (SETTINGS[*selected].1)(&mut self.settings);
                    *value = !*value;
                    return Some(Action::SettingsChanged);
                }
                KeyCode::Esc | KeyCode::Char('p') => {
                    let selected = *back_to;
                    self.screen = Screen::List {
                        selected,
                        scroll: self.list_scroll,
                    };
                }
                KeyCode::Char('q') => return Some(Action::Quit),
                _ => {}
            },
            // Their keys go to `find_key` and `new_chat_key`.
            Screen::Find(_) | Screen::NewChat(_) => {}
        }
        None
    }

    /// Pasted text (Ctrl+Shift+V, middle click…) goes into the message being typed, as is:
    /// its line breaks stay line breaks instead of sending the message. In a discussion
    /// that is not being written to, it starts a new message.
    pub fn handle_paste(&mut self, text: &str) {
        self.status = None;
        if let Screen::Discussion { composing, .. } = &mut self.screen {
            match composing {
                Some(compose) => {
                    compose.completion = None;
                    compose.editor.insert(text)
                }
                None => *composing = Some(Compose::new(text, None)),
            }
        }
    }

    /// Pasted images are displayed as a placeholder while the binary payload is kept for sending.
    pub fn handle_pasted_image(&mut self, attachment: data::Attachment) {
        self.status = None;
        if let Screen::Discussion {
            discussion_idx,
            composing,
            ..
        } = &mut self.screen
        {
            if composing.is_none() {
                *composing = Some(Compose::new("", None));
            }
            if let Some(compose) = composing {
                compose.completion = None;
                let number =
                    self.discussions[*discussion_idx].next_attachment() + compose.attachments.len();
                compose.editor.insert(&data::attachment_label(
                    &attachment.content_type,
                    Some(number),
                ));
                compose.attachments.push(attachment);
            }
        }
    }

    /// Draws the current screen on a `w` x `h` terminal.
    pub fn draw(&mut self, out: &mut impl Write, w: u16, h: u16) -> io::Result<()> {
        if let Some(text) = self.clipboard.take() {
            copy_to_clipboard(out, &text)?;
        }
        let typing: Vec<usize> = self.typing.iter().map(|t| t.discussion).collect();
        self.list_page = (h as usize).saturating_sub(FOOTER).max(1);
        let actions = self.capabilities.chat_actions;
        let for_good = match &self.deleting_chat {
            Some(chats)
                if chats
                    .iter()
                    .all(|&i| self.discussions[i].folder == Some(Folder::Trash)) =>
            {
                " for good"
            }
            _ => "",
        };
        let marked = self.marked.len();
        let list_footer = match (&self.deleting_chat, &self.status, self.folder) {
            (Some(chats), ..) if chats.len() == 1 => format!(
                "Delete \u{201c}{}\u{201d}{for_good}?   y yes   any other key: no",
                self.discussions[chats[0]].title,
            ),
            (Some(chats), ..) => format!(
                "Delete the {} marked chats{for_good}?   y yes   any other key: no",
                chats.len(),
            ),
            (None, ..) if self.purging => format!(
                "Purge the {} chats of the trash for good?   y yes   any other key: no",
                self.trash().len()
            ),
            (None, Some(status), _) => status.clone(),
            (None, None, Some(_)) if actions && marked > 0 => format!(
                "{marked} marked   Space mark/unmark   r restore   Del delete   \
                 Esc unmark"
            ),
            (None, None, None) if marked > 0 => format!(
                "{marked} marked   Space mark/unmark   P pin{}   Esc unmark",
                if actions {
                    "   a archive   Del delete"
                } else {
                    ""
                }
            ),
            (None, None, Some(Folder::Trash)) if actions => "\u{2191}/\u{2193} select   \
                 Enter open   Space mark   r restore   Del delete   X purge   Esc back   q quit"
                .to_string(),
            (None, None, Some(_)) if actions => "\u{2191}/\u{2193} select   Enter open   \
                 Space mark   r restore   Del delete   Esc back   q quit"
                .to_string(),
            (None, None, Some(_)) => {
                "\u{2191}/\u{2193} select   Enter open   Esc back   q quit".to_string()
            }
            (None, None, None) if actions && self.capabilities.new_chat => {
                "\u{2191}/\u{2193} select   Enter open   n new   / find   Space mark   P pin   \
                 a archive   Del delete   p parameters   q quit"
                    .to_string()
            }
            (None, None, None) if actions => "\u{2191}/\u{2193} select   Enter open   / find   \
                 Space mark   P pin   a archive   Del delete   p parameters   q quit"
                .to_string(),
            (None, None, None) => "\u{2191}/\u{2193} select   Enter open   / find   Space mark   \
                 P pin   \
                 p parameters   q quit"
                .to_string(),
        };
        let find_footer = match (&self.screen, &self.deleting_chat, &self.status) {
            (_, Some(_), _) | (_, None, Some(_)) => list_footer.clone(),
            (Screen::Find(f), ..) if f.browsing && marked > 0 => format!(
                "{marked} marked   Space mark/unmark   P pin   a archive   r restore   \
                 Del delete   Esc unmark"
            ),
            (Screen::Find(f), ..) if f.browsing => "\u{2191}/\u{2193} select   Enter open   \
                 Space mark   P pin   a archive   r restore   Del delete   / edit   Esc back"
                .to_string(),
            (Screen::Find(f), ..) if !f.query.trim().is_empty() && f.results.is_empty() => {
                "Nothing found   Esc back".to_string()
            }
            _ => "type to find   Enter results   Esc back".to_string(),
        };
        match &mut self.screen {
            Screen::List { selected, scroll } => draw_list(
                out,
                &self.discussions,
                &typing,
                &self.marked,
                Some((self.name.as_str(), self.version.as_str()))
                    .filter(|(name, _)| !name.is_empty()),
                None,
                self.folder,
                true,
                &list_footer,
                *selected,
                scroll,
                w,
                h,
            ),
            Screen::Discussion {
                discussion_idx,
                selected_msg,
                selecting,
                scroll,
                composing,
                reacting,
                deleting,
                new_from,
            } => {
                self.thumbnails
                    .send(out, &self.discussions[*discussion_idx].messages)?;
                let visible = draw_discussion(
                    out,
                    &self.discussions[*discussion_idx],
                    &self.thumbnails,
                    *new_from,
                    &self
                        .typing
                        .iter()
                        .filter(|t| t.discussion == *discussion_idx)
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>(),
                    *selected_msg,
                    *selecting,
                    scroll,
                    composing.as_deref(),
                    reacting.as_ref(),
                    deleting.then_some(if self.capabilities.delete_for_me {
                        "for you"
                    } else {
                        "for everyone"
                    }),
                    self.status.as_deref(),
                    w,
                    h,
                )?;
                // A page keeps one message of context.
                self.message_page = visible.saturating_sub(1).max(1);
                Ok(())
            }
            Screen::Forward {
                selected, scroll, ..
            } => draw_list(
                out,
                &self.discussions,
                &typing,
                &[],
                None,
                Some("Forward to\u{2026}"),
                None,
                false,
                "\u{2191}/\u{2193} select chat   Enter forward   Esc cancel",
                *selected,
                scroll,
                w,
                h,
            ),
            Screen::Settings { selected, .. } => draw_settings(
                out,
                &mut self.settings,
                *selected,
                self.status.as_deref(),
                w,
                h,
            ),
            Screen::Find(finding) => draw_find(
                out,
                &self.discussions,
                &self.marked,
                finding,
                &find_footer,
                w,
                h,
            ),
            Screen::NewChat(new) => {
                let help = match &self.status {
                    Some(status) => status.clone(),
                    None if new.query.trim().is_empty()
                        && !new.browsing
                        && !new.picked.is_empty() =>
                    {
                        "Enter start the chat   type to add someone   Backspace remove   Esc back"
                            .to_string()
                    }
                    None if !new.query.trim().is_empty() && new.entries().is_empty() => {
                        "No contact matches: type a number   Esc back".to_string()
                    }
                    None => "type a name or a number   \u{2191}/\u{2193} select   Enter add   \
                             , next number   Esc back"
                        .to_string(),
                };
                draw_new_chat(out, new, &help, w, h)
            }
        }
    }
}

/// Puts the terminal in full-screen raw mode, and restores it when dropped (also on panic).
pub struct TerminalGuard {
    enhanced_keys: bool,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, cursor::Hide)?;
        // Lets the terminal report Shift+Enter distinctly from Enter (kitty keyboard protocol).
        let enhanced_keys = terminal::supports_keyboard_enhancement().unwrap_or(false);
        if enhanced_keys {
            execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        // Pastes arrive as one event, so their line breaks do not act as Enter.
        execute!(out, EnableBracketedPaste)?;
        Ok(TerminalGuard { enhanced_keys })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, DisableBracketedPaste);
        if self.enhanced_keys {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Copies `text` through the terminal (OSC 52), which also works over SSH.
fn copy_to_clipboard(out: &mut impl Write, text: &str) -> io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    write!(out, "\x1b]52;c;{encoded}\x07")
}

fn print_row(out: &mut impl Write, y: u16, text: &str, selected: bool) -> io::Result<()> {
    print_styled(out, y, text, &[], selected)
}

/// Prints `text` on row `y`, its `styles` as terminal attributes (bold, italic, crossed out).
fn print_styled(
    out: &mut impl Write,
    y: u16,
    text: &str,
    styles: &[Styled],
    selected: bool,
) -> io::Result<()> {
    print_line(out, y, text, styles, selected, false)
}

/// [`print_styled`], in red if `alert`.
fn print_line(
    out: &mut impl Write,
    y: u16,
    text: &str,
    styles: &[Styled],
    selected: bool,
    alert: bool,
) -> io::Result<()> {
    queue!(out, cursor::MoveTo(0, y))?;
    let mut cuts: Vec<usize> = styles
        .iter()
        .flat_map(|s| [s.start, s.end])
        .filter(|&at| at < text.len())
        .collect();
    cuts.extend([0, text.len()]);
    cuts.sort_unstable();
    cuts.dedup();
    for part in cuts.windows(2) {
        let (start, end) = (part[0], part[1]);
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        queue!(out, SetAttribute(Attribute::Reset))?;
        if alert {
            queue!(out, SetForegroundColor(Color::Red))?;
        }
        if selected {
            queue!(out, SetAttribute(Attribute::Reverse))?;
        }
        for styled in styles.iter().filter(|s| s.start <= start && end <= s.end) {
            let attribute = match styled.style {
                data::Style::Bold => Attribute::Bold,
                data::Style::Italic => Attribute::Italic,
                data::Style::Strikethrough => Attribute::CrossedOut,
            };
            queue!(out, SetAttribute(attribute))?;
        }
        queue!(out, Print(&text[start..end]))?;
    }
    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

const HEADER: usize = 2;
const FOOTER: usize = 1;

/// Indices of the discussions in `folder` (`None`: the inbox) in display order: pinned
/// first (in pin order), then the others.
fn order(discussions: &[Discussion], folder: Option<Folder>) -> Vec<usize> {
    let (mut pinned, others): (Vec<usize>, Vec<usize>) = (0..discussions.len())
        .filter(|&i| !discussions[i].hidden && discussions[i].folder == folder)
        .partition(|&i| discussions[i].pinned.is_some());
    pinned.sort_by_key(|&i| discussions[i].pinned);
    pinned.into_iter().chain(others).collect()
}

/// The chats an action is for: those `marked`, in the order of the `list`, else the
/// `selected` one if a chat.
fn targets(marked: &[usize], list: &[usize], selected: usize) -> Vec<usize> {
    if marked.is_empty() {
        list.iter()
            .copied()
            .filter(|&e| e == selected && entry_folder(e).is_none())
            .collect()
    } else {
        list.iter()
            .copied()
            .filter(|e| marked.contains(e))
            .collect()
    }
}

/// Marks `chat`, or unmarks it.
fn toggle(marked: &mut Vec<usize>, chat: usize) {
    match marked.iter().position(|&m| m == chat) {
        Some(i) => _ = marked.remove(i),
        None => marked.push(chat),
    }
}

/// The folders holding chats, with their number of chats.
fn folders(discussions: &[Discussion]) -> Vec<(Folder, usize)> {
    Folder::ALL
        .into_iter()
        .map(|f| {
            (
                f,
                discussions
                    .iter()
                    .filter(|d| !d.hidden && d.folder == Some(f))
                    .count(),
            )
        })
        .filter(|&(_, n)| n > 0)
        .collect()
}

/// The entry of the chat list leading to `folder`: out of the discussion indices.
fn folder_entry(folder: Folder) -> usize {
    usize::MAX - folder as usize
}

/// The folder an entry of the chat list leads to, if it is one.
fn entry_folder(entry: usize) -> Option<Folder> {
    Folder::ALL.into_iter().find(|&f| folder_entry(f) == entry)
}

/// The entries of the chat list showing `folder` (`None`: the inbox, followed by the
/// folders if `with_folders`): discussion indices, and folder entries.
fn entries(discussions: &[Discussion], folder: Option<Folder>, with_folders: bool) -> Vec<usize> {
    let mut entries = order(discussions, folder);
    if folder.is_none() && with_folders {
        entries.extend(
            folders(discussions)
                .into_iter()
                .map(|(f, _)| folder_entry(f)),
        );
    }
    entries
}

/// `n` new messages: a circled number from ① to ⑳, digits beyond ("21", "99+"), nothing
/// for none. Not ㉑ and above: they take two columns where ① to ⑳ take one, and would
/// stick out on the left.
fn unread_badge(n: usize) -> String {
    match n {
        0 => String::new(),
        1..=20 => char::from_u32(0x2460 + n as u32 - 1).map_or_else(|| n.to_string(), String::from),
        21..=99 => n.to_string(),
        _ => "99+".to_string(),
    }
}

/// The entry `delta` places away from `selected` among `entries` (clamped).
fn step(entries: &[usize], selected: usize, delta: isize) -> usize {
    let Some(pos) = entries.iter().position(|&i| i == selected) else {
        return entries.first().copied().unwrap_or(0);
    };
    entries[pos.saturating_add_signed(delta).min(entries.len() - 1)]
}

/// A line of the chat list.
enum Row<'a> {
    /// The name and version of the application, above the sections.
    Heading(&'a str, &'a str),
    Title(&'a str),
    Blank,
    Chat(usize),
    /// A folder and its number of chats.
    Folder(Folder, usize),
}

/// The list lines: the `heading` if any, then the chats of `folder` under its name, or
/// those of the inbox in one `title` section, or "Pinned" and "Chats" sections (only
/// "Chats" when nothing is pinned), followed by the folders if `with_folders`.
fn list_rows<'a>(
    discussions: &[Discussion],
    heading: Option<(&'a str, &'a str)>,
    title: Option<&'a str>,
    folder: Option<Folder>,
    with_folders: bool,
) -> Vec<Row<'a>> {
    let order = order(discussions, folder);
    let pinned_count = order
        .iter()
        .filter(|&&i| discussions[i].pinned.is_some())
        .count();
    let mut rows: Vec<_> = heading
        .map(|(name, version)| Row::Heading(name, version))
        .into_iter()
        .collect();
    let section = |rows: &mut Vec<Row<'a>>, title: &'a str, chats: &[usize]| {
        if !rows.is_empty() {
            rows.push(Row::Blank);
        }
        rows.push(Row::Title(title));
        rows.push(Row::Blank);
        rows.extend(chats.iter().map(|&i| Row::Chat(i)));
    };
    match (folder, title) {
        (Some(folder), _) => section(&mut rows, folder.name(), &order),
        (None, Some(title)) => section(&mut rows, title, &order),
        (None, None) => {
            if pinned_count > 0 {
                section(&mut rows, "Pinned", &order[..pinned_count]);
            }
            if pinned_count == 0 || pinned_count < order.len() {
                section(&mut rows, "Chats", &order[pinned_count..]);
            }
        }
    }
    let folders = folders(discussions);
    if folder.is_none() && with_folders && !folders.is_empty() {
        rows.push(Row::Blank);
        rows.extend(folders.into_iter().map(|(f, n)| Row::Folder(f, n)));
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn draw_list(
    out: &mut impl Write,
    discussions: &[Discussion],
    // Discussions where someone is typing.
    typing: &[usize],
    // Discussions marked for the next action.
    marked: &[usize],
    // Name and version of the application, above the sections.
    heading: Option<(&str, &str)>,
    // Single section title; `None` for the "Pinned" / "Chats" sections.
    title: Option<&str>,
    // The folder shown, `None` for the inbox (followed by the folders if `with_folders`).
    folder: Option<Folder>,
    with_folders: bool,
    help: &str,
    selected: usize,
    scroll: &mut usize,
    w: u16,
    h: u16,
) -> io::Result<()> {
    let width = w as usize;
    let height = h as usize;
    let viewport = height.saturating_sub(FOOTER).max(1);
    let rows = list_rows(discussions, heading, title, folder, with_folders);

    // Keep the selected chat in view, with its section title when it is the first one.
    let sel_row = rows
        .iter()
        .position(|r| match r {
            Row::Chat(i) => *i == selected,
            Row::Folder(f, _) => folder_entry(*f) == selected,
            _ => false,
        })
        .unwrap_or(0);
    let mut top = sel_row;
    while top > 0 && !matches!(rows[top - 1], Row::Chat(_) | Row::Folder(..)) {
        top -= 1;
    }
    if top < *scroll {
        *scroll = top;
    }
    if sel_row >= *scroll + viewport {
        *scroll = sel_row + 1 - viewport;
    }
    *scroll = (*scroll).min(rows.len().saturating_sub(viewport));

    queue!(out, Clear(ClearType::All), cursor::Hide)?;
    for y in 0..viewport {
        let text = match rows.get(*scroll + y) {
            Some(Row::Heading(name, version)) => {
                let text = if version.is_empty() {
                    ui::pad_left(name, width)
                } else {
                    ui::pad_left(&format!("{name} {version}"), width)
                };
                let bold = Styled {
                    start: 0,
                    end: name.len().min(text.len()),
                    style: data::Style::Bold,
                };
                print_styled(out, y as u16, &text, &[bold], false)?;
                continue;
            }
            Some(Row::Title(title)) => ui::pad_left(title, width),
            Some(Row::Chat(idx)) => {
                let idx = *idx;
                let discussion = &discussions[idx];
                let mut title = if typing.contains(&idx) {
                    format!("{} ...", discussion.title)
                } else {
                    discussion.title.clone()
                };
                if marked.contains(&idx) {
                    title.insert_str(0, "\u{2713} ");
                }
                // Number of new messages in front, right-aligned in a 4-column prefix: "  ③ ", "99+ ".
                let text = format!(
                    "{} {}",
                    ui::pad_right(&unread_badge(discussion.unread), 3),
                    ui::pad_left(&title, width.saturating_sub(4))
                );
                print_row(out, y as u16, &text, idx == selected)?;
                continue;
            }
            Some(Row::Folder(f, n)) => {
                let text = format!(
                    "    {}",
                    ui::pad_left(&format!("{} ({n})", f.name()), width.saturating_sub(4))
                );
                print_row(out, y as u16, &text, folder_entry(*f) == selected)?;
                continue;
            }
            Some(Row::Blank) | None => " ".repeat(width),
        };
        print_row(out, y as u16, &text, false)?;
    }

    print_row(
        out,
        height.saturating_sub(1) as u16,
        &ui::pad_left(help, width),
        false,
    )?;
    Ok(())
}

/// The search: the query, then the chats and the messages found, the selected one in
/// view.
fn draw_find(
    out: &mut impl Write,
    discussions: &[Discussion],
    // Chats marked for the next action.
    marked: &[usize],
    finding: &mut Finding,
    help: &str,
    w: u16,
    h: u16,
) -> io::Result<()> {
    let width = w as usize;
    let height = h as usize;
    // The query line and a blank one above the results.
    let viewport = height.saturating_sub(FOOTER + 2).max(1);
    // The results under their section titles: (result index, text, styles).
    let mut rows: Vec<(Option<usize>, String, Vec<Styled>)> = Vec::new();
    let mut section = None;
    for (i, found) in finding.results.iter().enumerate() {
        let message = match found {
            // Gone meanwhile (deleted).
            Found::Chat(chat) if discussions[*chat].hidden => continue,
            Found::Chat(_) => None,
            Found::Message(chat, id) => match discussions[*chat].position_of(id) {
                Some(msg) => Some(&discussions[*chat].messages[msg]),
                // Gone meanwhile.
                None => continue,
            },
        };
        let title = match found {
            Found::Chat(_) => "Chats",
            Found::Message(..) => "Messages",
        };
        if section != Some(title) {
            if section.is_some() {
                rows.push((None, String::new(), Vec::new()));
            }
            rows.push((None, title.to_string(), Vec::new()));
            section = Some(title);
        }
        let row = match (found, message) {
            (Found::Chat(chat), _) => {
                let chat = *chat;
                let d = &discussions[chat];
                let folder = d.folder.map(|f| format!(" \u{b7} {}", f.name()));
                let mark = if marked.contains(&chat) {
                    "\u{2713} "
                } else {
                    ""
                };
                let text = format!("    {mark}{}{}", d.title, folder.unwrap_or_default());
                let styles = matched(&text, &finding.query, 4 + mark.len(), d.title.len());
                (Some(i), text, styles)
            }
            (Found::Message(chat, _), m) => {
                let (d, m) = (&discussions[*chat], m.expect("found above"));
                let head = format!(
                    "    {} \u{b7} {} \u{b7} ",
                    d.title,
                    m.timestamp.format("%b %-d, %Y")
                );
                let room = width.saturating_sub(ui::width_of(&head)).max(10);
                let (snippet, range) = find::snippet(&m.text, &finding.query, room / 4, room);
                let styles = range
                    .map(|(s, e)| Styled {
                        start: head.len() + s,
                        end: head.len() + e,
                        style: data::Style::Bold,
                    })
                    .into_iter()
                    .collect();
                (Some(i), format!("{head}{snippet}"), styles)
            }
        };
        rows.push(row);
    }

    let sel_row = rows
        .iter()
        .position(|(i, ..)| *i == Some(finding.selected))
        .unwrap_or(0);
    // The section title in view with its first result.
    let top = sel_row.saturating_sub(if sel_row <= 1 { sel_row } else { 0 });
    if top < finding.scroll {
        finding.scroll = top;
    }
    if sel_row >= finding.scroll + viewport {
        finding.scroll = sel_row + 1 - viewport;
    }

    queue!(out, Clear(ClearType::All), cursor::Hide)?;
    let cursor = if finding.browsing { "" } else { "\u{2588}" };
    let query = format!("Find: {}{cursor}", finding.query);
    let bold = Styled {
        start: 0,
        end: "Find:".len(),
        style: data::Style::Bold,
    };
    print_styled(out, 0, &ui::pad_left(&query, width), &[bold], false)?;
    for y in 0..viewport {
        let Some((result, text, styles)) = rows.get(finding.scroll + y) else {
            break;
        };
        let selected = *result == Some(finding.selected);
        print_styled(
            out,
            (y + 2) as u16,
            &ui::pad_left(text, width),
            styles,
            selected,
        )?;
    }
    print_row(
        out,
        height.saturating_sub(1) as u16,
        &ui::pad_left(help, width),
        false,
    )?;
    Ok(())
}

/// The choice of whom to start a chat with: those picked and the query, then the number
/// typed and the contacts matching, the selected one in view.
fn draw_new_chat(
    out: &mut impl Write,
    new: &mut Recipients,
    help: &str,
    w: u16,
    h: u16,
) -> io::Result<()> {
    let width = w as usize;
    let height = h as usize;
    // The recipients line and a blank one above the entries.
    let viewport = height.saturating_sub(FOOTER + 2).max(1);
    let entries = new.entries();
    if new.selected < new.scroll {
        new.scroll = new.selected;
    }
    if new.selected >= new.scroll + viewport {
        new.scroll = new.selected + 1 - viewport;
    }
    queue!(out, Clear(ClearType::All), cursor::Hide)?;
    let picked: Vec<&str> = new.picked.iter().map(Contact::label).collect();
    let mut to = format!("To: {}", picked.join(", "));
    if !picked.is_empty() {
        to.push_str(", ");
    }
    let cursor = if new.browsing { "" } else { "\u{2588}" };
    let line = format!("{to}{}{cursor}", new.query);
    let bold = Styled {
        start: 0,
        end: "To:".len(),
        style: data::Style::Bold,
    };
    print_styled(out, 0, &ui::pad_left(&line, width), &[bold], false)?;
    for (y, (i, contact)) in entries
        .iter()
        .enumerate()
        .skip(new.scroll)
        .take(viewport)
        .enumerate()
    {
        let (text, styles) = if contact.name.is_empty() {
            (format!("    Number {}", contact.number), Vec::new())
        } else {
            let text = format!("    {} \u{b7} {}", contact.name, contact.number);
            let styles = if recipients::number(&new.query).is_some() {
                Vec::new()
            } else {
                matched(&text, &new.query, 4, contact.name.len())
            };
            (text, styles)
        };
        let selected = i == new.selected && (new.browsing || !new.query.trim().is_empty());
        print_styled(
            out,
            (y + 2) as u16,
            &ui::pad_left(&text, width),
            &styles,
            selected,
        )?;
    }
    print_row(
        out,
        height.saturating_sub(1) as u16,
        &ui::pad_left(help, width),
        false,
    )?;
    Ok(())
}

/// Bold on the first match of `query` in the `len` bytes of `text` from `from`.
fn matched(text: &str, query: &str, from: usize, len: usize) -> Vec<Styled> {
    let (_, range) = find::snippet(&text[from..from + len], query, usize::MAX, usize::MAX);
    range
        .map(|(s, e)| Styled {
            start: from + s,
            end: from + e,
            style: data::Style::Bold,
        })
        .into_iter()
        .collect()
}

fn draw_settings(
    out: &mut impl Write,
    settings: &mut Settings,
    selected: usize,
    status: Option<&str>,
    w: u16,
    h: u16,
) -> io::Result<()> {
    let width = w as usize;
    let height = h as usize;
    queue!(out, Clear(ClearType::All), cursor::Hide)?;
    print_row(out, 0, &ui::pad_left("Parameters", width), false)?;
    for (i, (label, value)) in SETTINGS.iter().enumerate() {
        let mark = if *value(settings) { "x" } else { " " };
        print_row(
            out,
            (HEADER + i) as u16,
            &ui::pad_left(&format!("[{mark}] {label}"), width),
            i == selected,
        )?;
    }
    let help = "\u{2191}/\u{2193} select   Space/Enter toggle   Esc back   q quit";
    print_row(
        out,
        height.saturating_sub(1) as u16,
        &ui::pad_left(status.unwrap_or(help), width),
        false,
    )?;
    Ok(())
}

/// The picker footer before a shortcode is typed: the quick reactions.
fn picker_line() -> String {
    let quick: Vec<String> = QUICK_REACTIONS
        .iter()
        .enumerate()
        .map(|(i, emoji)| format!("{} {emoji}", i + 1))
        .collect();
    format!(
        "React: {}   or type a shortcode   Esc cancel",
        quick.join("  ")
    )
}

/// The completion list above the draft while a `:shortcode` is typed, laid out as fzf
/// does: `rows` matches with the best at the bottom, the letters typed in bold, then the
/// count. Also gives the row of the match under `cursor`.
fn completion_list(
    filter: &str,
    cursor: usize,
    rows: usize,
) -> (Vec<(String, Vec<Styled>)>, Option<usize>) {
    let matches = fuzzy::emoji_matches(filter);
    // The window of matches scrolls to keep the cursor in it.
    let start = cursor.saturating_sub(rows - 1);
    let mut lines = Vec::new();
    let mut highlight = None;
    for i in (start..start + rows).rev() {
        let Some(m) = matches.get(i) else {
            lines.push((String::new(), Vec::new()));
            continue;
        };
        let marker = if i == cursor {
            highlight = Some(lines.len());
            "> "
        } else {
            "  "
        };
        let line = format!("{marker}{} :{}:", m.emoji, m.shortcode);
        let at = marker.len() + m.emoji.len() + 2;
        let styles = m
            .positions
            .iter()
            .map(|&p| Styled {
                start: at + p,
                end: at + p + 1,
                style: data::Style::Bold,
            })
            .collect();
        lines.push((line, styles));
    }
    let count = if matches.is_empty() {
        "  0/0   no match   Esc cancel".to_string()
    } else {
        format!(
            "  {}/{}   \u{2191}/\u{2193} browse   Enter pick   Esc cancel",
            cursor + 1,
            matches.len()
        )
    };
    lines.push((count, Vec::new()));
    (lines, highlight)
}

#[allow(clippy::too_many_arguments)]
fn draw_discussion(
    out: &mut impl Write,
    discussion: &Discussion,
    thumbnails: &Thumbnails,
    // Where the "N new messages" line goes (index, count).
    new_from: Option<(usize, usize)>,
    // Names of the people typing.
    typing: &[&str],
    // Kept in view, and shown selected when `selecting`.
    selected_msg: usize,
    selecting: bool,
    scroll: &mut usize,
    composing: Option<&Compose>,
    // The reaction picker, while picking.
    reacting: Option<&Picker>,
    // Asking to confirm the deletion of the selected message: for whom.
    deleting: Option<&str>,
    status: Option<&str>,
    w: u16,
    h: u16,
) -> io::Result<usize> {
    let width = w as usize;
    let height = h as usize;

    // Footer: the help/status line, or the compose area with one row per line of the draft
    // (at most half the screen, showing the end of the draft).
    let mut cursor_pos = None;
    // Footer row shown reversed: the match under the cursor of the completion.
    let mut highlight = None;
    // Footer rows with their styles (only the draft has some).
    let footer: Vec<(String, Vec<Styled>)> = match composing {
        Some(
            compose @ Compose {
                editor, reply_to, ..
            },
        ) => {
            let prompt = match reply_to.and_then(|i| discussion.messages.get(i)) {
                Some(m) if m.from_me => "Reply to yourself> ".to_string(),
                Some(m) => format!("Reply to {}> ", m.sender_name),
                None => "> ".to_string(),
            };
            let indent = " ".repeat(ui::width_of(&prompt));
            let avail = width.saturating_sub(indent.len()).max(1);
            let max_rows = (height.saturating_sub(HEADER) / 2).max(1);
            let text = editor.text();
            let draft: Vec<&str> = text.split('\n').collect();
            let (cursor_line, _) = editor.cursor_line_col();
            // In screen columns: an emoji takes two.
            let cursor_col = ui::width_of(editor.line_before_cursor());
            // Rows shown: the end of the draft, or up to the cursor line if it is higher.
            let first = draft.len().saturating_sub(max_rows).min(cursor_line);
            // The cursor line scrolls sideways to keep the cursor visible.
            let offset = cursor_col.saturating_sub(avail - 1);
            let rows: Vec<(String, Vec<Styled>)> = draft
                [first..(first + max_rows).min(draft.len())]
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    let lead = if first + i == 0 { &prompt } else { &indent };
                    let skip = if first + i == cursor_line { offset } else { 0 };
                    let shown = ui::columns(line, skip, avail);
                    // Where `shown` is in the draft, for its styles.
                    let at = shown.as_ptr() as usize - text.as_ptr() as usize;
                    (
                        format!("{lead}{shown}"),
                        ui::styles_in(editor.styles(), at..at + shown.len(), lead.len()),
                    )
                })
                .collect();
            let col = indent.len() + cursor_col - offset;
            // The completion list goes above the draft, leaving a line of messages.
            let list_rows = COMPLETION_ROWS.min(height.saturating_sub(HEADER + rows.len() + 2));
            let completion = match (compose.completion_filter(), &compose.completion) {
                (Some(filter), Some(completion)) if list_rows > 0 => {
                    let (lines, at) = completion_list(&filter, completion.cursor, list_rows);
                    highlight = at;
                    lines
                }
                _ => Vec::new(),
            };
            cursor_pos = Some((
                col.min(width.saturating_sub(1)),
                cursor_line - first + completion.len(),
            ));
            completion.into_iter().chain(rows).collect()
        }
        None => match reacting {
            Some(picker) if picker.filter.is_empty() => vec![(picker_line(), Vec::new())],
            Some(picker) => {
                // The list of matches above the shortcode, leaving a line of messages.
                let rows = COMPLETION_ROWS
                    .min(height.saturating_sub(HEADER + 2))
                    .max(1);
                let (mut lines, at) = completion_list(&picker.filter, picker.cursor, rows);
                highlight = at;
                let prompt = format!("React :{}", picker.filter);
                cursor_pos = Some((
                    ui::width_of(&prompt).min(width.saturating_sub(1)),
                    lines.len(),
                ));
                lines.push((prompt, Vec::new()));
                lines
            }
            None if let Some(whom) = deleting => {
                vec![(
                    format!("Delete this message {whom}?   y yes   any other key: no"),
                    Vec::new(),
                )]
            }
            None => {
                let failed = discussion
                    .messages
                    .get(selected_msg)
                    .is_some_and(|m| m.failed && !m.deleted);
                let sending = discussion
                    .messages
                    .get(selected_msg)
                    .is_some_and(|m| m.sending);
                let help = if selecting && sending {
                    "\u{2191}/\u{2193} navigate   c cancel sending   Enter new message   Esc unselect   q quit"
                } else if selecting && failed {
                    "\u{2191}/\u{2193} navigate   s resend   c copy   Enter new message   Esc unselect   q quit"
                } else if selecting {
                    "\u{2191}/\u{2193} navigate   r reply   e react   f forward   c copy   o open links   Del delete   Enter new message   Esc unselect   q quit"
                } else {
                    "\u{2191} select   Enter new message   Esc back   q quit"
                };
                vec![(status.unwrap_or(help).to_string(), Vec::new())]
            }
        },
    };
    let viewport = height.saturating_sub(HEADER + footer.len()).max(1);

    let mut lines = ui::build_discussion_lines(discussion, width, new_from, thumbnails);

    let mut sel_min = None;
    let mut sel_max = None;
    for (i, l) in lines.iter().enumerate() {
        if l.msg_idx == Some(selected_msg) {
            sel_min.get_or_insert(i);
            sel_max = Some(i);
        }
    }
    let sel_min = sel_min.unwrap_or(0);
    let mut sel_max = sel_max.unwrap_or(0);

    // "Name ..." under the last message while someone types, kept in view with it.
    let typing_text = (!typing.is_empty()).then(|| format!("{} ...", typing.join(", ")));
    let mut typing_line = None;
    if let Some(text) = &typing_text {
        if !lines.is_empty() {
            lines.push(ui::RenderLine::new(None, ui::LineKind::Left(String::new())));
        }
        lines.push(ui::RenderLine::new(None, ui::LineKind::Left(text.clone())));
        typing_line = Some(lines.len() - 1);
        if selected_msg + 1 >= discussion.messages.len() {
            sel_max = lines.len() - 1;
        }
    }

    if sel_min < *scroll {
        *scroll = sel_min;
    }
    if sel_max >= *scroll + viewport {
        *scroll = sel_max + 1 - viewport;
    }
    let max_scroll = lines.len().saturating_sub(viewport);
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }

    // Reading further up (from the first new message, or a message selected): "Name ..." is
    // out of view, so it goes beside the title.
    let title = match (&typing_text, typing_line) {
        (Some(text), Some(line)) if line < *scroll || line >= *scroll + viewport => {
            format!("{}   {text}", discussion.title)
        }
        _ => discussion.title.clone(),
    };
    queue!(out, Clear(ClearType::All), cursor::Hide)?;
    print_row(out, 0, &ui::pad_left(&title, width), false)?;
    print_row(out, 1, &" ".repeat(width), false)?;

    let mut visible_messages = std::collections::BTreeSet::new();
    for row in 0..viewport {
        let y = (HEADER + row) as u16;
        let idx = *scroll + row;
        if idx < lines.len() {
            let rl = &lines[idx];
            visible_messages.extend(rl.msg_idx);
            let (text, styles) = ui::render_line(rl, width);
            // Reversed, the placeholders would lose their color: the image id.
            let selected = selecting
                && rl.msg_idx == Some(selected_msg)
                && !matches!(rl.kind, ui::LineKind::Image { .. });
            print_line(out, y, &text, &styles, selected, rl.alert)?;
        } else {
            print_row(out, y, &" ".repeat(width), false)?;
        }
    }

    let footer_y = height.saturating_sub(footer.len());
    for (row, (text, styles)) in footer.iter().enumerate() {
        print_styled(
            out,
            (footer_y + row) as u16,
            &ui::pad_left(text, width),
            styles,
            highlight == Some(row),
        )?;
    }
    if let Some((col, row)) = cursor_pos {
        queue!(
            out,
            cursor::MoveTo(col as u16, (footer_y + row) as u16),
            cursor::Show
        )?;
    }
    // Messages at least partly on screen.
    Ok(visible_messages.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        let mut app = App::new(data::mock_discussions());
        app.capabilities.delete_max_age = Some(Duration::from_secs(24 * 60 * 60));
        app
    }

    /// Opens the first discussion (last message selected).
    fn opened() -> App {
        let mut app = app();
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Opened(0)));
        app
    }

    /// The first discussion opened, its last message selected.
    fn selected() -> App {
        let mut app = opened();
        app.handle_key(key(KeyCode::Up));
        app
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(c))), None);
        }
    }

    fn image_attachment() -> data::Attachment {
        data::Attachment {
            content_type: "image/png".to_string(),
            file_name: Some("clipboard.png".to_string()),
            bytes: vec![1, 2, 3],
            width: Some(1),
            height: Some(1),
        }
    }

    #[test]
    fn quit_from_list_and_discussion() {
        assert_eq!(
            app().handle_key(key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        assert_eq!(
            opened().handle_key(key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn new_message_is_sent_trimmed() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, " hello ");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "hello".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn shift_enter_adds_a_line() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "a");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            None
        );
        type_text(&mut app, "b");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "a\nb".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn arrows_move_the_cursor_in_the_draft() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "ac");
        app.handle_key(key(KeyCode::Left));
        type_text(&mut app, "b");
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Delete));
        app.handle_key(key(KeyCode::End));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "bc".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn alt_or_ctrl_arrows_move_by_word() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "one two");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        type_text(&mut app, "big ");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        type_text(&mut app, "!");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "one! big two".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn colon_completes_emoji() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "go :rkt");
        let matches = fuzzy::emoji_matches("rkt");
        assert_eq!(matches[0].emoji, "\u{1f680}", "fuzzy: rocket");
        let text = screen(&mut app);
        assert!(
            text.contains("> \u{1f680} :rocket:"),
            "best match under the cursor"
        );
        assert!(text.contains(&format!("1/{}", matches.len())));
        // The next matches are above it, as in fzf.
        app.handle_key(key(KeyCode::Up));
        let next = format!("> {} :{}:", matches[1].emoji, matches[1].shortcode);
        assert!(screen(&mut app).contains(&next));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            None,
            "Enter picks, does not send"
        );
        type_text(&mut app, " now");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "go \u{1f680} now".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn esc_leaves_the_typed_colon() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "a :ro");
        app.handle_key(key(KeyCode::Esc));
        assert!(
            !screen(&mut app).contains("[\u{1f680} rocket]"),
            "completion closed"
        );
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "a :ro".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn no_completion_inside_words() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "at 10:30");
        assert!(!screen(&mut app).contains("Enter pick"));
        type_text(&mut app, " :");
        assert!(screen(&mut app).contains("Enter pick"), "all emoji listed");
        type_text(&mut app, " ok");
        assert!(!screen(&mut app).contains("Enter pick"), "a space ends it");
    }

    #[test]
    fn cursor_is_placed_after_wide_emoji() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "a :rocket: b");
        let mut out = Vec::new();
        app.draw(&mut out, 80, 30).unwrap();
        // The draw ends with the cursor move: "> a 🚀 b" takes 8 columns (the rocket two),
        // so the cursor goes to column 8, written 9 by the 1-based escape sequence.
        let tail = String::from_utf8(out).unwrap();
        assert!(
            tail.ends_with("\x1b[30;9H\x1b[?25h"),
            "{:?}",
            &tail[tail.len().saturating_sub(20)..]
        );
    }

    #[test]
    fn markdown_formatting_is_sent_as_styles() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, " **tout** *a* ~b~");
        let send = app.handle_key(key(KeyCode::Enter));
        let styles = vec![
            Styled {
                start: 0,
                end: 4,
                style: data::Style::Bold,
            },
            Styled {
                start: 5,
                end: 6,
                style: data::Style::Italic,
            },
            Styled {
                start: 7,
                end: 8,
                style: data::Style::Strikethrough,
            },
        ];
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "tout a b".to_string(),
                styles,
                reply_to: None
            })
        );
    }

    #[test]
    fn formatting_is_drawn_with_terminal_attributes() {
        let bold = "\x1b[1m";
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "**gras**");
        let mut out = Vec::new();
        app.draw(&mut out, 80, 30).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains(&format!("{bold}gras")),
            "in the draft"
        );

        let last = app.discussions[0].messages.len() - 1;
        let message = &mut app.discussions[0].messages[last];
        message.styles = vec![Styled {
            start: 0,
            end: 7,
            style: data::Style::Bold,
        }];
        let mut out = Vec::new();
        app.draw(&mut out, 80, 30).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains(&format!("{bold}Morning")),
            "in a post"
        );
    }

    #[test]
    fn esc_undoes_formatting_then_closes() {
        let mut app = opened();
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "**x**");
        app.handle_key(key(KeyCode::Esc));
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "**x**".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn paste_keeps_line_breaks_without_sending() {
        let mut app = opened();
        app.handle_paste("line 1\nline 2");
        type_text(&mut app, "!");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "line 1\nline 2!".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
    }

    #[test]
    fn ctrl_or_super_v_pastes_clipboard_image_placeholder() {
        for (code, modifiers) in [
            (KeyCode::Char('v'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('V'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('v'), KeyModifiers::SUPER),
        ] {
            let mut app = opened();
            assert_eq!(
                app.handle_key(KeyEvent::new(code, modifiers)),
                Some(Action::PasteImageFromClipboard)
            );
            let attachment = image_attachment();
            app.handle_pasted_image(attachment.clone());
            let send = app.handle_key(key(KeyCode::Enter));
            assert_eq!(
                send,
                Some(Action::SendWithAttachments {
                    discussion: 0,
                    text: "".to_string(),
                    attachments: vec![attachment],
                    reply_to: None,
                })
            );
        }
    }

    #[test]
    fn open_targets_include_pasted_images() {
        let mut app = opened();
        let last = app.discussions[0].messages.len() - 1;
        app.discussions[0].messages[last].attachments =
            vec![image_attachment(), image_attachment()];
        let targets = app.open_targets_for_selection();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|target| target.ends_with(".png")));
    }

    #[test]
    fn pasted_images_get_unique_labels() {
        let mut app = opened();
        app.handle_pasted_image(image_attachment());
        app.handle_pasted_image(image_attachment());
        assert!(screen(&mut app).contains("[image #1][image #2]"));
        let Some(Action::SendWithAttachments {
            discussion,
            text,
            attachments,
            reply_to,
        }) = app.handle_key(key(KeyCode::Enter))
        else {
            panic!("expected a send with attachments");
        };
        assert_eq!(text, "", "labels are not sent");
        let mut message = Message::mine(1, text, reply_to, false);
        message.attachments = attachments;
        app.push_message(discussion, message);
        app.handle_pasted_image(image_attachment());
        assert!(screen(&mut app).contains("[image #3]"), "numbering goes on");
    }

    #[test]
    fn opening_received_attachments_downloads_them() {
        let mut app = selected();
        let last = app.discussions[0].messages.len() - 1;
        app.discussions[0].messages[last].remote_attachments = vec!["image/jpeg".to_string()];
        assert_eq!(
            app.handle_key(key(KeyCode::Char('o'))),
            Some(Action::FetchAttachments {
                discussion: 0,
                msg: last
            })
        );
    }

    #[test]
    fn reply_quotes_the_selected_message() {
        let mut app = selected();
        let last = app.discussions[0].messages.len() - 1;
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Char('r')));
        type_text(&mut app, "ok");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "ok".to_string(),
                styles: vec![],
                reply_to: Some(last - 1)
            })
        );
    }

    #[test]
    fn forward_to_the_chosen_discussion() {
        let mut app = selected();
        let last = app.discussions[0].messages.len() - 1;
        app.handle_key(key(KeyCode::Char('f')));
        app.handle_key(key(KeyCode::Down));
        let forward = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            forward,
            Some(Action::Forward {
                from: 0,
                msg: last,
                to: 1
            })
        );
    }

    #[test]
    fn parameters_toggle_notifications() {
        let mut app = app();
        assert!(app.settings.notifications, "on by default");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char(' '))),
            Some(Action::SettingsChanged)
        );
        assert!(!app.settings.notifications);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::Opened(1)),
            "back on the same discussion"
        );
    }

    /// The screen as text (escape sequences dropped), for rendering checks.
    fn screen(app: &mut App) -> String {
        let mut out = Vec::new();
        app.draw(&mut out, 80, 30).unwrap();
        let raw = String::from_utf8(out).unwrap();
        let mut text = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // CSI sequence: skip up to its final letter.
                chars.by_ref().find(|c| c.is_ascii_alphabetic());
            } else {
                text.push(c);
            }
        }
        text
    }

    #[test]
    fn typing_is_shown_until_message_stop_or_timeout() {
        let now = Instant::now();
        let mut app = app();
        app.set_typing(1, "dad", "Dad", true, now);
        assert!(screen(&mut app).contains("Family Group ..."));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            screen(&mut app).matches("Dad ...").count(),
            1,
            "under the last message only"
        );

        let message = Message {
            from_me: false,
            author: Some("dad".to_string()),
            ..Message::mine(1, "hi".to_string(), None, false)
        };
        app.push_message(1, message);
        assert!(
            !screen(&mut app).contains("Dad ..."),
            "the message replaces the indicator"
        );

        app.set_typing(1, "mom", "Mom", true, now);
        app.set_typing(1, "mom", "Mom", false, now);
        assert_eq!(app.next_typing_expiry(), None, "stopped");

        app.set_typing(1, "mom", "Mom", true, now);
        app.expire_typing(now + TYPING_TIMEOUT);
        assert!(!screen(&mut app).contains("Mom ..."), "expired");
    }

    #[test]
    fn typing_out_of_view_is_shown_beside_the_title() {
        let mut app = app();
        for i in 0..60 {
            let news = Message {
                from_me: false,
                author: Some("mom".to_string()),
                ..Message::mine(100 + i, format!("news {i}"), None, false)
            };
            app.push_message(1, news);
        }
        // Opened on the first new message: the end of the discussion is out of view.
        app.open(1);
        app.set_typing(1, "dad", "Dad", true, Instant::now());
        let text = screen(&mut app);
        assert!(
            !text.contains("news 59"),
            "reading from the first new message"
        );
        assert!(text.contains("Family Group   Dad ..."));
        assert_eq!(text.matches("Dad ...").count(), 1);
    }

    #[test]
    fn pinned_chats_come_first_in_their_own_list() {
        let mut app = app();
        let text = screen(&mut app);
        assert!(text.find("Pinned").unwrap() < text.find("Alice Martin").unwrap());
        assert!(text.find("Alice Martin").unwrap() < text.find("Chats").unwrap());

        // Pin Bob (index 2): it moves to the pinned list, and the selection follows it.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('P'))),
            Some(Action::PinToggled(vec![2]))
        );
        let text = screen(&mut app);
        assert!(text.find("Bob Dupont").unwrap() < text.find("Chats").unwrap());
        // Display order is now Alice, Bob, Family Group, Chloe.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Opened(1)));
    }

    #[test]
    fn pinning_again_goes_to_the_end_of_the_pinned_list() {
        let mut app = app();
        let pinned = |app: &mut App| {
            let text = screen(app);
            let end = text.find("Chats").unwrap();
            let mut names: Vec<(usize, &str)> =
                ["Alice Martin", "Family Group", "Bob Dupont", "Chloe Renard"]
                    .into_iter()
                    .filter_map(|n| text[..end].find(n).map(|at| (at, n)))
                    .collect();
            names.sort();
            names.into_iter().map(|(_, n)| n).collect::<Vec<_>>()
        };
        // Pin Family Group: [Alice, Family]. Unpin then re-pin Alice: [Family, Alice].
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('P')));
        assert_eq!(pinned(&mut app), ["Alice Martin", "Family Group"]);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Char('P')));
        assert_eq!(pinned(&mut app), ["Family Group"]);
        app.handle_key(key(KeyCode::Char('P')));
        assert_eq!(pinned(&mut app), ["Family Group", "Alice Martin"]);
    }

    #[test]
    fn page_keys_jump_a_page_of_chats() {
        let titles: Vec<Discussion> = (0..50)
            .map(|i| Discussion::new(i.to_string(), format!("Chat {i}")))
            .collect();
        let mut app = App::new(titles);
        let mut out = Vec::new();
        app.draw(&mut out, 80, 11).unwrap(); // 10 rows of list above the help line
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::Opened(10))
        );

        let mut app = App::new(
            (0..5)
                .map(|i| Discussion::new(i.to_string(), format!("Chat {i}")))
                .collect(),
        );
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::Opened(4)),
            "clamped to the last chat"
        );
    }

    #[test]
    fn page_keys_jump_a_page_of_messages() {
        let mut discussion = Discussion::new("long", "Long");
        discussion.messages = (0..40)
            .map(|i| Message::mine(i, format!("message {i}"), None, false))
            .collect();
        let mut app = App::new(vec![discussion]);
        app.handle_key(key(KeyCode::Enter)); // last message (39) in view
        let mut out = Vec::new();
        app.draw(&mut out, 80, 23).unwrap(); // 20 lines of messages: 3 lines each, 7 messages
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(key(KeyCode::Up)); // selects the message in view
        // Reply to read which message is selected.
        app.handle_key(key(KeyCode::Char('r')));
        type_text(&mut app, "x");
        let send = app.handle_key(key(KeyCode::Enter));
        let Some(Action::Send {
            reply_to: Some(selected),
            ..
        }) = send
        else {
            panic!("{send:?}")
        };
        assert_eq!(selected, 39 - app.message_page);
        assert!(
            app.message_page >= 5,
            "about a screen of messages, got {}",
            app.message_page
        );
    }

    #[test]
    fn react_with_a_quick_pick_or_a_shortcode() {
        let mut app = selected();
        let last = app.discussions[0].messages.len() - 1;
        app.handle_key(key(KeyCode::Char('e')));
        assert!(
            screen(&mut app).contains("React: 1 \u{2764}\u{fe0f}  2 \u{1f44d}"),
            "quick reactions shown"
        );
        let react = app.handle_key(key(KeyCode::Char('2')));
        let thumbs_up = "\u{1f44d}".to_string();
        assert_eq!(
            react,
            Some(Action::React {
                discussion: 0,
                msg: last,
                emoji: thumbs_up.clone(),
                remove: false
            })
        );

        app.handle_key(key(KeyCode::Char('e')));
        type_text(&mut app, "rocket");
        assert!(screen(&mut app).contains("> \u{1f680} :rocket:"));
        let react = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            react,
            Some(Action::React {
                discussion: 0,
                msg: last,
                emoji: "\u{1f680}".to_string(),
                remove: false
            })
        );

        // Our own reaction picked again: removed.
        app.discussions[0].messages[last].set_reaction("me", true, Some(&thumbs_up));
        app.handle_key(key(KeyCode::Char('e')));
        let react = app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(
            react,
            Some(Action::React {
                discussion: 0,
                msg: last,
                emoji: thumbs_up,
                remove: true
            })
        );
    }

    #[test]
    fn arrows_browse_all_the_matches_of_a_filter() {
        let mut app = selected();
        let last = app.discussions[0].messages.len() - 1;
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            None,
            "no browsing before a shortcode is typed"
        );

        let matches = fuzzy::emoji_matches("sm");
        assert!(matches.len() > 50, "every match: {}", matches.len());
        type_text(&mut app, "sm");
        let text = screen(&mut app);
        assert!(text.contains(&format!("1/{}", matches.len())));
        assert!(
            text.contains("React :sm"),
            "the list goes above the shortcode"
        );
        // The next matches are above the best one, as in fzf.
        for _ in 0..7 {
            app.handle_key(key(KeyCode::Up));
        }
        app.handle_key(key(KeyCode::Down));
        let line = format!("> {} :{}:", matches[6].emoji, matches[6].shortcode);
        assert!(screen(&mut app).contains(&line));
        let react = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            react,
            Some(Action::React {
                discussion: 0,
                msg: last,
                emoji: matches[6].emoji.to_string(),
                remove: false
            })
        );
    }

    #[test]
    fn delete_own_recent_messages_after_confirmation() {
        let mut app = opened();
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        app.push_message(0, Message::mine(now_ms, "oops".to_string(), None, false));
        let last = app.discussions[0].messages.len() - 1;

        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete this message for everyone?"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            None,
            "anything but y keeps it"
        );
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::Delete {
                discussion: 0,
                msg: last
            })
        );

        app.discussions[0].messages[last].mark_deleted();
        assert!(screen(&mut app).contains("This message was deleted"));
        assert_eq!(
            app.handle_key(key(KeyCode::Delete)),
            None,
            "already deleted"
        );

        // Old messages, and those of others, cannot be deleted.
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Only your messages of the last 24 h"));
    }

    #[test]
    fn delete_any_message_for_me_after_confirmation() {
        let mut app = opened();
        app.capabilities.delete_for_me = true;
        // An old message of someone else: deletable for us.
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        let selected = match app.screen {
            Screen::Discussion { selected_msg, .. } => selected_msg,
            _ => unreachable!(),
        };
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete this message for you?"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::Delete {
                discussion: 0,
                msg: selected
            })
        );
    }

    #[test]
    fn the_draft_is_the_text_being_typed() {
        let mut app = opened();
        assert_eq!(app.draft(), None);
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "hi");
        let id = app.discussions[0].id.clone();
        assert_eq!(app.draft(), Some((&id, "hi")));
    }

    #[test]
    fn reactions_are_shown_under_the_message() {
        let mut app = opened();
        let last = app.discussions[0].messages.len() - 1;
        let message = &mut app.discussions[0].messages[last];
        message.set_reaction("a", false, Some("\u{1f44d}"));
        message.set_reaction("b", false, Some("\u{1f44d}"));
        message.set_reaction("c", true, Some("\u{2764}\u{fe0f}"));
        assert!(screen(&mut app).contains("\u{1f44d} 2  \u{2764}\u{fe0f}"));
        app.discussions[0].messages[last].set_reaction("c", true, None);
        assert!(!screen(&mut app).contains("\u{2764}\u{fe0f}"));
    }

    #[test]
    fn the_application_name_heads_the_list() {
        let mut app = app();
        assert!(
            !screen(&mut app).contains("signal-tui"),
            "no name, no heading"
        );
        app.name = "signal-tui".to_string();
        let text = screen(&mut app);
        assert!(text.find("signal-tui").unwrap() < text.find("Pinned").unwrap());
        app.handle_key(key(KeyCode::Char('P')));
        let text = screen(&mut app);
        assert!(text.find("signal-tui").unwrap() < text.find("Chats").unwrap());
        app.version = "v1.4.0+dfa66".to_string();
        assert!(
            screen(&mut app).contains("signal-tui v1.4.0+dfa66"),
            "on the same line"
        );
    }

    #[test]
    fn the_chat_list_keeps_its_view_after_a_chat_or_the_parameters() {
        let titles = (0..100)
            .map(|i| Discussion::new(i.to_string(), format!("Chat {i:02}")))
            .collect();
        let mut app = App::new(titles);
        // Scrolled down, then a few chats up: the selected one is not on the last row.
        for _ in 0..60 {
            app.handle_key(key(KeyCode::Down));
        }
        screen(&mut app);
        for _ in 0..20 {
            app.handle_key(key(KeyCode::Up));
        }
        let before = screen(&mut app);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::Opened(40))
        );
        screen(&mut app);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(screen(&mut app), before);
        app.handle_key(key(KeyCode::Char('p')));
        screen(&mut app);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(screen(&mut app), before);
    }

    #[test]
    fn without_pinned_chats_only_chats_are_listed() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('P')));
        let text = screen(&mut app);
        assert!(!text.contains("Pinned"));
        assert!(text.contains("Chats"));
    }

    #[test]
    fn unread_badges() {
        assert_eq!(unread_badge(0), "");
        assert_eq!(unread_badge(1), "\u{2460}");
        assert_eq!(unread_badge(20), "\u{2473}");
        assert_eq!(unread_badge(21), "21");
        assert_eq!(unread_badge(99), "99");
        assert_eq!(unread_badge(100), "99+");
        for n in 1..=100 {
            assert!(ui::width_of(&unread_badge(n)) <= 3, "fits the prefix");
        }
    }

    /// A message `id` sent at `ms`, from someone else.
    fn at(id: &str, ms: u64) -> Message {
        Message {
            sent_ms: ms,
            from_me: false,
            ..Message::mine(id, id.to_string(), None, false)
        }
    }

    #[test]
    fn history_merges_in_send_order_and_keeps_pointers() {
        let mut app = app();
        let discussion = &mut app.discussions[0];
        discussion.messages = vec![at("c", 30), at("d", 40)];
        discussion.messages[1].reply_to = Some(0);
        app.open(0);
        app.merge_history(
            0,
            vec![
                (at("b", 20), Some("a".to_string())),
                (at("a", 10), None),
                (
                    Message {
                        text: "c edited".to_string(),
                        ..at("c", 30)
                    },
                    None,
                ),
            ],
        );
        let messages = &app.discussions[0].messages;
        let ids: Vec<_> = messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        assert_eq!(messages[1].reply_to, Some(0), "quote resolved by id");
        assert_eq!(messages[3].reply_to, Some(2), "existing quote moved along");
        assert_eq!(messages[2].text, "c edited", "known message replaced");
        assert!(
            matches!(
                app.screen,
                Screen::Discussion {
                    selected_msg: 3,
                    ..
                }
            ),
            "still reading the end"
        );
    }

    #[test]
    fn history_keeps_the_selected_message() {
        let mut app = app();
        app.discussions[0].messages = vec![at("c", 30), at("d", 40)];
        app.open(0);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        app.merge_history(0, vec![(at("a", 10), None), (at("b", 20), None)]);
        assert!(
            matches!(
                app.screen,
                Screen::Discussion {
                    selected_msg: 2,
                    selecting: true,
                    ..
                }
            ),
            "\"c\" still selected"
        );
    }

    #[test]
    fn a_known_message_is_replaced_not_added() {
        let mut app = app();
        let received = |text: &str| Message {
            from_me: false,
            ..Message::mine("same", text.to_string(), None, false)
        };
        let before = app.discussions[2].messages.len();
        app.push_message(2, received("first"));
        let unread = app.discussions[2].unread;
        app.push_message(2, received("updated"));
        let messages = &app.discussions[2].messages;
        assert_eq!(messages.len(), before + 1);
        assert_eq!(messages.last().unwrap().text, "updated");
        assert_eq!(app.discussions[2].unread, unread, "not counted twice");
    }

    #[test]
    fn received_messages_mark_other_discussions_unread() {
        let mut app = opened();
        let received = |id: u32| Message {
            from_me: false,
            ..Message::mine(id, "hi".to_string(), None, false)
        };
        app.push_message(0, received(1));
        app.push_message(2, received(1));
        app.push_message(3, Message::mine(2, "sent".to_string(), None, false));
        app.push_message(2, received(3));
        assert_eq!(app.discussions[0].unread, 0, "open discussion stays read");
        assert_eq!(app.discussions[2].unread, 2);
        assert_eq!(app.discussions[3].unread, 0, "own messages are not updates");
    }

    #[test]
    fn opening_a_discussion_reads_it() {
        let mut app = app();
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.discussions[1].unread, 2);
        assert!(
            screen(&mut app).contains("\u{2461} Family Group"),
            "count in a circle"
        );
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Opened(1)));
        assert_eq!(app.discussions[1].unread, 0);
        // A line above the first new message, which is in view and selected by Up.
        let text = screen(&mut app);
        assert!(text.find("2 new messages").unwrap() < text.find("I'll bring the wine.").unwrap());
        assert!(text.find("Don't forget dinner").unwrap() < text.find("2 new messages").unwrap());
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Char('r')));
        type_text(&mut app, "x");
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 1,
                text: "x".to_string(),
                styles: vec![],
                reply_to: Some(1)
            })
        );
    }

    #[test]
    fn up_selects_and_esc_unselects() {
        let mut app = opened();
        assert!(
            screen(&mut app).contains("\u{2191} select"),
            "nothing selected"
        );
        assert_eq!(app.handle_key(key(KeyCode::Delete)), None);
        assert!(!screen(&mut app).contains("Delete this message"));
        app.handle_key(key(KeyCode::Up));
        assert!(
            screen(&mut app).contains("r reply"),
            "last message selected"
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(app.is_open(0), "Esc first unselects");
        // With no selection, the action letters start a new message.
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::Send {
                discussion: 0,
                text: "c".to_string(),
                styles: vec![],
                reply_to: None
            })
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.is_open(0), "then goes back to the list");
    }

    #[test]
    fn selection_follows_pushed_messages() {
        let mut app = opened();
        app.push_message(0, Message::mine(1, "new".to_string(), None, false));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Char('r')));
        type_text(&mut app, "x");
        let last = app.discussions[0].messages.len() - 1;
        let send = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            send,
            Some(Action::Send {
                discussion: 0,
                text: "x".to_string(),
                styles: vec![],
                reply_to: Some(last)
            })
        );
    }

    #[test]
    fn a_hidden_chat_is_not_listed_nor_selected() {
        let mut app = app();
        assert_eq!(app.handle_key(key(KeyCode::Down)), None);
        let selected = |app: &App| match app.screen {
            Screen::List { selected, .. } => selected,
            _ => unreachable!(),
        };
        let family = selected(&app);
        assert_eq!(app.discussions[family].id, "family");
        let shown = entries(&app.discussions, None, true);
        let next = shown[shown.iter().position(|&e| e == family).unwrap() + 1];
        let mut update = Discussion::new("family", "Family");
        update.hidden = true;
        app.update_discussion(family, update);
        assert!(!screen(&mut app).contains("Family"));
        assert_eq!(selected(&app), next, "the next one in its place");
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Down));
            assert_ne!(selected(&app), family);
        }
        app.update_discussion(family, Discussion::new("family", "Family renamed"));
        assert!(screen(&mut app).contains("Family renamed"));
    }

    #[test]
    fn a_hidden_chat_stays_open() {
        let mut app = opened();
        let mut update = Discussion::new("alice", "Alice Martin");
        update.hidden = true;
        app.update_discussion(0, update);
        assert!(app.is_open(0));
        app.handle_key(key(KeyCode::Esc));
        assert!(!screen(&mut app).contains("Alice Martin"));
        assert!(
            !screen(&mut app).contains("Pinned"),
            "nothing pinned is listed"
        );
    }

    #[test]
    fn archive_and_delete_chats_from_the_list() {
        let mut app = app();
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            None,
            "not supported"
        );
        assert_eq!(app.handle_key(key(KeyCode::Delete)), None);
        assert!(!screen(&mut app).contains("for good"));
        assert!(!screen(&mut app).contains("a archive"));

        app.capabilities.chat_actions = true;
        assert!(screen(&mut app).contains("P pin   a archive   Del delete"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Some(Action::Archive(vec![0]))
        );
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete \u{201c}Alice Martin\u{201d}?"));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);
        assert!(screen(&mut app).contains("Not deleted"));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::DeleteChat(vec![0]))
        );

        app.hide_discussion(0);
        assert!(!screen(&mut app).contains("Alice Martin"));
    }

    #[test]
    fn a_chat_archived_or_deleted_leaves_its_place_to_the_next_one() {
        let mut app = app();
        app.capabilities.chat_actions = true;
        let selected = |app: &App| match app.screen {
            Screen::List { selected, scroll } => (selected, scroll),
            _ => panic!("not the chat list"),
        };
        app.handle_key(key(KeyCode::Down));
        let shown = entries(&app.discussions, None, true);
        let (archived, _) = selected(&app);
        assert_eq!(archived, shown[1]);
        screen(&mut app);
        let scroll = selected(&app).1;

        app.file_discussion(archived, Some(Folder::Archive));
        assert_eq!(
            selected(&app),
            (shown[2], scroll),
            "the next one, the view kept"
        );
        screen(&mut app);
        assert_eq!(selected(&app).1, scroll);

        app.hide_discussion(shown[2]);
        assert_eq!(selected(&app).0, shown[3], "deleted: the next one too");

        // The last chat gone: the one before is last, the next entry (a folder) selected.
        let shown = entries(&app.discussions, None, true);
        let last = *shown.iter().rfind(|&&e| e < app.discussions.len()).unwrap();
        while selected(&app).0 != last {
            app.handle_key(key(KeyCode::Down));
        }
        app.hide_discussion(last);
        let place = shown.iter().position(|&e| e == last).unwrap();
        assert_eq!(selected(&app).0, shown[place + 1]);
    }

    #[test]
    fn marked_chats_are_pinned_archived_and_deleted_together() {
        let mut app = app();
        app.capabilities.chat_actions = true;
        let shown = entries(&app.discussions, None, true);
        let selected = |app: &App| match app.screen {
            Screen::List { selected, .. } => selected,
            _ => unreachable!(),
        };
        // Space marks, staying there: the first and third marked.
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(selected(&app), shown[0], "still on the chat marked");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        let text = screen(&mut app);
        assert!(text.contains("2 marked"));
        assert!(text.contains(&format!("\u{2713} {}", app.discussions[shown[0]].title)));
        assert!(!text.contains(&format!("\u{2713} {}", app.discussions[shown[1]].title)));

        // Esc unmarks, without quitting.
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert!(!screen(&mut app).contains("marked"));

        while selected(&app) != shown[0] {
            app.handle_key(key(KeyCode::Up));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        let marked = vec![shown[0], shown[1]];
        let pinned = |app: &App| {
            marked
                .iter()
                .map(|&i| app.discussions[i].pinned.is_some())
                .collect::<Vec<_>>()
        };
        let before = pinned(&app);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('P'))),
            Some(Action::PinToggled(marked.clone()))
        );
        let after = pinned(&app);
        assert!(
            if before.iter().all(|&p| p) {
                after.iter().all(|&p| !p)
            } else {
                after.iter().all(|&p| p)
            },
            "pinned unless all were: {before:?} -> {after:?}"
        );
        assert!(!screen(&mut app).contains("marked"), "unmarked once done");

        // Marked in list order, whatever the order of marking.
        let shown = entries(&app.discussions, None, true);
        while selected(&app) != shown[2] {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        while selected(&app) != shown[0] {
            app.handle_key(key(KeyCode::Up));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Some(Action::Archive(vec![shown[0], shown[2]]))
        );

        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete the 2 marked chats?"));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);
        assert_eq!(app.marked.len(), 2, "still marked");
        app.handle_key(key(KeyCode::Delete));
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::DeleteChat(chats)) if chats.len() == 2
        ));

        // A marked chat moved away is no longer marked.
        app.handle_key(key(KeyCode::Char(' ')));
        let gone = app.marked[0];
        app.file_discussion(gone, Some(Folder::Spam));
        assert!(app.marked.is_empty());
    }

    #[test]
    fn find_chats_and_messages_then_back_to_them() {
        let mut app = app();
        assert!(screen(&mut app).contains("/ find"));
        app.handle_key(key(KeyCode::Down));
        let list_selected = match app.screen {
            Screen::List { selected, .. } => selected,
            _ => unreachable!(),
        };
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "ALICE");
        let text = screen(&mut app);
        assert!(text.contains("Find: ALICE"));
        assert!(text.contains("Chats") && text.contains("Alice Martin"));
        // Enter: to the results, where letters act on chats; Enter again opens.
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(screen(&mut app).contains("Space mark"));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Opened(0)));
        // Back from the chat: the search as it was, then the chat list.
        app.handle_key(key(KeyCode::Esc));
        assert!(screen(&mut app).contains("Find: ALICE"));
        for _ in 0.."ALICE".len() {
            app.handle_key(key(KeyCode::Backspace));
        }
        type_text(&mut app, "having a LOOK");
        let text = screen(&mut app);
        assert!(text.contains("Messages") && text.contains("Thanks, having a look."));
        app.handle_key(key(KeyCode::Enter));
        let Some(Action::Opened(chat)) = app.handle_key(key(KeyCode::Enter)) else {
            panic!("not opened");
        };
        match app.screen {
            Screen::Discussion {
                selected_msg,
                selecting,
                ..
            } => {
                assert!(selecting, "the message found selected");
                assert_eq!(
                    app.discussions[chat].messages[selected_msg].text,
                    "Thanks, having a look."
                );
            }
            _ => panic!("not in the chat"),
        }
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::Find(_)));
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "zzz");
        assert!(screen(&mut app).contains("Nothing found"));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::List { selected, .. } if selected == list_selected));
    }

    #[test]
    fn chats_found_are_marked_pinned_archived_and_deleted() {
        let mut app = app();
        app.capabilities.chat_actions = true;
        app.handle_key(key(KeyCode::Char('/')));
        // Typed: letters go to the query.
        type_text(&mut app, "a");
        assert!(screen(&mut app).contains("Find: a\u{2588}"));
        let Screen::Find(finding) = &app.screen else {
            unreachable!()
        };
        let chats: Vec<usize> = finding
            .results
            .iter()
            .filter_map(|f| match f {
                Found::Chat(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert!(chats.len() >= 2, "{chats:?}");

        // Down: the results, where letters act on chats.
        app.handle_key(key(KeyCode::Down));
        let text = screen(&mut app);
        assert!(!text.contains("\u{2588}") && text.contains("Space mark"));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        let text = screen(&mut app);
        assert!(text.contains("2 marked"));
        assert!(text.contains(&format!("\u{2713} {}", app.discussions[chats[0]].title)));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Some(Action::Archive(vec![chats[0], chats[1]]))
        );
        app.file_discussion(chats[0], Some(Folder::Archive));
        assert!(
            screen(&mut app).contains("\u{b7} Archive"),
            "still found, in its folder"
        );

        // On a chat found: pinned, restored, deleted once confirmed.
        while !matches!(&app.screen, Screen::Find(f) if f.selected == 0) {
            app.handle_key(key(KeyCode::Up));
        }
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('P'))),
            Some(Action::PinToggled(c)) if c == [chats[0]]
        ));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            Some(Action::Restore(vec![chats[0]]))
        );
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete \u{201c}"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::DeleteChat(vec![chats[0]]))
        );
        app.hide_discussion(chats[0]);
        assert!(matches!(app.screen, Screen::Find(_)), "the search stays");

        // `/` types again; Up from the first result too.
        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "l");
        assert!(screen(&mut app).contains("Find: al"));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Up));
        type_text(&mut app, "i");
        assert!(screen(&mut app).contains("Find: ali"));
    }

    #[test]
    fn pinned_chats_move_up_and_down() {
        let mut app = app();
        let shift = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
        let pinned = |app: &App| -> Vec<usize> {
            order(&app.discussions, None)
                .into_iter()
                .filter(|&i| app.discussions[i].pinned.is_some())
                .collect()
        };
        // Three pinned, the first selected.
        for d in &mut app.discussions {
            d.pinned = None;
        }
        for (rank, i) in order(&app.discussions, None)
            .into_iter()
            .take(3)
            .enumerate()
        {
            app.discussions[i].pinned = Some(rank as u32);
        }
        let before = pinned(&app);
        assert_eq!(before.len(), 3, "{before:?}");
        while !matches!(app.screen, Screen::List { selected, .. } if selected == before[0]) {
            app.handle_key(key(KeyCode::Up));
        }
        assert_eq!(app.handle_key(shift(KeyCode::Up)), None, "first already");
        assert_eq!(
            app.handle_key(shift(KeyCode::Down)),
            Some(Action::PinsReordered)
        );
        assert_eq!(pinned(&app), [before[1], before[0], before[2]]);
        assert!(
            matches!(app.screen, Screen::List { selected, .. } if selected == before[0]),
            "the chat moved stays selected"
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            Some(Action::PinsReordered)
        );
        assert_eq!(pinned(&app), [before[1], before[2], before[0]]);
        assert_eq!(app.handle_key(shift(KeyCode::Down)), None, "last already");

        // Not pinned: nothing moves.
        let unpinned = order(&app.discussions, None)
            .into_iter()
            .find(|&i| app.discussions[i].pinned.is_none())
            .unwrap();
        while !matches!(app.screen, Screen::List { selected, .. } if selected == unpinned) {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.handle_key(shift(KeyCode::Up)), None);
    }

    #[test]
    fn the_trash_is_purged_once_confirmed() {
        let mut app = app();
        app.capabilities.chat_actions = true;
        app.file_discussion(1, Some(Folder::Trash));
        app.file_discussion(2, Some(Folder::Trash));
        assert_eq!(app.handle_key(key(KeyCode::Char('X'))), None);
        assert!(
            !screen(&mut app).contains("Purge"),
            "not from the chat list"
        );

        while !matches!(app.screen, Screen::List { selected, .. } if selected == folder_entry(Folder::Trash))
        {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(screen(&mut app).contains("X purge"));
        app.handle_key(key(KeyCode::Char('X')));
        assert!(screen(&mut app).contains("Purge the 2 chats of the trash for good?"));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);
        assert!(screen(&mut app).contains("Not purged"));
        app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::PurgeTrash)
        );
        assert_eq!(app.trash(), [1, 2]);
    }

    #[test]
    fn folders_follow_the_chat_list_and_open() {
        let mut app = app();
        app.capabilities.chat_actions = true;
        assert!(
            !screen(&mut app).contains("Archive ("),
            "no folder shown when empty"
        );
        app.file_discussion(1, Some(Folder::Archive));
        app.file_discussion(2, Some(Folder::Archive));
        app.file_discussion(3, Some(Folder::Trash));
        let text = screen(&mut app);
        assert!(text.contains("Archive (2)") && text.contains("Trash (1)"));
        assert!(!text.contains("Family Group"), "archived: out of the chats");
        assert!(text.find("Archive (2)").unwrap() < text.find("Trash (1)").unwrap());

        // Down to the archive, then in.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        let text = screen(&mut app);
        assert!(text.contains("Family Group") && text.contains("Bob Dupont"));
        assert!(!text.contains("Alice Martin"), "the inbox is not shown");
        assert!(text.contains("r restore"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            None,
            "archived already"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            Some(Action::Restore(vec![1]))
        );
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete \u{201c}Family Group\u{201d}?"));
        app.handle_key(key(KeyCode::Char('n')));

        // A chat opened from a folder goes back to it.
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Opened(1)));
        app.handle_key(key(KeyCode::Esc));
        assert!(screen(&mut app).contains("Bob Dupont"));
        assert!(!screen(&mut app).contains("Alice Martin"));

        // Esc: back to the inbox, on the folder.
        app.handle_key(key(KeyCode::Esc));
        assert!(screen(&mut app).contains("Alice Martin"));
        assert_eq!(app.handle_key(key(KeyCode::Down)), None);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        let text = screen(&mut app);
        assert!(
            text.contains("Chloe Renard") && !text.contains("Bob Dupont"),
            "in the trash"
        );
        app.handle_key(key(KeyCode::Delete));
        assert!(screen(&mut app).contains("Delete \u{201c}Chloe Renard\u{201d} for good?"));
        app.handle_key(key(KeyCode::Char('n')));

        // Emptied, the folder goes back to the inbox.
        app.file_discussion(3, None);
        let text = screen(&mut app);
        assert!(text.contains("Chloe Renard") && text.contains("Alice Martin"));
        assert!(!text.contains("Trash ("));
    }

    #[test]
    fn forwarding_offers_the_inbox_only() {
        let mut app = opened();
        app.file_discussion(1, Some(Folder::Archive));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Char('f')));
        let text = screen(&mut app);
        assert!(!text.contains("Family Group") && !text.contains("Archive ("));
    }

    #[test]
    fn new_chat_with_contacts_and_numbers() {
        let mut app = App::new(data::mock_discussions());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            None,
            "not supported"
        );
        app.capabilities.new_chat = true;
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            Some(Action::NewChat)
        );
        let contact = |name: &str, number: &str| Contact {
            name: name.to_string(),
            number: number.to_string(),
        };
        app.new_chat(vec![
            contact("Zoé", "+33612345678"),
            contact("Alain", "+33698765432"),
        ]);
        assert!(screen(&mut app).contains("Alain \u{b7} +33698765432"));
        app.handle_key(key(KeyCode::Enter));
        assert!(screen(&mut app).contains("Type a name or a number first"));
        for c in "zo".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(!screen(&mut app).contains("Alain"));
        app.handle_key(key(KeyCode::Enter));
        for c in "06 11 22 33 44,".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(screen(&mut app).contains("To: Zoé, 0611223344, Alain"));
        app.handle_key(key(KeyCode::Backspace));
        assert!(screen(&mut app).contains("To: Zoé, 0611223344,"));
        assert!(screen(&mut app).contains("Enter start the chat"));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::StartChat(vec![
                "+33612345678".to_string(),
                "0611223344".to_string()
            ]))
        );

        let idx = app.push_discussion(Discussion::new("new", "0611223344"));
        assert_eq!(app.started(idx), Action::Opened(idx));
        assert!(matches!(
            app.screen,
            Screen::Discussion {
                composing: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn a_message_not_sent_is_resent_then_removed() {
        let mut app = selected();
        let mine = app.discussions[0]
            .messages
            .iter()
            .rposition(|m| m.from_me)
            .unwrap();
        assert!(mine > 0);
        while !matches!(app.screen, Screen::Discussion { selected_msg, .. } if selected_msg == mine)
        {
            app.handle_key(key(KeyCode::Up));
        }
        app.discussions[0].messages[mine].failed = true;
        let text = screen(&mut app);
        assert!(text.contains("\u{26a0} Not sent") && text.contains("s resend"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('s'))),
            Some(Action::Resend {
                discussion: 0,
                msg: mine
            })
        );
        app.discussions[0].messages[mine].reply_to = Some(mine - 1);
        app.remove_message(0, mine - 1);
        assert_eq!(
            app.discussions[0].messages[mine - 1].reply_to,
            None,
            "its quote gone"
        );
        assert!(
            matches!(app.screen, Screen::Discussion { selected_msg, .. } if selected_msg == mine - 1),
            "the same one selected"
        );
    }

    #[test]
    fn a_message_being_sent_can_be_cancelled() {
        let mut app = selected();
        let last = app.discussions[0].messages.len() - 1;
        app.discussions[0].messages[last].sending = true;
        let text = screen(&mut app);
        assert!(text.contains("Sending\u{2026}") && text.contains("c cancel sending"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            None,
            "no reply to it yet"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Some(Action::CancelSend {
                discussion: 0,
                msg: last
            })
        );
        // Esc goes back as usual, the message still being sent.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::List { .. }));
    }
}
