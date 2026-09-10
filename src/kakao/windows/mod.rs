//! Windows KakaoTalk source. Database handles are read-only; keys stay in memory.
//! Format research: MoniKa (MIT), see THIRD_PARTY_NOTICES.md.
use crate::{adapters::ChatSummary, types::RawMessage, Error, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[cfg(windows)]
pub(crate) mod native;
#[cfg(windows)]
pub mod send;
#[cfg(windows)]
pub use native::{probe_status, read};

fn source_error(message: &str) -> Error {
    Error::Kakao(message.to_owned())
}

/// Enumerate profiles without guessing which account is currently signed in.
pub fn discover_profiles(base: &Path) -> Result<Vec<PathBuf>> {
    let mut profiles = Vec::new();
    let users = base.join("users");
    if users.is_dir() {
        for entry in std::fs::read_dir(users)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("chat_data").is_dir() {
                profiles.push(entry.path());
            }
        }
    }
    if base.join("chat_data").is_dir() {
        profiles.push(base.to_path_buf());
    }
    profiles.sort();
    Ok(profiles)
}

/// Apply a raw, per-database key. SQLCipher validates page authentication on read.
/// No decrypted database or key is written to a temporary directory.
pub fn open_database(path: &Path, key: &[u8; 32]) -> Result<Connection> {
    use zeroize::Zeroizing;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| {
        source_error("Windows database is unavailable; close competing readers and retry")
    })?;
    let hex = Zeroizing::new(key.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let pragma = Zeroizing::new(format!(
        "PRAGMA key=\"x'{}'\"; PRAGMA cipher_page_size=4096; PRAGMA query_only=ON;",
        &*hex
    ));
    conn.execute_batch(&pragma)
        .map_err(|_| source_error("Windows database key setup failed"))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|_| {
        source_error(
            "Windows database could not be authenticated; open the room in KakaoTalk and retry",
        )
    })?;
    Ok(conn)
}

pub fn read_rooms(conn: &Connection) -> Result<Vec<ChatSummary>> {
    let mut stmt = conn.prepare("SELECT CAST(chatId AS TEXT), type, chatRoomTitle, lastUpdatedAt FROM chatRoomList ORDER BY chatId")
        .map_err(|_| source_error("Unsupported Windows chat-list schema; run doctor and check the KakaoTalk version"))?;
    let rows = stmt.query_map([], |r| {
        let id: String = r.get(0)?;
        let kind: Option<String> = r.get(1)?;
        let title: Option<String> = r.get(2)?;
        let time: Option<i64> = r.get(3)?;
        Ok(ChatSummary {
            chat_name: title
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("Chat {id}")),
            chat_id: id,
            chat_type: match kind.as_deref() {
                Some("DirectChat" | "MemoChat") => "direct",
                _ => "group",
            }
            .into(),
            last_message_at: time.and_then(timestamp),
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| source_error("Invalid Windows chat-list row; sync was not completed"))
}

fn timestamp(value: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if value > 100_000_000_000 {
        chrono::DateTime::from_timestamp_millis(value)
    } else {
        chrono::DateTime::from_timestamp(value, 0)
    }
}

pub fn read_messages(
    conn: &Connection,
    room: &ChatSummary,
    account_hash: &str,
    names: &HashMap<String, String>,
) -> Result<Vec<RawMessage>> {
    let mut stmt = conn.prepare("SELECT CAST(logId AS TEXT), CAST(authorId AS TEXT), message, type, sendAt FROM chatLogs WHERE deleted IS NULL OR deleted=0 ORDER BY sendAt, logId")
        .map_err(|_| source_error("Unsupported Windows message schema; sync was not completed"))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut result = Vec::new();
    for row in rows {
        let (id, sender, text, kind, time) =
            row.map_err(|_| source_error("Invalid Windows message row; sync was not completed"))?;
        let timestamp =
            timestamp(time).ok_or_else(|| source_error("Invalid Windows message timestamp"))?;
        result.push(RawMessage {
            account_hash: account_hash.into(),
            chat_id: room.chat_id.clone(),
            chat_name: room.chat_name.clone(),
            chat_type: room.chat_type.clone(),
            message_id: id,
            sender_nickname: names
                .get(&sender)
                .cloned()
                .unwrap_or_else(|| format!("User {sender}")),
            sender_id: sender,
            timestamp,
            text: text.unwrap_or_default(),
            message_type: match kind.unwrap_or(0) {
                1 => "text",
                2 => "image",
                3 => "video",
                5 => "audio",
                18 => "file",
                26 => "reply",
                _ => "other",
            }
            .into(),
            reply_to_message_id: None,
        });
    }
    Ok(result)
}

#[cfg(windows)]
fn read_names(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT CAST(userId AS TEXT), COALESCE(NULLIF(friendNickName,''), nickName) FROM talkUser WHERE linkId=0")
        .map_err(|_| source_error("Unsupported Windows contact schema"))?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut names = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            names.insert(id, name);
        }
    }
    Ok(names)
}
