# Changelog

## v1.11.2

- The chat list (and the folders) in order of the last message, the most recent first: a chat written to goes up; pinned chats stay in their place. Chats without messages come last.
- "Name ..." shown beside the title when the end of the discussion is out of view (opened on the first new message, or reading further up): it was hidden at the bottom.

## v1.11.1

- Published on crates.io as `chat-tui-core` (the name chat-tui was taken there); the library is still `chat_tui`. The GitHub repository is renamed chat-tui-core too.
- emoji-expander 1.2.1, from crates.io.

## v1.11.0

- Deleting any message for oneself (`Capabilities::delete_for_me`); typing told to the backend (`Backend::typing`).

## v1.10.0

- Messages sent in the background, cancelled with `c`; those not sent shown in red, notified, sent again with `s`.

## v1.9.0

- New chats with contacts or numbers; one notification for the messages missed.

## v1.8.1

- Space marks a chat without moving to the next one.

## v1.8.0

- Find, marks and batch actions, purge of the trash, order of the pinned chats.

## v1.7.0

- The version of the application beside its name.

## v1.6.0

- Archive, spam and trash folders.

## v1.5.0

- Archiving and deleting chats.

## v1.4.0

- Hidden chats; chats read on other devices.

## v1.3.2

- Images of live messages fetched on arrival.

## v1.3.1

- Back from a chat or the parameters, the chat list keeps its view: the chat left is no longer pushed to the last row.

## v1.3.0

- The name of the application (`Config::name`) heads the chat list, in bold, above "Pinned" or "Chats".

## v1.2.1

- A message pushed or received again (same id: sent here and echoed back, updated) replaces the one shown: not added twice, not notified, not counted as new.

## v1.2.0

- Esc gives up a message being sent, whatever blocks it: "Sending cancelled" in the status line, the interface free again.
- `mock_tui`: a message starting with `/hang` is never sent, to try it.

## v1.1.2

- No panic drawing the chat list or the parameters in a terminal of no rows.

## v1.1.1

- Unread counts above 20 in digits (`21`, `99+`): circled ㉑ and up took two columns and stuck out on the left.
- History arriving after a chat was opened: its images are fetched too.

## v1.1.0

- `Event::History`: earlier messages of a chat, merged with those shown (known ones replaced, send order kept), without notifying. Selection, quotes and the reply being written keep pointing to their message.

## v1.0.0

Stable API: same as v0.1.0, now used by signal-tui.

## v0.1.0

First release, extracted from signal-tui v1.3.1.

- `Backend` trait: `capabilities`, `send`, `react`, `delete`, `download`.
- `Event` stream: `Chat`, `Message`, `Reaction`, `Deleted`, `Typing`, `Status`.
- `run(backend, chats, events, config)`, `Config::new(name)`.
- Chats and messages identified by strings (`ChatId`, `MessageId`), send time in `Message::sent_ms`.
- Settings, pinned chats and read marks kept by chat-tui.
