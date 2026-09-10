//! Targeted Win32 text delivery. Never injects global keyboard input.
use super::*;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::Threading::*,
    UI::{Input::KeyboardAndMouse::VK_RETURN, WindowsAndMessaging::*},
};

struct WindowList {
    pid: u32,
    windows: Vec<HWND>,
}
unsafe extern "system" fn collect_window(window: HWND, context: LPARAM) -> i32 {
    let list = &mut *(context as *mut WindowList);
    let mut pid = 0;
    GetWindowThreadProcessId(window, &mut pid);
    if pid == list.pid && IsWindowVisible(window) != 0 {
        list.windows.push(window);
    }
    1
}
unsafe extern "system" fn collect_edit(window: HWND, context: LPARAM) -> i32 {
    let output = &mut *(context as *mut Vec<HWND>);
    let mut class = [0u16; 128];
    let len = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
    let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
    if ["RichEdit50W", "RichEdit20W", "RICHEDIT50W"]
        .iter()
        .any(|name| class.eq_ignore_ascii_case(name))
        && IsWindowVisible(window) != 0
        && IsWindowEnabled(window) != 0
    {
        output.push(window);
    }
    1
}

fn message(window: HWND, id: u32, w: usize, l: isize) -> Result<usize> {
    let mut result = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            window,
            id,
            w,
            l,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            1500,
            &mut result,
        )
    };
    if ok == 0 {
        Err(source_error("KakaoTalk control did not respond. Nothing is confirmed sent; inspect the room before retrying."))
    } else {
        Ok(result)
    }
}
fn text(window: HWND) -> Result<String> {
    let len = message(window, WM_GETTEXTLENGTH, 0, 0)?;
    if len > 65536 {
        return Err(source_error(
            "KakaoTalk control text exceeds the supported size",
        ));
    }
    let mut buffer = vec![0u16; len + 1];
    let got = message(
        window,
        WM_GETTEXT,
        buffer.len(),
        buffer.as_mut_ptr() as isize,
    )?;
    if got >= buffer.len() {
        return Err(source_error("KakaoTalk control changed while being read"));
    }
    String::from_utf16(&buffer[..got])
        .map_err(|_| source_error("KakaoTalk control returned invalid Unicode"))
}
fn windows(pid: u32) -> Result<Vec<HWND>> {
    let mut context = WindowList {
        pid,
        windows: Vec::new(),
    };
    if unsafe { EnumWindows(Some(collect_window), &mut context as *mut _ as isize) } == 0 {
        return Err(source_error("Cannot enumerate KakaoTalk windows"));
    }
    Ok(context.windows)
}
pub fn open_window_titles() -> Result<Vec<String>> {
    let (pid, _process) = super::native::process()?;
    windows(pid)?.into_iter().map(text).collect()
}

struct SendLock(super::native::Handle);
impl Drop for SendLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0 .0);
        }
    }
}
fn acquire(pid: u32) -> Result<SendLock> {
    let name: Vec<u16> = format!("Local\\LazyKatokSend-{pid}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let raw = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if raw.is_null() {
        return Err(source_error("Cannot create Windows send lock"));
    }
    let handle = super::native::Handle(raw);
    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => Ok(SendLock(handle)),
        WAIT_ABANDONED => {
            unsafe {
                ReleaseMutex(handle.0);
            };
            Err(source_error(
                "A previous send was interrupted. Inspect the room and draft before retrying.",
            ))
        }
        _ => Err(source_error(
            "Another send is in progress; draft was not changed",
        )),
    }
}

/// Refuse duplicate titles across the source, even if only one duplicate window is open.
pub fn run(
    room: Option<&str>,
    chat: Option<&str>,
    body: Option<&str>,
    dry_run: bool,
    draft: bool,
) -> Result<serde_json::Value> {
    let (pid, _process) = super::native::process()?;
    let _lock = acquire(pid)?;
    let before = super::read()?;
    let candidates: Vec<_> = before
        .chats
        .iter()
        .filter(|r| match chat {
            Some(id) => r.chat_id == id,
            None => room == Some(r.chat_name.as_str()),
        })
        .collect();
    if candidates.len() != 1 {
        return Err(source_error(
            "Room identity is missing or ambiguous; sync and select one exact chat ID",
        ));
    }
    let target = candidates[0];
    if before
        .chats
        .iter()
        .filter(|r| r.chat_name == target.chat_name)
        .count()
        != 1
    {
        return Err(source_error("Multiple rooms share this title. Windows cannot verify their identity; rename the intended room uniquely in KakaoTalk before sending."));
    }
    let mut matches = Vec::new();
    for window in windows(pid)? {
        if text(window)? == target.chat_name {
            matches.push(window);
        }
    }
    if matches.len() != 1 {
        return Err(source_error("Open the intended room in KakaoTalk first. Exactly one window with its exact title is required; no window was opened or guessed."));
    }
    let window = matches[0];
    let mut edits = Vec::<HWND>::new();
    unsafe {
        EnumChildWindows(window, Some(collect_edit), &mut edits as *mut _ as isize);
    }
    if edits.len() != 1 {
        return Err(source_error(
            "Unsupported or ambiguous KakaoTalk compose control; nothing was sent",
        ));
    }
    let edit = edits[0];
    if dry_run {
        return Ok(serde_json::json!({"resolved":true,"sent":false,"chat_id":target.chat_id}));
    }
    let body = body.ok_or_else(|| source_error("A text body is required"))?;
    if body.trim().is_empty() || body.contains('\0') || body.encode_utf16().count() > 10000 {
        return Err(source_error(
            "Text must contain 1–10000 UTF-16 units and no NUL character",
        ));
    }
    if !text(edit)?.is_empty() {
        return Err(source_error(
            "The KakaoTalk compose box already contains a draft; it was not changed",
        ));
    }
    // We need a baseline from this room's DB before making any message-affecting change.
    let seen: HashSet<_> = before
        .messages
        .iter()
        .filter(|m| m.chat_id == target.chat_id)
        .map(|m| m.message_id.clone())
        .collect();
    if !draft && seen.is_empty() {
        return Err(source_error("No readable message baseline for this room. Open its history and retry before sending."));
    }
    let wide: Vec<u16> = body.encode_utf16().chain(Some(0)).collect();
    if message(edit, WM_SETTEXT, 0, wide.as_ptr() as isize)? == 0 {
        return Err(source_error(
            "KakaoTalk rejected the draft; nothing was submitted",
        ));
    }
    let equal = |s: &str| s.replace("\r\n", "\n") == body.replace("\r\n", "\n");
    if text(window)? != target.chat_name || !equal(&text(edit)?) {
        return Err(source_error(
            "Target or draft changed. Nothing was submitted; inspect the compose box.",
        ));
    }
    let mut current_pid = 0;
    unsafe {
        GetWindowThreadProcessId(edit, &mut current_pid);
    }
    if current_pid != pid {
        return Err(source_error(
            "KakaoTalk control identity changed; nothing was submitted",
        ));
    }
    if draft {
        return Ok(
            serde_json::json!({"sent":false,"drafted":true,"chat_id":target.chat_id,"chars":body.chars().count()}),
        );
    }
    // One submission only. Do not fall back to another key path after uncertainty.
    message(edit, WM_KEYDOWN, VK_RETURN as usize, 1)?;
    let _ = message(edit, WM_KEYUP, VK_RETURN as usize, 0xc0000001);
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(after) = super::read() {
            if after.messages.iter().any(|m| {
                m.chat_id == target.chat_id && !seen.contains(&m.message_id) && equal(&m.text)
            }) {
                return Ok(
                    serde_json::json!({"sent":true,"confirmed":"local_database","chat_id":target.chat_id,"chars":body.chars().count()}),
                );
            }
        }
    }
    Err(source_error("Submission occurred but delivery is unconfirmed. Inspect KakaoTalk before retrying; no automatic resend was attempted."))
}
