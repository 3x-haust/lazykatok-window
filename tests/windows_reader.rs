//! Synthetic Windows database contracts. Never opens a real KakaoTalk installation.
use lazykatok::kakao::windows::{discover_profiles, open_database, read_messages, read_rooms};
use rusqlite::Connection;
use std::collections::HashMap;

#[test]
fn windows_text_validation_rejects_invalid_input_before_accessing_an_app() {
    use lazykatok::kakao::windows::validate_text;
    for text in ["", " \r\n\t", "synthetic\0text"] {
        assert!(validate_text(text).is_err());
    }
    assert!(validate_text("한글\n둘째 줄 😀").is_ok());
    assert!(validate_text(&"😀".repeat(5000)).is_ok());
    assert!(validate_text(&"😀".repeat(5001)).is_err());
}

#[test]
fn windows_profile_discovery_keeps_accounts_separate() {
    let root = tempfile::tempdir().unwrap();
    for name in ["synthetic-a", "synthetic-b"] {
        std::fs::create_dir_all(root.path().join("users").join(name).join("chat_data")).unwrap();
    }
    let profiles = discover_profiles(root.path()).unwrap();
    assert_eq!(profiles.len(), 2);
    assert_ne!(profiles[0], profiles[1]);
}

#[test]
fn windows_reader_uses_stable_ids_unicode_and_filters_deleted_rows() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE chatRoomList(chatId INTEGER, type TEXT, chatRoomTitle TEXT, lastUpdatedAt INTEGER);
        INSERT INTO chatRoomList VALUES(11, 'MultiChat', '합성 방 😀', 1767225600);
        CREATE TABLE chatLogs(logId INTEGER, authorId INTEGER, message TEXT, type INTEGER, sendAt INTEGER, deleted INTEGER);
        INSERT INTO chatLogs VALUES(21, 31, '첫 줄\n둘째 줄 😀', 1, 1767225600, 0);
        INSERT INTO chatLogs VALUES(22, 31, '삭제된 합성 내용', 1, 1767225601, 1);
        INSERT INTO chatLogs VALUES(23, 31, NULL, 2, 1767225602, NULL);").unwrap();
    let rooms = read_rooms(&db).unwrap();
    assert_eq!(rooms[0].chat_id, "11");
    assert_eq!(rooms[0].chat_name, "합성 방 😀");
    let names = HashMap::from([("31".into(), "합성 사용자".into())]);
    let messages = read_messages(&db, &rooms[0], "synthetic-account", &names).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message_id, "21");
    assert_eq!(messages[0].text, "첫 줄\n둘째 줄 😀");
    assert_eq!(messages[0].sender_nickname, "합성 사용자");
    assert_eq!(messages[1].message_type, "image");
}

#[test]
fn windows_raw_key_read_is_read_only_and_rejects_a_wrong_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.edb");
    let key = [0x51; 32];
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!("PRAGMA key = \"x'{}'\"; PRAGMA cipher_page_size=4096; CREATE TABLE synthetic(value INTEGER); INSERT INTO synthetic VALUES(7);", "51".repeat(32))).unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let db = open_database(&path, &key).unwrap();
    assert_eq!(
        db.query_row("SELECT value FROM synthetic", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert!(db.execute("DELETE FROM synthetic", []).is_err());
    assert!(open_database(&path, &[0x52; 32]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn unsupported_windows_schema_is_an_error_not_an_empty_success() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE unexpected(value TEXT);")
        .unwrap();
    assert!(read_rooms(&db).is_err());
}
