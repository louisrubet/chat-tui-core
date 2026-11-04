//! What the interface remembers between runs: when each chat was last read, and the
//! pinned chats. Small text files, keyed by [`ChatId`].

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use crate::data::{ChatId, Discussion};

/// When each chat was last read: one `<chat> <send time of the newest message read>` per
/// line.
pub struct ReadMarks {
    path: PathBuf,
    marks: HashMap<ChatId, u64>,
}

impl ReadMarks {
    /// Loads the marks; a missing file means nothing was read yet.
    pub fn load(path: PathBuf) -> Self {
        let marks = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (key, ms) = line.split_once(' ')?;
                Some((key.to_string(), ms.parse().ok()?))
            })
            .collect();
        ReadMarks { path, marks }
    }

    /// Number of messages of `discussion` received after it was last read.
    pub fn unread_count(&self, discussion: &Discussion) -> usize {
        let last_read = self.marks.get(&discussion.id).copied().unwrap_or(0);
        discussion
            .messages
            .iter()
            .filter(|m| !m.from_me && m.sent_ms > last_read)
            .count()
    }

    /// Records that every message of `discussion` is read, and saves.
    pub fn mark_read(&mut self, discussion: &Discussion) -> io::Result<()> {
        let Some(newest) = discussion.messages.iter().map(|m| m.sent_ms).max() else {
            return Ok(());
        };
        let mark = self.marks.entry(discussion.id.clone()).or_default();
        if *mark >= newest {
            return Ok(());
        }
        *mark = newest;
        self.save()
    }

    /// Records that the last message received in `discussion` is not read, as if it had
    /// just come, and saves. Whether it was marked: not without a message received.
    pub fn mark_unread(&mut self, discussion: &Discussion) -> io::Result<bool> {
        let Some(last) = discussion.messages.iter().rev().find(|m| !m.from_me) else {
            return Ok(false);
        };
        self.marks
            .insert(discussion.id.clone(), last.sent_ms.saturating_sub(1));
        self.save().map(|()| true)
    }

    fn save(&self) -> io::Result<()> {
        let content: String = self
            .marks
            .iter()
            .map(|(key, ms)| format!("{key} {ms}\n"))
            .collect();
        write(&self.path, content)
    }
}

/// The pinned chats, in pin order: one per line.
pub struct Pins {
    path: PathBuf,
    keys: Vec<ChatId>,
}

impl Pins {
    /// Loads the pins; a missing file means nothing is pinned.
    pub fn load(path: PathBuf) -> Self {
        let keys = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        Pins { path, keys }
    }

    /// Rank of `chat` in the pinned list, if pinned.
    pub fn rank(&self, chat: &str) -> Option<u32> {
        self.keys
            .iter()
            .position(|k| k == chat)
            .map(|rank| rank as u32)
    }

    /// Pins `chat` at the end of the list, or unpins it, and saves.
    pub fn set(&mut self, chat: &str, pinned: bool) -> io::Result<()> {
        self.keys.retain(|k| k != chat);
        if pinned {
            self.keys.push(chat.to_string());
        }
        self.save()
    }

    /// Puts the pinned `chats` in this order, before the other pinned ones (unknown to
    /// `chats`), and saves.
    pub fn set_order(&mut self, mut chats: Vec<ChatId>) -> io::Result<()> {
        chats.retain(|c| self.keys.contains(c));
        let others = self
            .keys
            .iter()
            .filter(|k| !chats.contains(k))
            .cloned()
            .collect::<Vec<_>>();
        chats.extend(others);
        self.keys = chats;
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let content: String = self.keys.iter().map(|key| format!("{key}\n")).collect();
        write(&self.path, content)
    }
}

/// Writes `content` to `path`, creating its directory.
fn write(path: &PathBuf, content: String) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Message;

    #[test]
    fn pins_are_reordered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned");
        let mut pins = Pins::load(path.clone());
        for chat in ["a", "b", "c"] {
            pins.set(chat, true).unwrap();
        }
        pins.set_order(vec!["c".into(), "a".into(), "x".into()])
            .unwrap();
        let pins = Pins::load(path);
        assert_eq!(
            ["a", "b", "c", "x"].map(|c| pins.rank(c)),
            [Some(1), Some(2), Some(0), None],
            "reordered, b kept after, x not pinned"
        );
    }

    #[test]
    fn read_marks_count_what_came_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read");
        let mut discussion = Discussion::new("alice", "Alice");
        let received = |ms| Message {
            from_me: false,
            sent_ms: ms,
            ..Message::mine(ms, String::new(), None, false)
        };
        discussion.messages = vec![received(1), received(2)];
        let mut marks = ReadMarks::load(path.clone());
        assert_eq!(marks.unread_count(&discussion), 2);
        marks.mark_read(&discussion).unwrap();
        discussion.messages.push(received(3));
        assert_eq!(ReadMarks::load(path).unread_count(&discussion), 1, "saved");
    }

    #[test]
    fn marked_unread_the_last_message_received_is_new_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read");
        let received = |ms| Message {
            from_me: false,
            sent_ms: ms,
            ..Message::mine(ms, String::new(), None, false)
        };
        let mut discussion = Discussion::new("alice", "Alice");
        let mut marks = ReadMarks::load(path.clone());
        assert!(!marks.mark_unread(&discussion).unwrap(), "nothing received");
        discussion.messages = vec![
            received(1),
            received(2),
            Message::mine(3, String::new(), None, false),
        ];
        marks.mark_read(&discussion).unwrap();
        assert!(marks.mark_unread(&discussion).unwrap());
        assert_eq!(ReadMarks::load(path).unread_count(&discussion), 1, "saved");
        marks.mark_read(&discussion).unwrap();
        assert_eq!(marks.unread_count(&discussion), 0, "read again");
    }

    #[test]
    fn pins_keep_their_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("pinned");
        let mut pins = Pins::load(path.clone());
        pins.set("a", true).unwrap();
        pins.set("b", true).unwrap();
        pins.set("a", false).unwrap();
        pins.set("a", true).unwrap();
        let pins = Pins::load(path);
        assert_eq!(
            (pins.rank("b"), pins.rank("a"), pins.rank("c")),
            (Some(0), Some(1), None)
        );
    }
}
