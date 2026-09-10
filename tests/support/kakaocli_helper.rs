fn main() {
    if cfg!(failure) { eprintln!("Error: SQL error: prepare: file is not a database"); std::process::exit(1); }
    match std::env::args().nth(1).as_deref() {
        Some("chats") => println!("{}", r#"[{"chat_id":"chat-kakao-fixture","chat_name":"Synthetic Kakao","chat_type":"direct"}]"#),
        Some("messages") => println!("{}", r#"[{"account_hash":"acct-kakao-fixture","chat_id":"chat-kakao-fixture","chat_name":"Synthetic Kakao","chat_type":"direct","message_id":"kakao-1","sender_id":"sender-1","sender_nickname":"테스터","timestamp":"2026-01-01T00:00:00Z","text":"합성 카카오 메시지","message_type":"text","reply_to_message_id":null}]"#),
        _ => std::process::exit(2),
    }
}
