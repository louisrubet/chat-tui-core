//! What a messaging service provides to the interface: a [`Backend`] carrying out what the
//! user does, and a stream of [`Event`]s for what happens elsewhere.

use std::error::Error;
use std::future::Future;
use std::time::Duration;

use crate::data::{Attachment, ChatId, Discussion, Message, MessageId, Styled};
use crate::recipients::Contact;

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// What the backend can do, for the interface to offer only that.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// How long after sending our messages can be deleted for everyone; `None`: never.
    pub delete_max_age: Option<Duration>,
    /// Whether any message can be deleted for us only (from our devices), instead of ours
    /// for everyone: [`Backend::delete`] then removes it from the chat.
    pub delete_for_me: bool,
    /// Whether chats can be archived, deleted and restored ([`Backend::archive`],
    /// [`Backend::delete_chat`], [`Backend::restore`]).
    pub chat_actions: bool,
    /// Whether chats can be started ([`Backend::contacts`], [`Backend::start_chat`]).
    pub new_chat: bool,
}

/// A message to send.
#[derive(Debug, Clone)]
pub struct Draft {
    pub text: String,
    pub styles: Vec<Styled>,
    pub attachments: Vec<Attachment>,
    /// The message it replies to, in the same chat.
    pub reply_to: Option<Message>,
    pub forwarded: bool,
}

/// What was downloaded for a message: its attachments, and the images of its link previews
/// in order (`None` for those that failed).
pub type Downloaded = (Vec<Attachment>, Vec<Option<Attachment>>);

/// Something that happened elsewhere: received, or done from another of our devices.
// A few events a second at most: not worth boxing the big ones.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Event {
    /// A chat not known yet; its messages follow. For a known one: its title, kind, folder
    /// or visibility changed (its messages and pin are kept).
    Chat(Discussion),
    /// A new message, with the id of the one it quotes. Its images may be downloaded
    /// already; other attachments wait for the user to open them.
    Message {
        chat: ChatId,
        message: Message,
        quote: Option<MessageId>,
    },
    /// Earlier messages of a chat, each with the id of the one it quotes, in any order:
    /// merged with those shown (the known ones replaced), without notifying.
    History {
        chat: ChatId,
        messages: Vec<(Message, Option<MessageId>)>,
    },
    /// A reaction set, or removed with `None`.
    Reaction {
        chat: ChatId,
        target: MessageId,
        author: String,
        mine: bool,
        emoji: Option<String>,
    },
    /// A message deleted for everyone by `author` (only its author can do that).
    Deleted {
        chat: ChatId,
        target: MessageId,
        author: String,
    },
    /// Someone started or stopped typing.
    Typing {
        chat: ChatId,
        author: String,
        name: String,
        started: bool,
    },
    /// A chat read on another device: none of its messages is new any more.
    Read { chat: ChatId },
    /// A chat marked unread on another device, whose last message there is `latest`: its
    /// last message received is new again. Not for the chat open, nor while `latest` is not
    /// shown yet (arriving, it is new anyway).
    Unread { chat: ChatId, latest: MessageId },
    /// Messages received while the interface was not running, once their history came:
    /// one notification sums up those still unread.
    Missed(Vec<(ChatId, MessageId)>),
    /// Shown in the footer, e.g. a lost connection.
    Status(String),
}

/// A messaging service, as the interface uses it.
///
/// The futures returned are `'static`: they own what they need (clones of the client…),
/// so that downloads can run in the background while the interface goes on. They run on
/// the current thread (in a [`tokio::task::LocalSet`]): nothing needs to be `Send`.
pub trait Backend {
    fn capabilities(&self) -> Capabilities;

    /// Sends `draft` to `chat`, and returns the message as sent.
    fn send(&self, chat: &ChatId, draft: Draft) -> impl Future<Output = Result<Message>> + 'static;

    /// Sets our reaction to `message`, or removes it.
    fn react(
        &self,
        chat: &ChatId,
        message: &Message,
        emoji: &str,
        remove: bool,
    ) -> impl Future<Output = Result<()>> + 'static;

    /// Deletes our `message` for everyone; with [`Capabilities::delete_for_me`], deletes
    /// `message` (any) for us only.
    fn delete(
        &self,
        chat: &ChatId,
        message: &Message,
    ) -> impl Future<Output = Result<()>> + 'static;

    /// Downloads the attachments and link preview images of `message`, received without them.
    fn download(
        &self,
        chat: &ChatId,
        message: &Message,
    ) -> impl Future<Output = Result<Downloaded>> + 'static;

    /// Archives `chat`: out of the inbox, back on a new message.
    fn archive(&self, _chat: &ChatId) -> impl Future<Output = Result<()>> + 'static {
        async { Err("archiving is not supported".into()) }
    }

    /// Deletes `chat` and its messages: to the trash, or for good from there. Its new place
    /// (trash, or gone) comes as an [`Event::Chat`].
    fn delete_chat(&self, _chat: &ChatId) -> impl Future<Output = Result<()>> + 'static {
        async { Err("deleting chats is not supported".into()) }
    }

    /// Moves `chat` back to the inbox, from a folder.
    fn restore(&self, _chat: &ChatId) -> impl Future<Output = Result<()>> + 'static {
        async { Err("restoring chats is not supported".into()) }
    }

    /// Removes our `message` of `chat`, not sent (and sent again): from the other devices.
    /// Nothing by default.
    fn discard(
        &self,
        _chat: &ChatId,
        _message: &Message,
    ) -> impl Future<Output = Result<()>> + 'static {
        async { Ok(()) }
    }

    /// The address book, to start chats with.
    fn contacts(&self) -> impl Future<Output = Result<Vec<Contact>>> + 'static {
        async { Ok(Vec::new()) }
    }

    /// The chat with the people at `numbers` (a group for several), started unless it
    /// exists already. Its messages, if any, follow as [`Event::History`].
    fn start_chat(
        &self,
        _numbers: Vec<String>,
    ) -> impl Future<Output = Result<Discussion>> + 'static {
        async { Err("starting chats is not supported".into()) }
    }

    /// `chat` was read here up to its `last` message: for the other devices to know.
    /// Nothing by default.
    fn read(&self, _chat: &ChatId, _last: &Message) {}

    /// We are typing in `chat`: said while we do, every few seconds at most. Nothing by
    /// default.
    fn typing(&self, _chat: &ChatId) {}
}
