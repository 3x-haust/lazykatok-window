use crate::cli::{Commands, PermissionsCommand, SearchCommand, SourceCommand, WatchOutputFormat};
use crate::commands::source_adapter::adapter_for_source;
use crate::support::{dependency_status, print_payload};
use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use katok::{
    adapters::ChatSummary,
    archive::Archive,
    chunking::{
        rebuild_chunks_for_chats, rebuild_chunks_with_settings, ChunkSettings, CHUNKER_VERSION,
    },
    config::KatokConfig,
    search::{bm25_search_with_snippet, keyword_search_with_snippet},
    semantic::semantic_search_live_with_config,
    transcript::export_transcript,
    types::{RawMessage, SyncReport, SyncTimings},
    watch::WATCH_EVENT_SCHEMA_VERSION,
    watch::{
        chat_count, format_human_message_line, parse_reply_command, sanitize_terminal_text,
        ReplyCommand, ReplyUiState, WatchEvent,
    },
    watch::{WatchSnapshot, WatchState},
};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(all(target_os = "macos", feature = "private-send"))]
use std::ffi::OsString;
#[cfg(all(target_os = "macos", feature = "private-send"))]
use std::process::{Command as ProcessCommand, Stdio};

mod chunk_commands;
mod freshness;
mod index_commands;
mod media_commands;
mod permissions;
mod source_adapter;

enum ReplyTerminalAction {
    Submit(String),
    Quit,
}

struct ReplyTerminal {
    output: io::Stdout,
    state: ReplyUiState,
    active: bool,
}

impl ReplyTerminal {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().context("enable reply terminal raw mode")?;
        let mut output = io::stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, EnableBracketedPaste, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error).context("initialize reply terminal screen");
        }
        let mut terminal = Self {
            output,
            state: ReplyUiState::default(),
            active: true,
        };
        terminal.render()?;
        Ok(terminal)
    }

    fn push_line(&mut self, line: impl Into<String>) -> Result<()> {
        self.state.push_line(line);
        self.render()
    }

    fn poll_action(&mut self, timeout: Duration) -> Result<Option<ReplyTerminalAction>> {
        if !event::poll(timeout).context("poll reply terminal input")? {
            return Ok(None);
        }
        match event::read().context("read reply terminal input")? {
            Event::Resize(_, _) => self.render()?,
            Event::Paste(text) => {
                self.state.paste(&text);
                self.render()?;
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'd'))
                {
                    return Ok(Some(ReplyTerminalAction::Quit));
                }
                match key.code {
                    KeyCode::Enter => {
                        return Ok(Some(ReplyTerminalAction::Submit(self.state.take_draft())));
                    }
                    KeyCode::Backspace => {
                        self.state.backspace();
                        self.render()?;
                    }
                    KeyCode::Char(character)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.state.insert(character);
                        self.render()?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn render(&mut self) -> Result<()> {
        let (width, height) = terminal::size().context("read reply terminal size")?;
        let frame = self.state.frame(width, height);
        queue!(self.output, Hide, MoveTo(0, 0), Clear(ClearType::All))
            .context("clear reply terminal")?;
        for (row, line) in frame.conversation.iter().enumerate() {
            let Ok(row) = u16::try_from(row) else {
                break;
            };
            if row >= height.saturating_sub(2) {
                break;
            }
            queue!(self.output, MoveTo(0, row), Print(line)).context("draw reply history")?;
        }
        let separator_row = height.saturating_sub(2);
        let input_row = height.saturating_sub(1);
        queue!(
            self.output,
            MoveTo(0, separator_row),
            Print(&frame.separator),
            MoveTo(0, input_row),
            Print(&frame.input),
            MoveTo(frame.cursor_column.min(width.saturating_sub(1)), input_row),
            Show
        )
        .context("draw reply input")?;
        self.output.flush().context("flush reply terminal")?;
        Ok(())
    }
}

impl Drop for ReplyTerminal {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(
                self.output,
                Show,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            let _ = terminal::disable_raw_mode();
            self.active = false;
        }
    }
}

pub(crate) fn command_requests_json(command: &Commands) -> bool {
    match command {
        Commands::Doctor { json, .. }
        | Commands::Sync { json, .. }
        | Commands::Index { json, .. }
        | Commands::WipeIndex { json, .. }
        | Commands::Chunks { json, .. }
        | Commands::Transcript { json, .. } => *json,
        Commands::Watch {
            select,
            format,
            reply,
            ..
        } => effective_watch_format(*select || *reply, *format) == WatchOutputFormat::Jsonl,
        Commands::Search { command } => match command {
            SearchCommand::Keyword { json, .. }
            | SearchCommand::Bm25 { json, .. }
            | SearchCommand::Semantic { json, .. } => *json,
        },
        Commands::Chunk { command } => match command {
            crate::cli::ChunkCommand::Get { json, .. }
            | crate::cli::ChunkCommand::Context { json, .. }
            | crate::cli::ChunkCommand::Parent { json, .. } => *json,
        },
        Commands::Source { command } => match command {
            SourceCommand::Chats { json, .. } => *json,
        },
        Commands::Media { command } => match command {
            crate::cli::MediaCommand::Get { json, .. }
            | crate::cli::MediaCommand::Backfill { json, .. } => *json,
        },
        Commands::Permissions { command } => match command {
            PermissionsCommand::Macos { json, .. } => *json,
        },
        #[cfg(all(target_os = "macos", feature = "private-send"))]
        Commands::Send { json, .. } => *json,
    }
}

pub(crate) fn run(
    command: Commands,
    config: KatokConfig,
    data_dir: PathBuf,
    archive_path: PathBuf,
    semantic_dir: PathBuf,
) -> Result<()> {
    match command {
        Commands::Doctor { macos_probe, json } => run_doctor(
            macos_probe,
            json,
            config,
            data_dir,
            archive_path,
            semantic_dir,
        ),
        Commands::Sync {
            source,
            path,
            json,
            touched,
            prune_preview,
            prune_deleted,
        } => {
            let source = source.unwrap_or_else(|| config.source_adapter.clone());
            run_sync(
                &source,
                path,
                json,
                touched,
                prune_preview,
                prune_deleted,
                &config,
                &archive_path,
                &data_dir,
            )
        }
        Commands::Index {
            full,
            dry_run,
            json,
        } => index_commands::run(
            full,
            dry_run,
            json,
            &config,
            &archive_path,
            &semantic_dir,
            &data_dir,
        ),
        Commands::Search { command } => run_search(command, &config, &archive_path, &semantic_dir),
        Commands::Chunk { command } => chunk_commands::run(command, &archive_path),
        Commands::Source { command } => run_source(command, &config, &data_dir),
        Commands::Media { command } => media_commands::run(command, &data_dir),
        Commands::Permissions { command } => run_permissions(command),
        Commands::Chunks { chat, json } => run_chunks(&chat, json, &archive_path),
        Commands::Transcript {
            chat,
            since,
            out,
            json,
        } => run_transcript(&chat, since.as_deref(), out, json, &archive_path, &data_dir),
        Commands::Watch {
            source,
            path,
            chat,
            select,
            format,
            reply,
            reply_no_open,
            accept_use_policy,
            tail,
            poll_ms,
            once,
            max_polls,
            replay_existing,
        } => run_watch(
            source,
            path,
            chat,
            select,
            format,
            reply,
            reply_no_open,
            accept_use_policy,
            tail,
            poll_ms,
            once,
            max_polls,
            replay_existing,
            &config,
            &archive_path,
            &data_dir,
        ),
        Commands::WipeIndex { yes, json } => run_wipe_index(yes, json, &semantic_dir),
        #[cfg(all(target_os = "macos", feature = "private-send"))]
        Commands::Send {
            room,
            chat,
            text,
            image,
            list_windows,
            list_rooms,
            limit,
            dry_run,
            no_open,
            background_only,
            draft,
            take_focus_now,
            focus_wait,
            accept_use_policy,
            json,
        } => run_send(
            room,
            chat,
            text,
            image,
            list_windows,
            list_rooms,
            limit,
            dry_run,
            no_open,
            background_only,
            draft,
            take_focus_now,
            focus_wait,
            accept_use_policy,
            json,
            &archive_path,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_watch(
    source: Option<String>,
    path: Option<PathBuf>,
    chat: Option<String>,
    select: bool,
    format: Option<WatchOutputFormat>,
    reply: bool,
    reply_no_open: bool,
    accept_use_policy: bool,
    tail: u64,
    poll_ms: u64,
    once: bool,
    max_polls: Option<u64>,
    replay_existing: bool,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let source = source.unwrap_or_else(|| config.source_adapter.clone());
    let format = effective_watch_format(select || reply, format);
    if reply {
        if format != WatchOutputFormat::Text {
            anyhow::bail!("reply mode is only available with --format text");
        }
        if chat.is_none() && !select {
            anyhow::bail!("reply mode requires --chat or --select");
        }
        if !accept_use_policy {
            anyhow::bail!(
                "refusing to continue without --accept-use-policy; read \
                 ACCEPTABLE_USE_POLICY.md and DISCLAIMER.md"
            );
        }
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "reply mode requires an interactive terminal; piped or redirected input is not sent"
            );
        }
        if !io::stdout().is_terminal() {
            anyhow::bail!("reply mode requires an interactive output terminal");
        }
    }
    let poll_interval = Duration::from_millis(poll_ms);
    let max_polls = if once { Some(1) } else { max_polls };
    let mut snapshot = WatchSnapshot::default();
    let mut stdout = std::io::stdout();
    let mut chat = chat;
    let mut selected_chat_name = None;
    let mut archive_counts = None;

    if select {
        let selected = select_watch_chat(&source, path.clone(), data_dir)?;
        selected_chat_name = Some(selected.chat_name);
        chat = Some(selected.chat_id);
    }

    let mut reply_terminal = if reply {
        let mut terminal = ReplyTerminal::new()?;
        terminal.push_line("katok: starting terminal watch; press Ctrl-C to stop")?;
        terminal.push_line(
            "katok: reply mode enabled; type a message and press Enter to send, /help for commands, /quit to stop",
        )?;
        Some(terminal)
    } else {
        None
    };

    if format == WatchOutputFormat::Jsonl {
        print_jsonl_event(
            &mut stdout,
            &WatchEvent::State {
                schema_version: WATCH_EVENT_SCHEMA_VERSION,
                state: WatchState::Started,
                poll: 0,
                observed_messages: 0,
                observed_chats: 0,
                emitted_messages: 0,
                archived_messages: None,
                chunks: None,
            },
        )?;
    } else if !reply {
        eprintln!("katok: starting terminal watch; press Ctrl-C to stop");
    }

    let mut poll = 0u64;
    loop {
        poll += 1;
        let poll_started = Instant::now();
        let next_poll_at = poll_started + poll_interval;
        let read_started = poll_started;
        if format == WatchOutputFormat::Jsonl {
            print_jsonl_event(
                &mut stdout,
                &WatchEvent::State {
                    schema_version: WATCH_EVENT_SCHEMA_VERSION,
                    state: WatchState::Reading,
                    poll,
                    observed_messages: snapshot.seen_count(),
                    observed_chats: 0,
                    emitted_messages: 0,
                    archived_messages: None,
                    chunks: None,
                },
            )?;
        } else if poll == 1 && !reply {
            eprintln!("katok: reading source {source}...");
        }
        // Build a fresh adapter each pass. The macOS adapter memoizes one read
        // per instance, which is correct for `sync` but a watch loop must see
        // source changes made after the previous poll.
        let adapter = adapter_for_source(&source, path.clone(), data_dir)?;
        let messages = adapter.messages().context("read source messages")?;
        let read_source = read_started.elapsed().as_millis();
        if selected_chat_name.is_none() {
            selected_chat_name = chat
                .as_deref()
                .and_then(|chat_id| find_chat_name(&messages, chat_id));
        }
        let text_replay = format == WatchOutputFormat::Text && chat.is_some();
        let events = snapshot.diff(
            &messages,
            poll,
            replay_existing || text_replay,
            chat.as_deref(),
        );
        let events = if format == WatchOutputFormat::Text && poll == 1 {
            tail_events(events, tail as usize)
        } else {
            events
        };
        let emitted_messages = events.len();
        let (archive_changed, archived_messages, chunks) = if snapshot.source_changed() {
            let report = sync_watch_messages(
                &messages,
                read_source,
                &source,
                config,
                archive_path,
                data_dir,
            )
            .context("sync watched messages")?;
            let counts = (report.total_messages, report.chunks);
            archive_counts = Some(counts);
            (
                report.inserted_messages > 0 || report.updated_messages > 0,
                counts.0,
                counts.1,
            )
        } else {
            let (archived_messages, chunks) = archive_counts
                .context("watch archive counts missing after the initial source snapshot")?;
            // A successful unchanged poll is still a successful freshness check.
            // Preserve that timestamp while avoiding the full archive/chunk pass.
            freshness::record_sync(data_dir, &source, archived_messages, chunks)?;
            (false, archived_messages, chunks)
        };

        for event in events {
            match format {
                WatchOutputFormat::Jsonl => print_jsonl_event(&mut stdout, &event)?,
                WatchOutputFormat::Text => {
                    if let Some(terminal) = reply_terminal.as_mut() {
                        if let WatchEvent::Message {
                            change, message, ..
                        } = &event
                        {
                            terminal.push_line(format_human_message_line(*change, message))?;
                        }
                    } else {
                        print_text_event(&mut stdout, &event)?;
                    }
                }
            }
        }

        if format == WatchOutputFormat::Jsonl {
            print_jsonl_event(
                &mut stdout,
                &WatchEvent::State {
                    schema_version: WATCH_EVENT_SCHEMA_VERSION,
                    state: if emitted_messages > 0 || archive_changed {
                        WatchState::Synced
                    } else {
                        WatchState::Idle
                    },
                    poll,
                    observed_messages: messages.len(),
                    observed_chats: chat_count(&messages),
                    emitted_messages,
                    archived_messages: Some(archived_messages),
                    chunks: Some(chunks),
                },
            )?;
        } else if poll == 1 {
            let label = selected_chat_name
                .as_deref()
                .map(sanitize_terminal_text)
                .unwrap_or_else(|| "selected chat".to_string());
            let status = format!(
                "katok: watching {label}; displayed {emitted_messages} recent message(s); observed {} message(s) across {} chat(s)",
                messages.len(), chat_count(&messages)
            );
            if let Some(terminal) = reply_terminal.as_mut() {
                terminal.push_line(status)?;
                terminal.push_line(format!("katok: replies target {label}"))?;
            } else {
                eprintln!("{status}");
            }
        }

        if let Some(terminal) = reply_terminal.as_mut() {
            if process_pending_reply_events(terminal, chat.as_deref(), reply_no_open, data_dir)? {
                break;
            }
        }

        if max_polls.is_some_and(|limit| poll >= limit) {
            break;
        }
        let remaining = remaining_poll_delay(next_poll_at, Instant::now());
        if let Some(terminal) = reply_terminal.as_mut() {
            if wait_for_next_poll_with_reply(
                terminal,
                chat.as_deref(),
                reply_no_open,
                data_dir,
                remaining,
            )? {
                break;
            }
        } else {
            thread::sleep(remaining);
        }
    }

    Ok(())
}

fn effective_watch_format(
    human_default: bool,
    format: Option<WatchOutputFormat>,
) -> WatchOutputFormat {
    format.unwrap_or(if human_default {
        WatchOutputFormat::Text
    } else {
        WatchOutputFormat::Jsonl
    })
}

fn select_watch_chat(source: &str, path: Option<PathBuf>, data_dir: &Path) -> Result<ChatSummary> {
    eprintln!("katok: reading chat list from {source}...");
    let adapter = adapter_for_source(source, path, data_dir)?;
    let mut chats = adapter.chats().context("list source chats")?;
    if chats.is_empty() {
        anyhow::bail!("no chats found in source");
    }
    chats.sort_by(|left, right| {
        left.chat_name
            .cmp(&right.chat_name)
            .then_with(|| left.chat_id.cmp(&right.chat_id))
    });

    let mut stderr = io::stderr().lock();
    writeln!(stderr, "Choose a chat to watch:").context("write chat selection prompt")?;
    for (index, chat) in chats.iter().enumerate() {
        writeln!(
            stderr,
            "{:>3}. {} ({}, {})",
            index + 1,
            sanitize_terminal_text(&chat.chat_name),
            sanitize_terminal_text(&chat.chat_type),
            sanitize_terminal_text(&chat.chat_id)
        )
        .context("write chat selection option")?;
    }
    write!(stderr, "chat number or chat_id> ").context("write chat selection input prompt")?;
    stderr.flush().context("flush chat selection prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read selected chat")?;
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("no chat selected");
    }
    if let Ok(index) = input.parse::<usize>() {
        return chats
            .get(index.saturating_sub(1))
            .cloned()
            .with_context(|| format!("chat number {index} is out of range"));
    }
    chats
        .into_iter()
        .find(|chat| chat.chat_id == input)
        .with_context(|| format!("no chat_id {input} in source"))
}

fn find_chat_name(messages: &[RawMessage], chat_id: &str) -> Option<String> {
    messages
        .iter()
        .find(|message| message.chat_id == chat_id)
        .map(|message| message.chat_name.clone())
}

fn tail_events(mut events: Vec<WatchEvent>, tail: usize) -> Vec<WatchEvent> {
    if events.len() > tail {
        events.split_off(events.len() - tail)
    } else {
        events
    }
}

fn print_text_event(stdout: &mut impl Write, event: &WatchEvent) -> Result<()> {
    if let WatchEvent::Message {
        change, message, ..
    } = event
    {
        writeln!(stdout, "{}", format_human_message_line(*change, message))
            .context("write watch text line")?;
        stdout.flush().context("flush watch text line")?;
    }
    Ok(())
}

fn print_jsonl_event(stdout: &mut impl Write, event: &WatchEvent) -> Result<()> {
    serde_json::to_writer(&mut *stdout, event).context("serialize watch event")?;
    writeln!(stdout).context("write watch event")?;
    stdout.flush().context("flush watch event")?;
    Ok(())
}

fn process_pending_reply_events(
    terminal: &mut ReplyTerminal,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
) -> Result<bool> {
    while let Some(action) = terminal.poll_action(Duration::ZERO)? {
        if process_reply_action(terminal, action, chat_id, no_open, data_dir)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn process_reply_action(
    terminal: &mut ReplyTerminal,
    action: ReplyTerminalAction,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
) -> Result<bool> {
    let ReplyTerminalAction::Submit(line) = action else {
        return Ok(true);
    };
    match parse_reply_command(1, &line) {
        ReplyCommand::Send(body) => {
            let chat_id = chat_id.context("watch --reply has no selected chat")?;
            match send_reply_to_chat(chat_id, &body, no_open, data_dir) {
                Ok(chars) => terminal.push_line(format!("katok: sent reply ({chars} chars)"))?,
                Err(err) => terminal.push_line(format!("katok: send failed: {err:#}"))?,
            }
        }
        ReplyCommand::Quit => return Ok(true),
        ReplyCommand::Help => terminal.push_line(
            "katok: type a message and press Enter to send; commands: /send message, /help, /quit",
        )?,
        ReplyCommand::Empty => {}
        ReplyCommand::Ignored => terminal.push_line("katok: not sent; unknown slash command")?,
    }
    Ok(false)
}

fn remaining_poll_delay(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

fn wait_for_next_poll_with_reply(
    terminal: &mut ReplyTerminal,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
    poll_interval: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + poll_interval;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        if let Some(action) = terminal.poll_action(remaining.min(Duration::from_millis(250)))? {
            if process_reply_action(terminal, action, chat_id, no_open, data_dir)? {
                return Ok(true);
            }
        }
    }
}

#[cfg(all(target_os = "macos", feature = "private-send"))]
fn send_reply_to_chat(chat_id: &str, body: &str, no_open: bool, data_dir: &Path) -> Result<usize> {
    let mut command = ProcessCommand::new(std::env::current_exe().context("resolve katok binary")?);
    command.args(reply_send_args(chat_id, no_open, data_dir));
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start katok send")?;
    {
        let stdin = child.stdin.as_mut().context("open katok send stdin")?;
        stdin
            .write_all(body.as_bytes())
            .context("write reply body to katok send")?;
        stdin
            .write_all(b"\n")
            .context("finish reply body for katok send")?;
    }
    let output = child.wait_with_output().context("wait for katok send")?;
    if !output.status.success() {
        let detail = send_child_error_detail(output.status, &output.stdout, &output.stderr);
        anyhow::bail!("{detail}");
    }
    Ok(body.chars().count())
}

#[cfg(all(target_os = "macos", feature = "private-send"))]
fn reply_send_args(chat_id: &str, no_open: bool, data_dir: &Path) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_os_string(),
        OsString::from("send"),
        OsString::from("--chat"),
        OsString::from(chat_id),
        OsString::from("--accept-use-policy"),
        OsString::from("--json"),
    ];
    if no_open {
        args.push(OsString::from("--no-open"));
        args.push(OsString::from("--background-only"));
    }
    args
}

#[cfg(all(target_os = "macos", feature = "private-send"))]
fn send_child_error_detail(status: impl std::fmt::Display, stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }

    let stdout = String::from_utf8_lossy(stdout);
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) {
            if let Some(cause) = value
                .pointer("/error/cause")
                .and_then(|value| value.as_str())
            {
                return cause.to_string();
            }
            if let Some(message) = value
                .pointer("/error/message")
                .and_then(|value| value.as_str())
            {
                return message.to_string();
            }
        }
        return stdout.to_string();
    }

    format!("katok send failed with {status}")
}

#[cfg(not(all(target_os = "macos", feature = "private-send")))]
fn send_reply_to_chat(
    _chat_id: &str,
    _body: &str,
    _no_open: bool,
    _data_dir: &Path,
) -> Result<usize> {
    anyhow::bail!("watch --reply requires the macOS private-send feature")
}

fn sync_watch_messages(
    messages: &[RawMessage],
    read_source: u128,
    source: &str,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<SyncReport> {
    let archive = Archive::open(archive_path).context("open archive")?;
    let mut report = archive.in_transaction(|| {
        let upsert_started = Instant::now();
        let mut report = archive.sync_messages(messages).context("sync messages")?;
        let upsert_messages = upsert_started.elapsed().as_millis();

        let rebuild_started = Instant::now();
        let settings = ChunkSettings {
            group_gap_seconds: config.chunk_gap_group_seconds,
            direct_gap_seconds: config.chunk_gap_direct_seconds,
        };
        let stored_settings = archive
            .stored_chunk_settings()
            .context("read chunk settings")?;
        let settings_changed = stored_settings
            != Some((
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            ));
        report.chunks = if archive.chunk_count().context("count chunks")? == 0 || settings_changed {
            rebuild_chunks_with_settings(&archive, settings).context("rebuild chunks")?
        } else {
            rebuild_chunks_for_chats(&archive, settings, &report.touched_chats)
                .context("rebuild chunks")?
        };
        archive
            .record_chunk_settings(
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            )
            .context("record chunk settings")?;

        report.timings_ms = SyncTimings {
            read_source,
            upsert_messages,
            rebuild_chunks: rebuild_started.elapsed().as_millis(),
        };
        Ok::<_, anyhow::Error>(report)
    })?;
    report.include_touched = false;
    freshness::record_sync(data_dir, source, report.total_messages, report.chunks)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
#[cfg(all(target_os = "macos", feature = "private-send"))]
fn run_send(
    room: Option<String>,
    chat: Option<String>,
    text: Option<String>,
    image: Option<PathBuf>,
    list_windows: bool,
    list_rooms: bool,
    limit: usize,
    dry_run: bool,
    no_open: bool,
    background_only: bool,
    draft: bool,
    take_focus_now: bool,
    focus_wait: u64,
    accept_use_policy: bool,
    json: bool,
    archive_path: &Path,
) -> Result<()> {
    use katok::kakao::ax_send;
    use std::io::Read;

    if list_windows {
        let titles = ax_send::open_window_titles()?;
        return print_payload(json, &serde_json::json!({ "open_windows": titles }));
    }
    if list_rooms {
        let rooms = ax_send::chat_list_rooms(limit)?;
        return print_payload(json, &serde_json::json!({ "rooms": rooms }));
    }
    if !dry_run && !accept_use_policy {
        anyhow::bail!(
            "refusing to continue without --accept-use-policy; read \
             ACCEPTABLE_USE_POLICY.md and DISCLAIMER.md"
        );
    }

    let allow_open = !no_open;
    // Anything that has to bring KakaoTalk forward waits for a gap in the user's
    // own typing first, so a send never lands mid-keystroke.
    let policy = if take_focus_now {
        ax_send::FocusPolicy::immediate()
    } else {
        ax_send::FocusPolicy {
            max_wait: std::time::Duration::from_secs(focus_wait),
            ..ax_send::FocusPolicy::default()
        }
    };

    // Read stdin before the curtain goes up: a blocked terminal behind a
    // full-screen overlay waiting for input the user cannot give would be a
    // deadlock they can only escape by killing the process.
    let body = if dry_run || image.is_some() {
        None
    } else {
        let body = match text {
            Some(t) => t,
            None => {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("failed to read message body from stdin")?;
                buf
            }
        };
        let body = body.trim_end_matches('\n').to_string();
        if body.is_empty() {
            anyhow::bail!("refusing to send an empty message");
        }
        Some(body)
    };

    // The curtain owns the main thread — AppKit insists on it — so the send runs
    // on a worker. It only actually covers the screen for the steps that must
    // bring KakaoTalk forward; an ordinary background text send never raises it.
    // Resolve the target once. A `--chat` id also brings the last-message time,
    // which is the only thing on the chat list that separates two rooms sharing
    // a name — without it such a send is refused rather than sent to whichever
    // one happens to sit higher.
    let target = match (&room, &chat) {
        (_, Some(chat_id)) => {
            let archive = Archive::open(archive_path).context("open archive")?;
            let (name, rank) = archive
                .chat_identity(chat_id)
                .context("look up chat")?
                .with_context(|| format!("no chat {chat_id} in the archive; run sync first"))?;
            ax_send::RoomTarget {
                name,
                rank: Some(rank),
            }
        }
        (Some(name), None) => ax_send::RoomTarget::named(name),
        (None, None) => anyhow::bail!("pass --room or --chat"),
    };
    let room_display = target.name.clone();
    let target_owned = target.clone();
    let image_owned = image.clone();
    let body_owned = body.clone();
    // The curtain says what is actually happening. It used to read "전송중" for
    // every operation, including ones that send nothing at all — the same kind
    // of claim that made a draft mode report `sent: false` while delivering.
    let curtain_title = if dry_run {
        "카톡 채팅방 여는 중"
    } else if draft {
        "카톡 초안 넣는 중"
    } else if image.is_some() {
        "카톡 이미지 보내는 중"
    } else {
        "카톡 보내는 중"
    };
    if background_only {
        let body = body_owned.expect("text body prepared for background-only send");
        let ctx = ax_send::SendContext::background_only(policy);
        ax_send::send_to_open_window(&target_owned, &body, false, &ctx)?;
        return print_payload(
            json,
            &serde_json::json!({ "sent": true, "room": room_display }),
        );
    }

    let outcome = katok::kakao::send_curtain::run_with_curtain(curtain_title, move |curtain| {
        let ctx = ax_send::SendContext::with_curtain(policy, curtain);
        if dry_run {
            return ax_send::resolve_room_window(&target_owned, allow_open, &ctx);
        }
        if let Some(path) = &image_owned {
            return ax_send::send_image_to_open_window(&target_owned, path, allow_open, &ctx);
        }
        let body = body_owned.expect("text body prepared before the curtain");
        if draft {
            return ax_send::draft_to_open_window(&target_owned, &body, allow_open, &ctx);
        }
        ax_send::send_to_open_window(&target_owned, &body, allow_open, &ctx)
    });

    match outcome {
        Ok(result) => result?,
        // The worker panicked. Input and focus were already handed back by the
        // curtain's own scope guards, so report rather than resume it.
        Err(_) => anyhow::bail!("the send thread panicked; nothing was confirmed sent"),
    }

    if dry_run {
        return print_payload(
            json,
            &serde_json::json!({ "resolved": true, "room": room_display, "sent": false }),
        );
    }
    if let Some(path) = image {
        // Report the file name only; the path can leak directory structure into logs.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(unnamed)");
        return print_payload(
            json,
            &serde_json::json!({ "sent": true, "room": room_display, "image": name }),
        );
    }
    // Never echo the body: sent content is as sensitive as anything else this crate handles.
    let chars = body.map(|b| b.chars().count()).unwrap_or(0);
    print_payload(
        json,
        &serde_json::json!({
            "sent": !draft,
            "drafted": draft,
            "room": room_display,
            "chars": chars
        }),
    )
}

fn run_permissions(command: PermissionsCommand) -> Result<()> {
    match command {
        PermissionsCommand::Macos {
            accessibility,
            dry_run,
            json,
        } => permissions::open_macos(accessibility, dry_run, json),
    }
}

fn run_doctor(
    macos_probe_enabled: bool,
    json: bool,
    config: KatokConfig,
    data_dir: PathBuf,
    archive_path: PathBuf,
    semantic_dir: PathBuf,
) -> Result<()> {
    let macos_probe = macos_probe_payload(macos_probe_enabled, &data_dir);
    let payload = serde_json::json!({
        "name": "katok",
        "command": "katok",
        "data_dir": data_dir,
        "archive": archive_path,
        "semantic_index": semantic_dir,
        "freshness": freshness::load(&data_dir, &archive_path, &semantic_dir)?,
        "local_first": true,
        "macos": cfg!(target_os = "macos"),
        "source_adapter": {
            "configured": config.source_adapter,
            "fixture": "ok",
            "kakaocli": dependency_status("kakaocli"),
            "macos": macos_probe
        },
        "archive": {
            "status": if archive_path.exists() { "present" } else { "missing" }
        },
        "embedder": {
            "model": config.embedder_model,
            "dimension": config.vector_dimension,
            "provider": "local",
            "mode": std::env::var("KATOK_EMBEDDER").unwrap_or_else(|_| "local".to_string()),
            "endpoint": null
        }
    });
    print_payload(json, &payload)
}

#[cfg(all(test, target_os = "macos", feature = "private-send"))]
mod tests {
    use super::{remaining_poll_delay, reply_send_args, send_child_error_detail};
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn poll_delay_preserves_start_to_start_interval_and_saturates_on_overrun() {
        let poll_started = Instant::now();
        let deadline = poll_started + Duration::from_secs(2);
        assert_eq!(
            remaining_poll_delay(deadline, poll_started + Duration::from_millis(750)),
            Duration::from_millis(1_250)
        );
        assert_eq!(
            remaining_poll_delay(deadline, poll_started + Duration::from_millis(2_500)),
            Duration::ZERO
        );
    }

    #[test]
    fn send_child_error_detail_prefers_stderr() {
        let detail = send_child_error_detail(
            "exit status: 1",
            br#"{"ok":false,"error":{"message":"stdout message","cause":"stdout cause"}}"#,
            b"stderr cause\n",
        );

        assert_eq!(detail, "stderr cause");
    }

    #[test]
    fn send_child_error_detail_reads_json_stdout_cause() {
        let detail = send_child_error_detail(
            "exit status: 1",
            br#"{
              "ok": false,
              "error": {
                "message": "command failed",
                "cause": "no chat chat-group-1 in the archive; run sync first"
              }
            }"#,
            b"",
        );

        assert_eq!(
            detail,
            "no chat chat-group-1 in the archive; run sync first"
        );
    }

    #[test]
    fn send_child_error_detail_falls_back_to_status() {
        let detail = send_child_error_detail("exit status: 1", b"", b"");

        assert_eq!(detail, "katok send failed with exit status: 1");
    }

    #[test]
    fn reply_send_args_makes_no_open_replies_strictly_background_only() {
        let default_args = reply_send_args("chat-group-1", false, Path::new("/tmp/katok-data"));
        let no_open_args = reply_send_args("chat-group-1", true, Path::new("/tmp/katok-data"));

        assert!(!default_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--no-open")));
        assert!(!default_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--background-only")));
        assert!(no_open_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--no-open")));
        assert!(no_open_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--background-only")));
    }
}

fn macos_probe_payload(enabled: bool, data_dir: &Path) -> serde_json::Value {
    if !enabled {
        return serde_json::json!({
            "status": "not_checked",
            "reason": "run katok doctor --macos-probe --json to check KakaoTalk app data access"
        });
    }
    match dirs::home_dir() {
        Some(home) => {
            let status = katok::kakao::probe_status(&home, data_dir);
            serde_json::json!({
                "status": "checked",
                "app_installed": status.app_installed,
                "container_present": status.container_present,
                "db_file_count": status.db_file_count,
                "auth_cached": status.auth_cached
            })
        }
        None => serde_json::json!({ "status": "home_unavailable" }),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sync(
    source: &str,
    path: Option<PathBuf>,
    json: bool,
    include_touched: bool,
    prune_preview: bool,
    prune_deleted: bool,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let adapter = adapter_for_source(source, path, data_dir)?;
    let read_started = Instant::now();
    let messages = adapter.messages().context("read source messages")?;
    let read_source = read_started.elapsed().as_millis();
    let archive = Archive::open(archive_path).context("open archive")?;
    // Message upserts and the chunk rebuild are one unit: chunks derived from half-written
    // messages are not a usable archive state, so either both land or neither does.
    let report = archive.in_transaction(|| {
        let upsert_started = Instant::now();
        let mut report = archive.sync_messages(&messages).context("sync messages")?;
        let upsert_messages = upsert_started.elapsed().as_millis();

        // Before chunking, so a removed message never reaches a chunk. Inside
        // the same transaction, so a failure anywhere leaves nothing deleted.
        let pruned = if prune_preview || prune_deleted {
            let doomed = archive
                .reconcile_deletions(&messages, prune_deleted)
                .context("reconcile deletions")?;
            if prune_deleted {
                // Their chats have to be rechunked for the removal to reach the
                // chunk text and the index, not just the messages table.
                for chat_id in doomed
                    .iter()
                    .map(|d| d.chat_id.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    if let Some(touched) = report
                        .touched_chats
                        .iter_mut()
                        .find(|candidate| candidate.chat_id == chat_id)
                    {
                        // A later edit may already have put this chat in the
                        // report with a tail floor after the deleted row. A
                        // deletion has no surviving row that can safely anchor
                        // a cut, so widen that existing touch to the whole chat.
                        touched.earliest_changed_timestamp.clear();
                        touched.earliest_changed_message_id.clear();
                    } else {
                        report.touched_chats.push(katok::types::TouchedChat {
                            chat_id: chat_id.to_string(),
                            earliest_changed_timestamp: String::new(),
                            earliest_changed_message_id: String::new(),
                        });
                    }
                }
                report.rebuilt_chats = report.touched_chats.len();
                report.total_messages = archive.message_count().context("count messages")?;
            }
            doomed
        } else {
            Vec::new()
        };

        let rebuild_started = Instant::now();
        let settings = ChunkSettings {
            group_gap_seconds: config.chunk_gap_group_seconds,
            direct_gap_seconds: config.chunk_gap_direct_seconds,
        };
        // Recompute only the chats that changed. Three cases still need the full pass: a first
        // sync has no chunks to scope to, a gap-settings change invalidates every existing
        // chunk, and a chunker-version bump does the same — which includes the first run
        // against an archive written before the version was recorded. Without these checks a
        // settings or algorithm change would only ever reach rooms that happened to receive a
        // message, leaving the rest on the old boundaries forever.
        let stored_settings = archive
            .stored_chunk_settings()
            .context("read chunk settings")?;
        let settings_changed = stored_settings
            != Some((
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            ));
        report.chunks = if archive.chunk_count().context("count chunks")? == 0 || settings_changed {
            rebuild_chunks_with_settings(&archive, settings).context("rebuild chunks")?
        } else {
            rebuild_chunks_for_chats(&archive, settings, &report.touched_chats)
                .context("rebuild chunks")?
        };
        archive
            .record_chunk_settings(
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            )
            .context("record chunk settings")?;

        report.timings_ms = SyncTimings {
            read_source,
            upsert_messages,
            rebuild_chunks: rebuild_started.elapsed().as_millis(),
        };
        Ok::<_, anyhow::Error>((report, pruned))
    })?;
    let (mut report, pruned) = report;
    // Gate is output-only: the archive always computed touched_chats for the rebuild above.
    report.include_touched = include_touched;
    freshness::record_sync(data_dir, source, report.total_messages, report.chunks)?;

    if prune_preview || prune_deleted {
        let mut payload = serde_json::to_value(&report).context("serialize sync report")?;
        if let Some(map) = payload.as_object_mut() {
            map.insert(
                if prune_deleted {
                    "pruned_messages".to_string()
                } else {
                    "prunable_messages".to_string()
                },
                serde_json::to_value(&pruned).context("serialize pruned")?,
            );
        }
        return print_payload(json, &payload);
    }
    print_payload(json, &report)
}

fn run_transcript(
    chat: &str,
    since: Option<&str>,
    out: Option<PathBuf>,
    json: bool,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let archive = Archive::open(archive_path).context("open archive")?;
    // Transcripts hold raw message bodies, so they default under the katok data dir rather than
    // the working directory, where they could be committed by accident.
    let out_dir = out.unwrap_or_else(|| data_dir.join("transcripts"));
    let report = export_transcript(&archive, chat, since, &out_dir).context("export transcript")?;
    print_payload(json, &report)
}

fn run_search(
    command: SearchCommand,
    config: &KatokConfig,
    archive_path: &Path,
    semantic_dir: &Path,
) -> Result<()> {
    let archive = Archive::open(archive_path).context("open archive")?;
    match command {
        SearchCommand::Keyword { query, limit, json } => {
            let hits = keyword_search_with_snippet(&archive, &query, limit, config.snippet_length)
                .context("keyword search")?;
            print_payload(json, &hits)
        }
        SearchCommand::Bm25 { query, limit, json } => {
            let hits = bm25_search_with_snippet(&archive, &query, limit, config.snippet_length)
                .context("bm25 search")?;
            print_payload(json, &hits)
        }
        SearchCommand::Semantic { query, limit, json } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("create semantic runtime")?;
            let hits = runtime
                .block_on(semantic_search_live_with_config(
                    &archive,
                    semantic_dir,
                    &query,
                    limit,
                    config,
                ))
                .context("semantic search")?;
            print_payload(json, &hits)
        }
    }
}

fn run_source(command: SourceCommand, config: &KatokConfig, data_dir: &Path) -> Result<()> {
    match command {
        SourceCommand::Chats { source, path, json } => {
            let source = source.unwrap_or_else(|| config.source_adapter.clone());
            let adapter = adapter_for_source(&source, path, data_dir)?;
            let chats = adapter.chats().context("list source chats")?;
            print_payload(json, &chats)
        }
    }
}

fn run_chunks(chat: &str, json: bool, archive_path: &Path) -> Result<()> {
    let archive = Archive::open(archive_path).context("open archive")?;
    let chunks = archive.chunks_for_chat(chat).context("list chunks")?;
    print_payload(json, &chunks)
}

fn run_wipe_index(yes: bool, json: bool, semantic_dir: &Path) -> Result<()> {
    if !yes {
        anyhow::bail!("refusing to wipe semantic index without --yes");
    }
    if semantic_dir.exists() {
        std::fs::remove_dir_all(semantic_dir).context("remove semantic index")?;
    }
    print_payload(json, &serde_json::json!({"semantic_removed": true}))
}
