//! `thirdeye` — the terminal door into Third Eye (spec 2026-08-02 N4).
//!
//! A thin, synchronous bridge client (the S7 loopback WS + per-boot token):
//!
//! - `thirdeye` / `thirdeye <path>` — add the directory to the workspace
//!   roots and bring the overlay up, ready to work there;
//! - `thirdeye ask "…"` — submit a question (it appears in the overlay
//!   too) and stream the answer to stdout;
//! - `thirdeye tui` — a ratatui chat: type, Enter to send, Esc to leave.
//!
//! No flags parse a config; everything comes from `bridge.json` in Third
//! Eye's app-data dir. When the app is not running the failure names that
//! plainly instead of retrying forever.

use std::io::Write as _;

use tungstenite::Message;

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            Ok(())
        }
        Some("ask") => {
            let text = args[1..].join(" ");
            if text.trim().is_empty() {
                Err("usage: thirdeye ask \"your question\"".to_string())
            } else {
                cmd_ask(&text)
            }
        }
        Some("tui") => tui::run(),
        Some(path) if path.starts_with('-') => {
            print_help();
            Err(format!("unknown flag: {path}"))
        }
        maybe_path => cmd_here(maybe_path.unwrap_or(".")),
    };
    if let Err(e) = result {
        eprintln!("thirdeye: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "thirdeye — talk to the running Third Eye app\n\n\
         usage:\n  \
         thirdeye [path]      add the directory (default .) as a workspace and open the overlay\n  \
         thirdeye ask \"…\"     ask a question; the answer streams here and in the overlay\n  \
         thirdeye tui         terminal chat (Enter sends, Esc quits)\n"
    );
}

/// `thirdeye [path]`: workspace + overlay.
fn cmd_here(path: &str) -> Result<(), String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("cannot resolve {path}: {e}"))?
        .display()
        .to_string();
    let mut socket = connect()?;
    send(
        &mut socket,
        &serde_json::json!({"type": "add-workspace", "path": canonical}).to_string(),
    )?;
    let ack = wait_ack(&mut socket, "add-workspace")?;
    println!("workspace: {ack}");
    send(&mut socket, r#"{"type":"show-overlay"}"#)?;
    wait_ack(&mut socket, "show-overlay")?;
    println!("Third Eye is up — working in {canonical}");
    Ok(())
}

/// `thirdeye ask "…"`: stream the answer to stdout.
fn cmd_ask(text: &str) -> Result<(), String> {
    let mut socket = connect()?;
    send(
        &mut socket,
        &serde_json::json!({"type": "ask", "text": text}).to_string(),
    )?;
    wait_ack(&mut socket, "ask")?;
    loop {
        let frame = read_json(&mut socket)?;
        match frame.get("type").and_then(|t| t.as_str()) {
            Some("chat-token") => {
                if let Some(token) = frame.get("token").and_then(|t| t.as_str()) {
                    print!("{token}");
                    let _ = std::io::stdout().flush();
                }
            }
            Some("chat-done") => {
                println!();
                return Ok(());
            }
            Some("chat-error") => {
                return Err(format!(
                    "the run failed: {}",
                    frame.get("detail").and_then(|d| d.as_str()).unwrap_or("?")
                ));
            }
            // Coding-subset frames (tool activity) ride the same socket;
            // the one-shot ask ignores them.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge plumbing (sync)
// ---------------------------------------------------------------------------

fn discovery_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "no HOME in environment".to_string())?;
    let candidate = if cfg!(target_os = "macos") {
        format!("{home}/Library/Application Support/com.slastrina.thirdeye/bridge.json")
    } else {
        let base = std::env::var("XDG_DATA_HOME").unwrap_or(format!("{home}/.local/share"));
        format!("{base}/com.slastrina.thirdeye/bridge.json")
    };
    Ok(std::path::PathBuf::from(candidate))
}

fn connect() -> Result<Socket, String> {
    let path = discovery_path()?;
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        "Third Eye does not appear to be running (no bridge.json) — start the app first".to_string()
    })?;
    let discovery: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("bridge.json is malformed: {e}"))?;
    let port = discovery
        .get("port")
        .and_then(|p| p.as_u64())
        .ok_or("bridge.json has no port")?;
    let token = discovery
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or("bridge.json has no token")?;
    let (mut socket, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}")).map_err(|e| {
        format!("cannot reach Third Eye on 127.0.0.1:{port} ({e}) — is the app running?")
    })?;
    send(
        &mut socket,
        &serde_json::json!({"type": "auth", "token": token}).to_string(),
    )?;
    let hello = read_json(&mut socket)?;
    if hello.get("type").and_then(|t| t.as_str()) != Some("hello") {
        return Err("authentication failed (stale bridge.json? restart the app)".into());
    }
    Ok(socket)
}

fn send(socket: &mut Socket, message: &str) -> Result<(), String> {
    socket
        .send(Message::Text(message.into()))
        .map_err(|e| format!("send failed: {e}"))
}

fn read_json(socket: &mut Socket) -> Result<serde_json::Value, String> {
    loop {
        match socket.read().map_err(|e| format!("connection lost: {e}"))? {
            Message::Text(text) => {
                if let Ok(value) = serde_json::from_str(&text) {
                    return Ok(value);
                }
            }
            Message::Close(_) => return Err("Third Eye closed the connection".into()),
            _ => {}
        }
    }
}

/// Read frames until the ack for `cmd` arrives; ok → detail, err → Err.
fn wait_ack(socket: &mut Socket, cmd: &str) -> Result<String, String> {
    loop {
        let frame = read_json(socket)?;
        let kind = frame.get("type").and_then(|t| t.as_str());
        if frame.get("cmd").and_then(|c| c.as_str()) == Some(cmd) {
            let detail = frame
                .get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            return match kind {
                Some("ok") => Ok(detail),
                _ => Err(detail),
            };
        }
    }
}

// ---------------------------------------------------------------------------
// TUI (ratatui)
// ---------------------------------------------------------------------------

mod tui {
    use super::{send, Socket};
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    use tungstenite::Message;

    struct Entry {
        who: &'static str,
        text: String,
    }

    pub fn run() -> Result<(), String> {
        let mut socket = super::connect()?;
        // Non-blocking reads so one loop serves both the keyboard and the
        // socket (WouldBlock = no frame right now).
        if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(std::time::Duration::from_millis(30)))
                .map_err(|e| e.to_string())?;
        }
        let mut terminal = ratatui::init();
        let result = event_loop(&mut terminal, &mut socket);
        ratatui::restore();
        result
    }

    fn event_loop(
        terminal: &mut ratatui::DefaultTerminal,
        socket: &mut Socket,
    ) -> Result<(), String> {
        let mut entries: Vec<Entry> = vec![Entry {
            who: "·",
            text: "Connected to Third Eye. Type a question, Enter to send, Esc to quit.".into(),
        }];
        let mut input = String::new();
        let mut streaming = false;
        loop {
            terminal
                .draw(|frame| {
                    let [log_area, input_area] =
                        Layout::vertical([Constraint::Min(3), Constraint::Length(3)])
                            .areas(frame.area());
                    let lines: Vec<Line> = entries
                        .iter()
                        .flat_map(|entry| {
                            let style = match entry.who {
                                "you" => Style::default().fg(Color::Cyan),
                                "eye" => Style::default().fg(Color::Green),
                                _ => Style::default().fg(Color::DarkGray),
                            };
                            entry.text.split('\n').enumerate().map(move |(i, part)| {
                                let prefix = if i == 0 {
                                    format!("{:>3} ", entry.who)
                                } else {
                                    "    ".into()
                                };
                                Line::from(vec![
                                    Span::styled(prefix, style),
                                    Span::raw(part.to_string()),
                                ])
                            })
                        })
                        .collect();
                    let total = lines.len() as u16;
                    let visible = log_area.height.saturating_sub(2);
                    let scroll = total.saturating_sub(visible);
                    frame.render_widget(
                        Paragraph::new(lines)
                            .wrap(Wrap { trim: false })
                            .scroll((scroll, 0))
                            .block(Block::default().borders(Borders::ALL).title(" Third Eye ")),
                        log_area,
                    );
                    let title = if streaming { " answering… " } else { " ask " };
                    frame.render_widget(
                        Paragraph::new(input.as_str())
                            .block(Block::default().borders(Borders::ALL).title(title)),
                        input_area,
                    );
                })
                .map_err(|e| e.to_string())?;

            // Keyboard (50ms poll).
            if crossterm::event::poll(std::time::Duration::from_millis(50))
                .map_err(|e| e.to_string())?
            {
                if let Event::Key(key) = crossterm::event::read().map_err(|e| e.to_string())? {
                    match key.code {
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        KeyCode::Enter => {
                            let text = input.trim().to_string();
                            if !text.is_empty() && !streaming {
                                send(
                                    socket,
                                    &serde_json::json!({"type": "ask", "text": text}).to_string(),
                                )?;
                                entries.push(Entry { who: "you", text });
                                entries.push(Entry {
                                    who: "eye",
                                    text: String::new(),
                                });
                                streaming = true;
                                input.clear();
                            }
                        }
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Char(c) => input.push(c),
                        _ => {}
                    }
                }
            }

            // Socket: drain whatever frames are ready.
            loop {
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        match frame.get("type").and_then(|t| t.as_str()) {
                            Some("chat-token") => {
                                if let (Some(token), Some(last)) = (
                                    frame.get("token").and_then(|t| t.as_str()),
                                    entries.last_mut(),
                                ) {
                                    if last.who == "eye" {
                                        last.text.push_str(token);
                                    }
                                }
                            }
                            Some("chat-done") => {
                                // The done text is authoritative (replaces
                                // the coalesced stream, same as the overlay).
                                if let (Some(text), Some(last)) = (
                                    frame.get("text").and_then(|t| t.as_str()),
                                    entries.last_mut(),
                                ) {
                                    if last.who == "eye" {
                                        last.text = text.to_string();
                                    }
                                }
                                streaming = false;
                            }
                            Some("chat-error") => {
                                entries.push(Entry {
                                    who: "!",
                                    text: frame
                                        .get("detail")
                                        .and_then(|d| d.as_str())
                                        .unwrap_or("run failed")
                                        .to_string(),
                                });
                                streaming = false;
                            }
                            _ => {}
                        }
                    }
                    Ok(Message::Close(_)) => return Err("Third Eye closed the connection".into()),
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(e) => return Err(format!("connection lost: {e}")),
                }
            }
        }
    }
}
