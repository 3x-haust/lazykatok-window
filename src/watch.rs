use crate::types::RawMessage;
use chrono::SecondsFormat;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const WATCH_EVENT_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMessageChange {
    Existing,
    Inserted,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchEvent {
    State {
        schema_version: u8,
        state: WatchState,
        poll: u64,
        observed_messages: usize,
        observed_chats: usize,
        emitted_messages: usize,
        archived_messages: Option<usize>,
        chunks: Option<usize>,
    },
    Message {
        schema_version: u8,
        change: WatchMessageChange,
        poll: u64,
        message: RawMessage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchState {
    Started,
    Reading,
    Synced,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyCommand {
    Send(String),
    Quit,
    Help,
    Empty,
    Ignored,
}

const REPLY_HISTORY_LIMIT: usize = 2_000;
pub const REPLY_PROMPT: &str = "reply> ";

/// Terminal-independent state for the interactive reply surface.
///
/// Keeping editing and layout separate from terminal I/O makes redraws deterministic and lets
/// tests prove that incoming history cannot overwrite a partially typed draft.
#[derive(Debug, Clone, Default)]
pub struct ReplyUiState {
    history: std::collections::VecDeque<String>,
    draft: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyFrame {
    pub conversation: Vec<String>,
    pub separator: String,
    pub input: String,
    pub cursor_column: u16,
}

impl ReplyUiState {
    pub fn push_line(&mut self, line: impl Into<String>) {
        if self.history.len() == REPLY_HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(sanitize_terminal_text(&line.into()));
    }

    pub fn insert(&mut self, character: char) {
        if !character.is_control() {
            self.draft.push(character);
        }
    }

    /// Paste is text insertion, never implicit submission. Line breaks are rendered as spaces so
    /// a send still requires a later, explicit Enter key event.
    pub fn paste(&mut self, text: &str) {
        for character in text.chars() {
            if matches!(character, '\r' | '\n') {
                self.draft.push(' ');
            } else if !character.is_control() {
                self.draft.push(character);
            }
        }
    }

    pub fn backspace(&mut self) {
        self.draft.pop();
    }

    pub fn take_draft(&mut self) -> String {
        std::mem::take(&mut self.draft)
    }

    pub fn frame(&self, terminal_width: u16, terminal_height: u16) -> ReplyFrame {
        // Leave the final terminal column unused. Writing into it can trigger an automatic wrap
        // on terminals whose right-margin behavior differs.
        let width = terminal_width.saturating_sub(1).max(1) as usize;
        let conversation_rows = terminal_height.saturating_sub(2) as usize;
        let mut wrapped = self
            .history
            .iter()
            .flat_map(|line| wrap_display_line(line, width))
            .collect::<Vec<_>>();
        if wrapped.len() > conversation_rows {
            wrapped = wrapped.split_off(wrapped.len() - conversation_rows);
        }

        let prompt_width = UnicodeWidthStr::width(REPLY_PROMPT);
        let visible_draft = display_suffix(&self.draft, width.saturating_sub(prompt_width));
        let input = if prompt_width >= width {
            display_prefix(REPLY_PROMPT, width)
        } else {
            format!("{REPLY_PROMPT}{visible_draft}")
        };
        let cursor_column = UnicodeWidthStr::width(input.as_str())
            .min(width)
            .try_into()
            .unwrap_or(u16::MAX);

        ReplyFrame {
            conversation: wrapped,
            separator: "─".repeat(width),
            input,
            cursor_column,
        }
    }
}

fn wrap_display_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut rows = vec![String::new()];
    let mut row_width = 0usize;
    for character in line.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if row_width > 0 && row_width + character_width > width {
            rows.push(String::new());
            row_width = 0;
        }
        rows.last_mut().expect("one reply row").push(character);
        row_width += character_width;
    }
    rows
}

fn display_suffix(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut characters = Vec::new();
    let mut used = 0usize;
    for character in value.chars().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        characters.push(character);
        used += character_width;
    }
    characters.into_iter().rev().collect()
}

fn display_prefix(value: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output
}

pub fn parse_reply_command(bytes_read: usize, line: &str) -> ReplyCommand {
    if bytes_read == 0 {
        return ReplyCommand::Quit;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        ReplyCommand::Empty
    } else if matches!(trimmed, "/quit" | "/exit" | "/q") {
        ReplyCommand::Quit
    } else if matches!(trimmed, "/help" | "/?") {
        ReplyCommand::Help
    } else if let Some(rest) = trimmed.strip_prefix("/send") {
        if rest.is_empty() {
            return ReplyCommand::Empty;
        }
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            return ReplyCommand::Ignored;
        }
        let body = rest.trim();
        if body.is_empty() {
            ReplyCommand::Empty
        } else {
            ReplyCommand::Send(body.to_string())
        }
    } else if trimmed.starts_with('/') {
        ReplyCommand::Ignored
    } else {
        ReplyCommand::Send(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct WatchSnapshot {
    initialized: bool,
    source_changed: bool,
    fingerprints: BTreeMap<MessageKey, MessageFingerprint>,
}

impl WatchSnapshot {
    pub fn diff(
        &mut self,
        messages: &[RawMessage],
        poll: u64,
        replay_existing: bool,
        chat_filter: Option<&str>,
    ) -> Vec<WatchEvent> {
        let initial = !self.initialized;
        let mut next = BTreeMap::new();
        let mut changed = Vec::new();

        for message in messages {
            let key = MessageKey::from(message);
            let fingerprint = MessageFingerprint::from(message);
            let previous = self.fingerprints.get(&key);
            next.insert(key, fingerprint.clone());

            if chat_filter.is_some_and(|chat_id| chat_id != message.chat_id.as_str()) {
                continue;
            }

            let change = match previous {
                None if initial && replay_existing => Some(WatchMessageChange::Existing),
                None if !initial => Some(WatchMessageChange::Inserted),
                Some(old) if old != &fingerprint => Some(WatchMessageChange::Updated),
                _ => None,
            };

            if let Some(change) = change {
                changed.push((message_order_key(message), change, message.clone()));
            }
        }

        self.source_changed = initial || self.fingerprints != next;
        self.fingerprints = next;
        self.initialized = true;

        changed.sort_by(|left, right| left.0.cmp(&right.0));
        changed
            .into_iter()
            .map(|(_, change, message)| WatchEvent::Message {
                schema_version: WATCH_EVENT_SCHEMA_VERSION,
                change,
                poll,
                message,
            })
            .collect()
    }

    pub fn seen_count(&self) -> usize {
        self.fingerprints.len()
    }

    pub fn source_changed(&self) -> bool {
        self.source_changed
    }
}

fn message_order_key(message: &RawMessage) -> (String, String, String) {
    (
        message.timestamp.to_rfc3339(),
        message.chat_id.clone(),
        message.message_id.clone(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MessageKey {
    account_hash: String,
    chat_id: String,
    message_id: String,
}

impl From<&RawMessage> for MessageKey {
    fn from(message: &RawMessage) -> Self {
        Self {
            account_hash: message.account_hash.clone(),
            chat_id: message.chat_id.clone(),
            message_id: message.message_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageFingerprint {
    chat_name: String,
    chat_type: String,
    sender_id: String,
    sender_nickname: String,
    timestamp: String,
    text: String,
    message_type: String,
    reply_to_message_id: Option<String>,
}

impl From<&RawMessage> for MessageFingerprint {
    fn from(message: &RawMessage) -> Self {
        Self {
            chat_name: message.chat_name.clone(),
            chat_type: message.chat_type.clone(),
            sender_id: message.sender_id.clone(),
            sender_nickname: message.sender_nickname.clone(),
            timestamp: message.timestamp.to_rfc3339(),
            text: message.text.clone(),
            message_type: message.message_type.clone(),
            reply_to_message_id: message.reply_to_message_id.clone(),
        }
    }
}

pub fn chat_count(messages: &[RawMessage]) -> usize {
    messages
        .iter()
        .map(|message| message.chat_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

pub fn format_human_message_line(change: WatchMessageChange, message: &RawMessage) -> String {
    let timestamp = message
        .timestamp
        .to_rfc3339_opts(SecondsFormat::Secs, true)
        .replace('T', " ")
        .replace('Z', " UTC");
    let room = sanitize_terminal_text(&message.chat_name);
    let sender = if message.sender_nickname.trim().is_empty() {
        sanitize_terminal_text(&message.sender_id)
    } else {
        sanitize_terminal_text(&message.sender_nickname)
    };
    let body = if message.text.trim().is_empty() {
        format!("<{}>", sanitize_terminal_text(&message.message_type))
    } else {
        sanitize_terminal_text(&message.text)
    };
    let change_label = match change {
        WatchMessageChange::Existing | WatchMessageChange::Inserted => "",
        WatchMessageChange::Updated => " (updated)",
    };

    format!("[{timestamp}] {room} / {sender}{change_label}: {body}")
}

/// Convert untrusted chat/status text into one inert terminal line.
///
/// Terminal escape protocols require control characters such as ESC, BEL, CR, or C1 controls.
/// Replacing every Unicode control before folding whitespace prevents chat content from moving
/// the cursor, changing terminal state, or opening an OSC sequence.
pub fn sanitize_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn message(chat_id: &str, message_id: &str, seconds: i64, text: &str) -> RawMessage {
        RawMessage {
            account_hash: "acct-synthetic".to_string(),
            chat_id: chat_id.to_string(),
            chat_name: format!("Synthetic {chat_id}"),
            chat_type: "direct".to_string(),
            message_id: message_id.to_string(),
            sender_id: "sender-1".to_string(),
            sender_nickname: "Tester".to_string(),
            timestamp: Utc.timestamp_opt(seconds, 0).single().expect("timestamp"),
            text: text.to_string(),
            message_type: "text".to_string(),
            reply_to_message_id: None,
        }
    }

    fn message_ids(events: &[WatchEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                WatchEvent::Message { message, .. } => Some(message.message_id.clone()),
                WatchEvent::State { .. } => None,
            })
            .collect()
    }

    #[test]
    fn first_snapshot_is_quiet_unless_replay_is_requested() {
        let mut snapshot = WatchSnapshot::default();
        let messages = vec![message("chat-a", "a-1", 1, "one")];

        assert!(snapshot.diff(&messages, 1, false, None).is_empty());

        let mut replay = WatchSnapshot::default();
        let events = replay.diff(&messages, 1, true, None);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            WatchEvent::Message {
                change: WatchMessageChange::Existing,
                ..
            }
        ));
    }

    #[test]
    fn later_snapshots_emit_inserted_and_updated_messages_in_send_order() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![
            message("chat-a", "a-1", 20, "seed"),
            message("chat-b", "b-1", 10, "seed"),
        ];
        snapshot.diff(&first, 1, false, None);

        let mut edited = first.clone();
        edited[0].text = "edited".to_string();
        edited.push(message("chat-a", "a-2", 30, "new"));

        let events = snapshot.diff(&edited, 2, false, None);
        assert_eq!(message_ids(&events), ["a-1", "a-2"]);
        assert!(matches!(
            events[0],
            WatchEvent::Message {
                change: WatchMessageChange::Updated,
                ..
            }
        ));
        assert!(matches!(
            events[1],
            WatchEvent::Message {
                change: WatchMessageChange::Inserted,
                ..
            }
        ));
    }

    #[test]
    fn chat_filter_limits_emitted_messages_without_forgetting_snapshot_state() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![
            message("chat-a", "a-1", 1, "seed"),
            message("chat-b", "b-1", 2, "seed"),
        ];
        snapshot.diff(&first, 1, false, Some("chat-a"));

        let mut second = first.clone();
        second.push(message("chat-a", "a-2", 3, "visible"));
        second.push(message("chat-b", "b-2", 4, "hidden"));

        let events = snapshot.diff(&second, 2, false, Some("chat-a"));
        assert_eq!(message_ids(&events), ["a-2"]);
        assert_eq!(snapshot.seen_count(), 4);
        assert_eq!(chat_count(&second), 2);
    }

    #[test]
    fn human_message_lines_are_readable_and_single_line() {
        let mut item = message("chat-a", "a-1", 1, "hello\nthere");
        item.chat_name = "Synthetic\nRoom".to_string();
        item.sender_nickname = "Tester One".to_string();

        assert_eq!(
            format_human_message_line(WatchMessageChange::Existing, &item),
            "[1970-01-01 00:00:01 UTC] Synthetic Room / Tester One: hello there"
        );

        item.text.clear();
        item.message_type = "photo".to_string();
        assert_eq!(
            format_human_message_line(WatchMessageChange::Updated, &item),
            "[1970-01-01 00:00:01 UTC] Synthetic Room / Tester One (updated): <photo>"
        );
    }

    #[test]
    fn human_message_lines_remove_terminal_escape_and_control_characters() {
        let mut item = message(
            "chat-a",
            "a-1",
            1,
            "safe\u{1b}]0;owned\u{7} body\r\nnext\u{009b}2J",
        );
        item.chat_name = "Room\u{1b}[2J".to_string();
        item.sender_nickname = "Tester\tName\u{7}".to_string();

        let rendered = format_human_message_line(WatchMessageChange::Inserted, &item);

        assert_eq!(
            rendered,
            "[1970-01-01 00:00:01 UTC] Room [2J / Tester Name: safe ]0;owned body next 2J"
        );
        assert!(!rendered.chars().any(char::is_control));
    }

    #[test]
    fn reply_ui_sanitizes_every_history_line_before_rendering() {
        let mut ui = ReplyUiState::default();
        ui.push_line("status\u{1b}]8;;https://example.invalid\u{7}link\u{1b}]8;;\u{7}\nnext");

        let frame = ui.frame(120, 5);
        assert!(frame
            .conversation
            .iter()
            .all(|line| !line.chars().any(char::is_control)));
    }

    #[test]
    fn reply_line_parser_sends_direct_lines_and_keeps_slash_commands_explicit() {
        assert_eq!(parse_reply_command(0, ""), ReplyCommand::Quit);
        assert_eq!(parse_reply_command(1, "\n"), ReplyCommand::Empty);
        assert_eq!(parse_reply_command(4, "   \n"), ReplyCommand::Empty);
        assert_eq!(parse_reply_command(6, "/quit\n"), ReplyCommand::Quit);
        assert_eq!(parse_reply_command(6, "/help\n"), ReplyCommand::Help);
        assert_eq!(
            parse_reply_command(25, "/send   확인했습니다  \n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(25, "/send\t확인했습니다\n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(25, "/send\u{00a0}확인했습니다\n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(25, "/send확인했습니다\n"),
            ReplyCommand::Ignored
        );
        assert_eq!(
            parse_reply_command(20, "확인했습니다\n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(20, "/unknown 확인했습니다\n"),
            ReplyCommand::Ignored
        );
    }

    #[test]
    fn reply_ui_keeps_korean_draft_intact_while_history_arrives() {
        let mut ui = ReplyUiState::default();
        for character in "확인 중".chars() {
            ui.insert(character);
        }
        ui.push_line("[now] Synthetic Room / Tester: 새 메시지");

        let frame = ui.frame(80, 8);
        assert_eq!(
            frame.conversation,
            ["[now] Synthetic Room / Tester: 새 메시지"]
        );
        assert_eq!(frame.input, "reply> 확인 중");

        ui.backspace();
        assert_eq!(ui.take_draft(), "확인 ");
    }

    #[test]
    fn reply_ui_resize_wraps_history_without_changing_draft() {
        let mut ui = ReplyUiState::default();
        ui.push_line("abcdefghij");
        ui.paste("한글\n붙여넣기");

        let narrow = ui.frame(7, 5);
        assert_eq!(narrow.conversation, ["abcdef", "ghij"]);
        assert_eq!(narrow.separator, "──────");
        assert_eq!(narrow.input, "reply>");

        let wide = ui.frame(40, 5);
        assert_eq!(wide.conversation, ["abcdefghij"]);
        assert_eq!(wide.input, "reply> 한글 붙여넣기");
        assert_eq!(ui.take_draft(), "한글 붙여넣기");
    }

    #[test]
    fn reply_ui_paste_never_submits_and_history_is_bounded() {
        let mut ui = ReplyUiState::default();
        ui.paste("first\r\nsecond");
        for index in 0..=REPLY_HISTORY_LIMIT {
            ui.push_line(format!("line {index}"));
        }

        assert_eq!(ui.take_draft(), "first  second");
        let frame = ui.frame(80, 4);
        assert_eq!(frame.conversation, ["line 1999", "line 2000"]);
    }

    #[test]
    fn snapshot_tracks_full_source_changes_even_when_they_are_filtered_from_display() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![
            message("chat-a", "a-1", 1, "seed"),
            message("chat-b", "b-1", 2, "seed"),
        ];

        snapshot.diff(&first, 1, false, Some("chat-a"));
        assert!(snapshot.source_changed(), "the baseline must be archived");

        snapshot.diff(&first, 2, false, Some("chat-a"));
        assert!(!snapshot.source_changed(), "an identical poll is redundant");

        let mut hidden_change = first.clone();
        hidden_change[1].text = "changed outside display filter".to_string();
        let events = snapshot.diff(&hidden_change, 3, false, Some("chat-a"));
        assert!(events.is_empty(), "the chat filter still controls display");
        assert!(
            snapshot.source_changed(),
            "archive sync must see changes outside the display filter"
        );
    }
}
