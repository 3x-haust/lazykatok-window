# Windows port (experimental)

This branch targets Windows x64. A Windows 11 ARM virtual machine can run the
x64 executable through Windows emulation. Native ARM64 and 32-bit x86 binaries
are not currently shipped. Windows KakaoTalk compatibility still requires
interactive validation on the installed app version; passing synthetic tests
does not establish it.

## Build

Use Rust 1.91, Visual Studio C++ Build Tools (Desktop development with C++), and
Perl. From the x64 Native Tools Command Prompt:

```cmd
cargo build --locked --release
cd target\release
.\lazykatok.exe doctor --json
```

Run these commands from the repository checkout on the `windows-port` branch.
The examples below continue in `target\release`. Users of an ARM VM should use
the x64 CI artifact; a plain `cargo build` inside an ARM environment may select
ARM64 instead.

The Windows CI workflow builds, lints and runs synthetic tests on an x64 Windows
runner. It does not sign into KakaoTalk or send real messages.

## Read and search

Start KakaoTalk under your own Windows user, sign in, and open the chat list
and the rooms you want to read. Run both applications at the same privilege
level; automatic administrator elevation is not used.

```cmd
.\lazykatok.exe source chats --source windows --json
.\lazykatok.exe sync --source windows --json
.\lazykatok.exe watch --source windows --select
.\lazykatok.exe search keyword "example" --json
```

Windows databases have per-file encryption keys. Keys for closed rooms may not
be resident in the running KakaoTalk process. The sync response includes
`source_coverage` counts; unavailable rooms were not synchronized. Open the
rooms in KakaoTalk and retry. Deletion reconciliation refuses an incomplete
source. An unsupported schema or failed authenticated read is an error.

If several KakaoTalk profiles are present, set `KATOK_WINDOWS_PROFILE` to the
intended profile directory beneath your local KakaoTalk users directory. The
program refuses to choose the first or most recently modified account. List the
profiles and set one in the same Command Prompt, replacing `<profile>` with the
chosen directory name:

```cmd
dir "%LOCALAPPDATA%\Kakao\KakaoTalk\users"
set "KATOK_WINDOWS_PROFILE=%LOCALAPPDATA%\Kakao\KakaoTalk\users\<profile>"
```

`doctor` reports the platform, architecture and profile count without reading
messages. It is a setup diagnostic, not proof that decryption or sending works.
A successful `source chats --source windows --json` is the next read check.

## Text replies

Use a `chat_id` returned by `source chats` above. Open the intended room in KakaoTalk first. Its title must uniquely identify it
across the account. Duplicate titles, multiple matching windows, an existing
draft, or an unrecognized compose control cause a refusal.

```cmd
.\lazykatok.exe send --chat <chat-id> --dry-run --json
.\lazykatok.exe watch --source windows --select --reply --reply-no-open --accept-use-policy
```

`send --dry-run` checks the exact room window and compose control without inserting
text. `watch --select --reply` shows a numbered room list, then an interactive
text input: type a reply and press Enter to submit it; Ctrl-C exits.

Windows sends only through the already-open room's control; it does not press
global keys or take focus. One Enter submission is attempted. A new matching
message must appear in the local room database before success is reported.
An unconfirmed submission is never retried automatically. Inspect KakaoTalk
before manually retrying to avoid a duplicate.

Opening closed rooms automatically, sending images, and extracting Windows
media have not been implemented. Reply relationships and some special message
types also require additional format work. These are outstanding parity items,
not supported features.

## Local data

The default application directory is `%LOCALAPPDATA%\katok`. Windows source
databases are opened read-only. Candidate keys and validated keys remain in
process memory and are not logged or persisted. Raw process dumps are never
created. Conversations are not uploaded; semantic models may be downloaded.

The derived archive and search indexes are ordinary local SQLite files. The
presence of a SQLCipher dependency does not encrypt those derived files. Use
OS disk encryption and keep the data directory private. Never include real
databases, keys, account paths, logs or conversations in bug reports or commits.
