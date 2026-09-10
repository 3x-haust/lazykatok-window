use crate::kakao::{AuthOptions, ReaderOutput};
use crate::{types::RawMessage, Result};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait SourceAdapter {
    fn chats(&self) -> Result<Vec<ChatSummary>>;
    fn messages(&self) -> Result<Vec<RawMessage>>;
    fn coverage(&self) -> Option<SourceCoverage> {
        None
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceCoverage {
    pub available_chats: usize,
    pub unavailable_chats: usize,
}

#[cfg(windows)]
#[derive(Default)]
pub struct WindowsAdapter {
    cached: std::cell::OnceCell<crate::kakao::windows::native::ReadOutput>,
}

#[cfg(windows)]
impl WindowsAdapter {
    fn read(&self) -> Result<&crate::kakao::windows::native::ReadOutput> {
        if self.cached.get().is_none() {
            let _ = self.cached.set(crate::kakao::windows::read()?);
        }
        Ok(self.cached.get().expect("successful source read"))
    }
}
#[cfg(windows)]
impl SourceAdapter for WindowsAdapter {
    fn chats(&self) -> Result<Vec<ChatSummary>> {
        Ok(self.read()?.chats.clone())
    }
    fn messages(&self) -> Result<Vec<RawMessage>> {
        let output = self.read()?;
        if output.available == 0 && output.locked > 0 {
            return Err(crate::Error::Kakao(
                "No Windows room databases are readable. Open a room in KakaoTalk and retry."
                    .into(),
            ));
        }
        Ok(output.messages.clone())
    }
    fn coverage(&self) -> Option<SourceCoverage> {
        self.cached.get().map(|o| SourceCoverage {
            available_chats: o.available,
            unavailable_chats: o.locked,
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ChatSummary {
    pub chat_id: String,
    pub chat_name: String,
    pub chat_type: String,
    /// Latest message time the source can report; used to order chat lists by
    /// recency. Sources without per-chat timestamps leave this None.
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
}

pub struct FixtureAdapter {
    path: std::path::PathBuf,
}

impl FixtureAdapter {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl SourceAdapter for FixtureAdapter {
    fn chats(&self) -> Result<Vec<ChatSummary>> {
        let mut latest = std::collections::BTreeMap::new();
        for message in self.messages()? {
            latest
                .entry(message.chat_id.clone())
                .and_modify(|chat: &mut ChatSummary| {
                    if Some(message.timestamp) > chat.last_message_at {
                        chat.last_message_at = Some(message.timestamp);
                    }
                })
                .or_insert_with(|| ChatSummary {
                    chat_id: message.chat_id.clone(),
                    chat_name: message.chat_name.clone(),
                    chat_type: message.chat_type.clone(),
                    last_message_at: Some(message.timestamp),
                });
        }
        Ok(latest.into_values().collect())
    }

    fn messages(&self) -> Result<Vec<RawMessage>> {
        crate::fixture::read_fixture(&self.path)
    }
}

/// Reads the current macOS KakaoTalk encrypted database natively (no Python,
/// no `kakaocli`). One read is shared between `chats()` and `messages()`: the
/// first call decrypts + scans and memoizes the result, so a caller that
/// invokes both on the same instance pays the decrypt once.
pub struct MacosAdapter {
    options: AuthOptions,
    cached: std::cell::OnceCell<ReaderOutput>,
}

impl MacosAdapter {
    /// Build an adapter for the given `home` and katok `data_dir`.
    pub fn new(home: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            options: AuthOptions::new(home, data_dir),
            cached: std::cell::OnceCell::new(),
        }
    }

    /// Read the databases once and memoize the output. Subsequent calls clone
    /// the cached `ReaderOutput` instead of re-resolving auth and re-decrypting.
    /// Errors are not cached, so a transient failure can be retried.
    fn read(&self) -> Result<ReaderOutput> {
        if let Some(output) = self.cached.get() {
            return Ok(output.clone());
        }
        let output = crate::kakao::read_kakao_with_options(&self.options)?;
        // Ignore a lost race: `get_or_init` is not fallible-friendly, so set and
        // re-read; on the (single-threaded) common path this stores our value.
        let _ = self.cached.set(output.clone());
        Ok(output)
    }
}

impl SourceAdapter for MacosAdapter {
    fn chats(&self) -> Result<Vec<ChatSummary>> {
        let output = self.read()?;
        let mut latest: std::collections::HashMap<String, DateTime<Utc>> =
            std::collections::HashMap::new();
        for message in &output.messages {
            let entry = latest
                .entry(message.chat_id.clone())
                .or_insert(message.timestamp);
            if message.timestamp > *entry {
                *entry = message.timestamp;
            }
        }
        Ok(output
            .chats
            .into_iter()
            .map(|chat| {
                let chat_id = chat.chat_id;
                ChatSummary {
                    last_message_at: latest.get(&chat_id).copied(),
                    chat_id,
                    chat_name: chat.chat_name,
                    chat_type: chat.chat_type,
                }
            })
            .collect())
    }

    fn messages(&self) -> Result<Vec<RawMessage>> {
        Ok(self.read()?.messages)
    }
}

pub struct KakaocliAdapter;

impl SourceAdapter for KakaocliAdapter {
    fn chats(&self) -> Result<Vec<ChatSummary>> {
        let output = run_kakaocli("chats")?;
        parse_kakaocli_json("chats", &output)
    }

    fn messages(&self) -> Result<Vec<RawMessage>> {
        let output = run_kakaocli("messages")?;
        parse_kakaocli_json("messages", &output)
    }
}

fn parse_kakaocli_json<T>(command: &str, bytes: &[u8]) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(|err| {
        crate::Error::Kakaocli(format!("kakaocli {command} returned invalid JSON: {err}"))
    })
}

fn run_kakaocli(command: &str) -> Result<Vec<u8>> {
    let output = Command::new("kakaocli")
        .arg(command)
        .arg("--json")
        .output()
        .map_err(|err| {
            crate::Error::Kakaocli(format!(
                "kakaocli not found on PATH; install kakaocli or ensure it is executable ({err})"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            "no stderr output (run `kakaocli auth` to check database access)"
        } else {
            detail
        };
        return Err(crate::Error::Kakaocli(format!(
            "kakaocli {command} failed ({}): {detail}",
            output.status
        )));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_line(chat_id: &str, message_id: &str, timestamp: &str) -> String {
        format!(
            "{{\"account_hash\":\"acct-synthetic\",\"chat_id\":\"{chat_id}\",\
             \"chat_name\":\"Room {chat_id}\",\"chat_type\":\"group\",\
             \"message_id\":\"{message_id}\",\"sender_id\":\"u1\",\
             \"sender_nickname\":\"테스터\",\"timestamp\":\"{timestamp}\",\
             \"text\":\"합성 메시지\",\"message_type\":\"text\",\
             \"reply_to_message_id\":null}}\n"
        )
    }

    #[test]
    fn fixture_chats_carry_latest_message_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chats.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}{}{}{}",
                fixture_line("chat-a", "a1", "2026-01-01T09:00:00Z"),
                fixture_line("chat-b", "b1", "2026-01-01T10:00:00Z"),
                fixture_line("chat-a", "a2", "2026-01-02T08:00:00Z"),
                fixture_line("chat-b", "b2", "2026-01-01T09:30:00Z"),
            ),
        )
        .expect("write fixture");

        let chats = FixtureAdapter::new(&path).chats().expect("read chats");
        fn instant(iso: &str) -> DateTime<Utc> {
            chrono::DateTime::parse_from_rfc3339(iso)
                .expect("parse timestamp")
                .with_timezone(&Utc)
        }
        let mut seen = std::collections::BTreeMap::new();
        for chat in chats {
            seen.insert(chat.chat_id.clone(), chat.last_message_at);
        }
        assert_eq!(
            seen.get("chat-a").copied().flatten(),
            Some(instant("2026-01-02T08:00:00Z"))
        );
        assert_eq!(
            seen.get("chat-b").copied().flatten(),
            Some(instant("2026-01-01T10:00:00Z"))
        );
    }
}
