use super::*;
use aes::{
    cipher::{BlockDecrypt, KeyInit},
    Aes256,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    mem::size_of,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{
        Diagnostics::{Debug::ReadProcessMemory, ToolHelp::*},
        Memory::*,
        Threading::*,
    },
};
use zeroize::{Zeroize, Zeroizing};

pub(super) struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(super) fn process() -> Result<(u32, Handle)> {
    unsafe {
        let raw = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if raw == INVALID_HANDLE_VALUE {
            return Err(source_error("Cannot enumerate Windows processes"));
        }
        let snapshot = Handle(raw);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot.0, &mut entry);
        let mut found = Vec::new();
        while more != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len])
                .eq_ignore_ascii_case("KakaoTalk.exe")
            {
                let raw = OpenProcess(
                    PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                    0,
                    entry.th32ProcessID,
                );
                if !raw.is_null() {
                    let handle = Handle(raw);
                    if same_user(handle.0) {
                        found.push((entry.th32ProcessID, handle));
                    }
                }
            }
            more = Process32NextW(snapshot.0, &mut entry);
        }
        if found.len() != 1 {
            return Err(source_error("Exactly one accessible KakaoTalk process for the current Windows user is required. Start KakaoTalk at the same privilege level and sign in."));
        }
        Ok(found.remove(0))
    }
}

unsafe fn same_user(process: HANDLE) -> bool {
    unsafe fn sid(token: HANDLE) -> Option<Vec<usize>> {
        let mut bytes = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return None;
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        ) == 0
        {
            None
        } else {
            Some(buffer)
        }
    }
    let mut own = std::ptr::null_mut();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own) == 0 {
        return false;
    }
    let own = Handle(own);
    let mut other = std::ptr::null_mut();
    if OpenProcessToken(process, TOKEN_QUERY, &mut other) == 0 {
        return false;
    }
    let other = Handle(other);
    let (Some(a), Some(b)) = (sid(own.0), sid(other.0)) else {
        return false;
    };
    EqualSid(
        (*(a.as_ptr().cast::<TOKEN_USER>())).User.Sid,
        (*(b.as_ptr().cast::<TOKEN_USER>())).User.Sid,
    ) != 0
}

fn profile() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .ok_or(Error::HomeDirUnavailable)?
        .join("Kakao")
        .join("KakaoTalk");
    let profiles = discover_profiles(&base)?;
    if let Some(selected) = std::env::var_os("KATOK_WINDOWS_PROFILE") {
        let selected = PathBuf::from(selected)
            .canonicalize()
            .map_err(|_| source_error("KATOK_WINDOWS_PROFILE is unavailable"))?;
        for candidate in profiles {
            if candidate.canonicalize()? == selected {
                return Ok(selected);
            }
        }
        return Err(source_error("KATOK_WINDOWS_PROFILE must name a KakaoTalk profile belonging to the current Windows user"));
    }
    if profiles.len() != 1 {
        return Err(source_error("Expected one Windows KakaoTalk profile. Sign in, or set KATOK_WINDOWS_PROFILE explicitly when multiple profiles exist."));
    }
    Ok(profiles[0].clone())
}

#[derive(Default)]
struct KeyCache {
    pid: u32,
    profile: PathBuf,
    keys: BTreeMap<PathBuf, Zeroizing<[u8; 32]>>,
    last_scan: Option<Instant>,
}
static KEYS: OnceLock<Mutex<KeyCache>> = OnceLock::new();

fn edb_files(dir: &Path, depth: usize, result: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 3 {
        return Ok(());
    }
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_dir() {
            edb_files(&item.path(), depth + 1, result)?;
        } else if kind.is_file()
            && item
                .path()
                .extension()
                .is_some_and(|s| s.eq_ignore_ascii_case("edb"))
        {
            result.push(item.path());
        }
    }
    Ok(())
}

struct Probe {
    path: PathBuf,
    first: [u8; 4096],
}
impl Probe {
    fn matches(&self, key: &[u8; 32]) -> bool {
        let cipher = Aes256::new(key.into());
        let mut block = aes::cipher::Block::<Aes256>::clone_from_slice(&self.first[16..32]);
        cipher.decrypt_block(&mut block);
        [80, 48, 64, 16, 32, 96].into_iter().any(|reserved| {
            let iv = &self.first[4096 - reserved..];
            let p = |i: usize| block[i] ^ iv[i];
            p(0) == 0x10
                && p(1) == 0
                && (1..=2).contains(&p(2))
                && (1..=2).contains(&p(3))
                && p(4) == reserved as u8
                && p(5) == 0x40
                && p(6) == 0x20
                && p(7) == 0x20
        })
    }
}

/// Bounded read-only scan; no debug privilege, injection, process dump, or key persistence.
fn scan(handle: HANDLE, files: &[PathBuf], cache: &mut KeyCache) -> Result<()> {
    use std::io::Read;
    let mut probes = Vec::new();
    for path in files {
        if cache.keys.contains_key(path) {
            continue;
        }
        let mut first = [0; 4096];
        if let Ok(mut file) = std::fs::File::open(path) {
            if file.read_exact(&mut first).is_ok() {
                probes.push(Probe {
                    path: path.clone(),
                    first,
                });
            }
        }
    }
    if probes.is_empty() {
        return Ok(());
    }
    let start = Instant::now();
    let mut address = 0usize;
    let mut seen = std::collections::HashSet::new();
    while start.elapsed() < Duration::from_secs(20) && !probes.is_empty() {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            VirtualQueryEx(
                handle,
                address as *const c_void,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        } == 0
        {
            break;
        }
        let base = info.BaseAddress as usize;
        let length = info.RegionSize;
        if info.State == MEM_COMMIT
            && info.Type == MEM_PRIVATE
            && info.Protect & PAGE_GUARD == 0
            && [
                PAGE_READONLY,
                PAGE_READWRITE,
                PAGE_WRITECOPY,
                PAGE_EXECUTE_READ,
                PAGE_EXECUTE_READWRITE,
                PAGE_EXECUTE_WRITECOPY,
            ]
            .contains(&(info.Protect & 0xff))
        {
            for offset in (0..length).step_by(1024 * 1024) {
                if start.elapsed() >= Duration::from_secs(20) || probes.is_empty() {
                    break;
                }
                let offset = offset.saturating_sub(89);
                let size = (1024 * 1024 + 89).min(length - offset);
                let mut buffer = Zeroizing::new(vec![0u8; size]);
                let mut got = 0;
                unsafe {
                    ReadProcessMemory(
                        handle,
                        (base + offset) as *const c_void,
                        buffer.as_mut_ptr().cast(),
                        size,
                        &mut got,
                    );
                }
                if got < 89 {
                    continue;
                }
                for i in 1..=got - 88 {
                    if buffer[i] != 0x88 || buffer[i - 1] >= 8 {
                        continue;
                    }
                    for k in [1, 16, 49, 56] {
                        let candidate = Zeroizing::new(
                            <[u8; 32]>::try_from(&buffer[i + k..i + k + 32])
                                .expect("bounded candidate"),
                        );
                        if candidate.iter().filter(|v| **v == 0).count() > 2 {
                            continue;
                        }
                        // Retain only a fingerprint for deduplication, never all candidate keys.
                        let fingerprint: [u8; 32] = Sha256::digest(candidate.as_slice()).into();
                        if !seen.insert(fingerprint) {
                            continue;
                        }
                        probes.retain(|probe| {
                            if probe.matches(&candidate)
                                && open_database(&probe.path, &candidate).is_ok()
                            {
                                cache
                                    .keys
                                    .insert(probe.path.clone(), Zeroizing::new(*candidate));
                                false
                            } else {
                                true
                            }
                        });
                    }
                }
                buffer.zeroize();
            }
        }
        let Some(next) = base.checked_add(length).filter(|next| *next > address) else {
            break;
        };
        address = next;
    }
    cache.last_scan = Some(Instant::now());
    Ok(())
}

pub struct ReadOutput {
    pub chats: Vec<ChatSummary>,
    pub messages: Vec<RawMessage>,
    pub available: usize,
    pub locked: usize,
}

pub fn read() -> Result<ReadOutput> {
    let profile = profile()?;
    let (pid, handle) = process()?;
    let mut files = Vec::new();
    edb_files(&profile, 0, &mut files)?;
    files.sort();
    let mut cache = KEYS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| source_error("Windows source cache was interrupted; restart lazykatok"))?;
    if cache.pid != pid || cache.profile != profile {
        *cache = KeyCache {
            pid,
            profile: profile.clone(),
            ..Default::default()
        };
    }
    cache
        .keys
        .retain(|p, key| files.contains(p) && open_database(p, key).is_ok());
    if cache
        .last_scan
        .is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
    {
        scan(handle.0, &files, &mut cache)?;
    }
    let list = files
        .iter()
        .find(|p| {
            p.file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("chatListInfo.edb"))
        })
        .ok_or_else(|| {
            source_error("Windows chat list is missing; open the KakaoTalk chat list")
        })?;
    let key = cache.keys.get(list).ok_or_else(|| {
        source_error("Windows chat list is locked; open the KakaoTalk chat list, then retry")
    })?;
    let chats = read_rooms(&open_database(list, key)?)?;
    let mut names = HashMap::new();
    for path in &files {
        if path
            .file_name()
            .is_some_and(|s| s.eq_ignore_ascii_case("TalkUserDB.edb"))
        {
            if let Some(key) = cache.keys.get(path) {
                names = read_names(&open_database(path, key)?)?;
            }
        }
    }
    let account: String = Sha256::digest(
        profile
            .file_name()
            .ok_or_else(|| source_error("Invalid Windows profile"))?
            .to_string_lossy()
            .as_bytes(),
    )
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect();
    let mut messages = Vec::new();
    let mut available = 0;
    let mut locked = 0;
    for room in &chats {
        let name = format!("chatLogs_{}.edb", room.chat_id);
        let paths: Vec<_> = files
            .iter()
            .filter(|p| p.file_name().is_some_and(|s| s.eq_ignore_ascii_case(&name)))
            .collect();
        if paths.len() > 1 {
            return Err(source_error(
                "Ambiguous Windows room database; sync was not completed",
            ));
        }
        let Some(path) = paths.first() else {
            locked += 1;
            continue;
        };
        let Some(key) = cache.keys.get(*path) else {
            locked += 1;
            continue;
        };
        let conn = open_database(path, key)?;
        conn.execute_batch("BEGIN")?;
        messages.extend(read_messages(&conn, room, &account, &names)?);
        available += 1;
    }
    Ok(ReadOutput {
        chats,
        messages,
        available,
        locked,
    })
}

pub fn probe_status() -> serde_json::Value {
    let base = dirs::data_local_dir().map(|p| p.join("Kakao").join("KakaoTalk"));
    let profiles = base.as_deref().map(discover_profiles);
    serde_json::json!({"platform":"windows","architecture":std::env::consts::ARCH,"profile_count":profiles.and_then(|r|r.ok()).map(|v|v.len()),"source_available":true,"read_validation":"required","send_validation":"required"})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_only_the_synthetic_test_process_and_authenticates_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.edb");
        let key: [u8; 32] = std::array::from_fn(|i| (i + 1) as u8);
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(&format!("PRAGMA key=\"x'{hex}'\"; PRAGMA cipher_page_size=4096; CREATE TABLE synthetic(value INTEGER); INSERT INTO synthetic VALUES(42);")).unwrap();
        }
        // A synthetic codec record inside this test executable's own memory.
        // No real KakaoTalk process, installation or database is used.
        let mut record = Zeroizing::new(vec![0x31u8; 128]);
        record[0] = 1;
        record[1] = 0x88;
        record[2..34].copy_from_slice(&key);
        std::hint::black_box(&record);
        let mut cache = KeyCache::default();
        scan(
            unsafe { GetCurrentProcess() },
            std::slice::from_ref(&path),
            &mut cache,
        )
        .unwrap();
        std::hint::black_box(&record);
        assert_eq!(cache.keys.get(&path).map(|k| **k), Some(key));
    }

    #[test]
    fn process_identity_check_accepts_own_user() {
        assert!(unsafe { same_user(GetCurrentProcess()) });
    }
}
