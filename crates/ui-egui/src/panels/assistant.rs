//! AI Assistant panel. A small native client talks to the optional
//! `vectorcraft-ai-assistant` loopback sidecar; every document operation still
//! comes back through [`VectorcraftApp::run`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};

use egui::{RichText, ScrollArea, TextEdit, Ui};
use serde_json::{Value, json};

use crate::VectorcraftApp;

pub const ID: &str = "aiAssistant";
const DEFAULT_PORT: u16 = 3894;
const PROVIDERS: &[(&str, &str)] = &[
    ("deepseek", "DeepSeek"),
    ("kimi", "Kimi"),
    ("qwen", "Qwen"),
    ("minimax", "MiniMax"),
    ("glm", "GLM"),
    ("openrouter", "OpenRouter"),
    ("ollama", "Ollama"),
    ("custom", "Custom"),
    ("claude-api", "Claude API"),
    ("codex", "Codex"),
    ("claude-code", "Claude Code"),
    ("opencode", "OpenCode"),
    ("qwen-code", "Qwen Code"),
    ("kimi-cli", "Kimi CLI"),
    ("gemini-cli", "Gemini CLI"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    User,
    Assistant,
    Tool,
    Status,
}

struct Message {
    who: Who,
    text: String,
}

enum WorkerEvent {
    Event(Value),
    Error(String),
    Finished,
}

/// Answers to `/resume`: the saved conversations, or the one picked.
enum ResumeEvent {
    List(Vec<Value>),
    OutOfRange,
    Open(Value),
    Error(String),
}

pub struct Assistant {
    input: String,
    messages: Vec<Message>,
    provider: String,
    port: u16,
    chat_id: String,
    busy: bool,
    queue: Vec<String>,
    receiver: Option<Receiver<WorkerEvent>>,
    resume_receiver: Option<Receiver<ResumeEvent>>,
}

impl Default for Assistant {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            input: String::new(),
            messages: Vec::new(),
            provider: "claude-code".into(),
            port: DEFAULT_PORT,
            chat_id: new_chat_id(NEXT.fetch_add(1, Ordering::Relaxed)),
            busy: false,
            queue: Vec::new(),
            receiver: None,
            resume_receiver: None,
        }
    }
}

/// Take the assistant out of the app while it draws: tool events need mutable
/// access to the rest of the app, including its command dispatcher.
pub fn show(app: &mut VectorcraftApp, ui: &mut Ui) {
    let mut assistant = std::mem::take(&mut app.assistant);
    assistant.poll(app, ui.ctx());
    assistant.draw(app, ui);
    app.assistant = assistant;
}

impl Assistant {
    fn draw(&mut self, app: &mut VectorcraftApp, ui: &mut Ui) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("assistant-provider")
                .selected_text(PROVIDERS.iter().find(|(id, _)| *id == self.provider).map(|(_, label)| *label).unwrap_or("Provider"))
                .show_ui(ui, |ui| {
                    for &(id, label) in PROVIDERS {
                        ui.selectable_value(&mut self.provider, id.to_string(), label);
                    }
                });
            if ui.small_button(tl!("New")).on_hover_text(tl!("New chat")).clicked() {
                self.reset(ui.ctx());
            }
            if ui.small_button(tl!("Settings…")).clicked() {
                app.open_url(&format!("http://127.0.0.1:{}/vectorcraft/settings.html", self.port));
            }
        });
        ui.separator();

        // Keep the composer visible: only the transcript scrolls. The panel used to rely on its
        // parent icon-panel scroller, which moved the input below the viewport as chat grew.
        let queued_height = if self.queue.is_empty() { 0.0 } else { ui.spacing().interact_size.y + ui.spacing().item_spacing.y };
        let composer_height = 76.0 + ui.spacing().interact_size.y + queued_height + ui.spacing().item_spacing.y * 3.0 + 8.0;
        let transcript_height = (ui.available_height() - composer_height).max(48.0);
        ScrollArea::vertical()
            .id_salt("assistant-transcript")
            .max_height(transcript_height)
            .min_scrolled_height(transcript_height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.label(RichText::new(tl!("Ask for an edit, a new illustration, or an explanation of the selected art.")).italics());
                    ui.add_space(6.0);
                    ui.label(tl!("The optional vectorcraft-ai-assistant server must be running on localhost."));
                }
                for message in &self.messages {
                    let (prefix, strong) = match message.who {
                        Who::User => (tl!("You"), true),
                        Who::Assistant => (tl!("Assistant"), true),
                        Who::Tool => (tl!("Tool"), false),
                        Who::Status => (tl!("Status"), false),
                    };
                    ui.label(if strong { RichText::new(prefix).strong() } else { RichText::new(prefix).small() });
                    ui.label(&message.text);
                    ui.add_space(8.0);
                }
            });

        ui.separator();
        if !self.queue.is_empty() {
            ui.label(RichText::new(format!("{}: {}", tl!("Queued"), self.queue.len())).small());
        }
        let edit = ui.add_sized(
            [ui.available_width(), 76.0],
            TextEdit::multiline(&mut self.input).hint_text(tl!("Describe what you want to create or change…")),
        );
        let send_key = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        ui.horizontal(|ui| {
            ui.label(tl!("Port:"));
            ui.add(egui::DragValue::new(&mut self.port).range(1024..=65535));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.busy {
                    if ui.button(tl!("Stop")).clicked() {
                        self.stop(ui.ctx());
                    }
                    if !self.input.trim().is_empty() && (ui.button(tl!("Queue")).clicked() || send_key) {
                        self.enqueue();
                    }
                } else if ui.add_enabled(!self.input.trim().is_empty(), egui::Button::new(tl!("Send"))).clicked() || send_key {
                    self.send(app, ui.ctx());
                }
            });
        });
    }

    fn enqueue(&mut self) {
        let text = self.input.trim().to_string();
        if !text.is_empty() {
            self.input.clear();
            self.queue.push(text);
        }
    }

    fn send(&mut self, app: &mut VectorcraftApp, ctx: &egui::Context) {
        let text = self.input.trim().to_string();
        if let Some(pick) = resume_command(&text) {
            self.input.clear();
            self.resume(pick, ctx);
            return;
        }
        if text.is_empty() || self.busy {
            return;
        }
        self.input.clear();
        self.messages.push(Message { who: Who::User, text: text.clone() });
        self.messages.push(Message { who: Who::Assistant, text: String::new() });

        let snapshot = app.run("document.inspect", json!({"depth": 0, "childLimit": 20})).unwrap_or_else(|e| json!({"error": e}));
        let prompt = format!("<document_context>\n{}\n</document_context>\n\n{text}", serde_json::to_string_pretty(&snapshot).unwrap_or_default());
        let body = json!({
            "chatId": self.chat_id,
            "app": "vectorcraft",
            "provider": self.provider,
            "text": prompt,
        });
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.busy = true;
        start_chat(self.port, body, tx, ctx.clone());
    }

    /// `/resume` lists the saved conversations; `/resume N` continues the Nth.
    fn resume(&mut self, pick: Option<usize>, ctx: &egui::Context) {
        if self.busy {
            self.messages.push(Message { who: Who::Status, text: tl!("Wait for the current answer to finish.").to_string() });
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.resume_receiver = Some(rx);
        start_resume(self.port, self.chat_id.clone(), pick, tx, ctx.clone());
    }

    fn poll_resume(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.resume_receiver else { return };
        let Ok(event) = rx.try_recv() else { return };
        self.resume_receiver = None;
        match event {
            ResumeEvent::List(rows) if rows.is_empty() => {
                self.messages.push(Message { who: Who::Status, text: tl!("No saved conversations yet.").to_string() });
            }
            ResumeEvent::List(rows) => {
                let mut text = tl!("Earlier conversations. Type /resume and a number to continue one:").to_string();
                for (i, row) in rows.iter().enumerate() {
                    let title = row.get("title").and_then(Value::as_str).unwrap_or_default();
                    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(70).collect();
                    let doc = row.get("doc").and_then(|d| d.get("name")).and_then(Value::as_str).unwrap_or_default();
                    let provider = row.get("provider").and_then(Value::as_str).unwrap_or_default();
                    let provider = PROVIDERS.iter().find(|(id, _)| *id == provider).map(|(_, label)| *label).unwrap_or(provider);
                    let meta = [doc, provider].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
                    text.push_str(&format!("\n{}. {title}  ({meta})", i + 1));
                }
                self.messages.push(Message { who: Who::Status, text });
            }
            ResumeEvent::OutOfRange => {
                self.messages.push(Message { who: Who::Status, text: tl!("Pick a number from the list.").to_string() });
            }
            ResumeEvent::Error(error) => self.messages.push(Message { who: Who::Status, text: format!("⚠ {error}") }),
            ResumeEvent::Open(saved) => {
                let Some(id) = saved.get("id").and_then(Value::as_str) else { return };
                post_json(self.port, "/api/reset", json!({"chatId": self.chat_id}), ctx.clone());
                self.chat_id = id.to_string();
                if let Some(provider) = saved.get("provider").and_then(Value::as_str)
                    && PROVIDERS.iter().any(|(p, _)| *p == provider)
                {
                    self.provider = provider.to_string();
                }
                self.queue.clear();
                self.messages.clear();
                for entry in saved.get("log").and_then(Value::as_array).into_iter().flatten() {
                    let text = entry.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                    let message = match entry.get("role").and_then(Value::as_str).unwrap_or_default() {
                        "user" => Message { who: Who::User, text },
                        "assistant" => Message { who: Who::Assistant, text },
                        "tool" => {
                            let ok = entry.get("ok").and_then(Value::as_bool) != Some(false);
                            let name = entry.get("name").and_then(Value::as_str).unwrap_or_default();
                            Message { who: Who::Tool, text: format!("{} {name}", if ok { "✓" } else { "✗" }) }
                        }
                        "error" => Message { who: Who::Status, text: format!("⚠ {text}") },
                        "notice" => Message { who: Who::Status, text: tl!("Stopped").to_string() },
                        _ => continue,
                    };
                    self.messages.push(message);
                }
                self.messages.push(Message { who: Who::Status, text: tl!("Conversation resumed.").to_string() });
            }
        }
    }

    fn poll(&mut self, app: &mut VectorcraftApp, ctx: &egui::Context) {
        self.poll_resume(ctx);
        let mut events = Vec::new();
        if let Some(rx) = &self.receiver {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                WorkerEvent::Event(value) => self.handle_event(app, value, ctx),
                WorkerEvent::Error(error) => self.messages.push(Message { who: Who::Status, text: format!("⚠ {error}") }),
                WorkerEvent::Finished => {
                    self.busy = false;
                    self.receiver = None;
                    if !self.queue.is_empty() {
                        self.input = self.queue.remove(0);
                        self.send(app, ctx);
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, app: &mut VectorcraftApp, event: Value, ctx: &egui::Context) {
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "text" => {
                let text = event.get("text").and_then(Value::as_str).unwrap_or_default();
                if let Some(message) = self.messages.iter_mut().rev().find(|m| m.who == Who::Assistant) {
                    message.text.push_str(text);
                }
            }
            "text_break" => {
                if let Some(message) = self.messages.iter_mut().rev().find(|m| m.who == Who::Assistant)
                    && !message.text.is_empty()
                    && !message.text.ends_with("\n\n")
                {
                    message.text.push_str("\n\n");
                }
            }
            "status" => self.messages.push(Message { who: Who::Status, text: event["text"].as_str().unwrap_or_default().to_string() }),
            "error" => {
                self.messages.push(Message { who: Who::Status, text: format!("⚠ {}", event["message"].as_str().unwrap_or("Assistant error")) })
            }
            "stopped" => self.messages.push(Message { who: Who::Status, text: tl!("Stopped").to_string() }),
            "exec" => self.run_tool(app, &event, ctx),
            _ => {}
        }
    }

    fn run_tool(&mut self, app: &mut VectorcraftApp, event: &Value, ctx: &egui::Context) {
        let name = event.get("name").and_then(Value::as_str).unwrap_or_default();
        let input = event.get("input").cloned().unwrap_or_else(|| json!({}));
        let call_id = event.get("callId").and_then(Value::as_str).unwrap_or_default().to_string();
        let result = execute_tool(app, name, input);
        let (ok, value) = match result {
            Ok(value) => (true, value),
            Err(error) => (false, json!({"error": error})),
        };
        self.messages.push(Message { who: Who::Tool, text: format!("{} {}", if ok { "✓" } else { "✗" }, name) });
        let body = json!({"chatId": self.chat_id, "callId": call_id, "ok": ok, "result": value});
        post_json(self.port, "/api/tool-result", body, ctx.clone());
    }

    fn stop(&mut self, ctx: &egui::Context) {
        post_json(self.port, "/api/stop", json!({"chatId": self.chat_id}), ctx.clone());
    }

    fn reset(&mut self, ctx: &egui::Context) {
        post_json(self.port, "/api/reset", json!({"chatId": self.chat_id}), ctx.clone());
        self.messages.clear();
        self.queue.clear();
        self.busy = false;
        self.receiver = None;
        static NEXT: AtomicU64 = AtomicU64::new(10_000);
        self.chat_id = new_chat_id(NEXT.fetch_add(1, Ordering::Relaxed));
    }
}

fn execute_tool(app: &mut VectorcraftApp, name: &str, mut input: Value) -> Result<Value, String> {
    match name {
        "inspect_document" => {
            rename(&mut input, "child_limit", "childLimit");
            app.run("document.inspect", input)
        }
        "find_objects" => app.run("document.find", input),
        "read_object" => {
            rename(&mut input, "child_limit", "childLimit");
            if input.get("summary").is_none() && (input.get("depth").is_some() || input.get("childLimit").is_some()) {
                input["summary"] = Value::Bool(true);
            }
            app.run("document.node", input)
        }
        "list_commands" => {
            let filter = input.get("filter").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
            let enabled_only = input.get("enabled_only").and_then(Value::as_bool).unwrap_or(false);
            let commands: Vec<_> = app
                .session
                .commands()
                .into_iter()
                .filter(|c| !enabled_only || c.enabled)
                .filter(|c| filter.is_empty() || format!("{} {} {}", c.id, c.label, c.menu.join(" ")).to_ascii_lowercase().contains(&filter))
                .collect();
            serde_json::to_value(commands).map_err(|e| e.to_string())
        }
        "run_command" => {
            let id = input.get("id").and_then(Value::as_str).ok_or_else(|| "run_command needs `id`".to_string())?.to_string();
            let params = input.get("params").cloned().unwrap_or_else(|| json!({}));
            app.run(&id, params)
        }
        "run_batch" => {
            if let Some(object) = input.as_object_mut() {
                object.remove("atomic");
                object.remove("stop_on_error");
            }
            app.run("command.batch", input)
        }
        _ => Err(format!("Unknown assistant tool `{name}`")),
    }
}

/// `/resume` → `Some(None)`, `/resume 2` → `Some(Some(2))`, anything else → `None`.
fn resume_command(text: &str) -> Option<Option<usize>> {
    let mut words = text.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("/resume") {
        return None;
    }
    match (words.next(), words.next()) {
        (None, _) => Some(None),
        (Some(n), None) => n.parse().ok().map(Some),
        _ => None,
    }
}

/// Chat ids are unique across runs: the server saves conversations by id, and a
/// reused id would continue an old one.
fn new_chat_id(n: u64) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
    #[cfg(target_arch = "wasm32")]
    let millis = 0u128;
    format!("vectorcraft-{millis}-{}-{n}", std::process::id())
}

fn rename(value: &mut Value, from: &str, to: &str) {
    if let Some(object) = value.as_object_mut()
        && let Some(value) = object.remove(from)
    {
        object.insert(to.to_string(), value);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn start_chat(port: u16, body: Value, sender: mpsc::Sender<WorkerEvent>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let result = stream_request(port, "/api/chat", &body, |event| {
            let _ = sender.send(WorkerEvent::Event(event));
            ctx.request_repaint();
        });
        if let Err(error) = result {
            let _ = sender.send(WorkerEvent::Error(error));
        }
        let _ = sender.send(WorkerEvent::Finished);
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
fn start_chat(_port: u16, _body: Value, sender: mpsc::Sender<WorkerEvent>, ctx: egui::Context) {
    let _ = sender.send(WorkerEvent::Error("The built-in assistant currently needs the desktop app.".into()));
    let _ = sender.send(WorkerEvent::Finished);
    ctx.request_repaint();
}

#[cfg(not(target_arch = "wasm32"))]
fn start_resume(port: u16, current: String, pick: Option<usize>, sender: mpsc::Sender<ResumeEvent>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let _ = sender.send(fetch_resume(port, &current, pick));
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
fn start_resume(_port: u16, _current: String, _pick: Option<usize>, sender: mpsc::Sender<ResumeEvent>, ctx: egui::Context) {
    let _ = sender.send(ResumeEvent::Error("The built-in assistant currently needs the desktop app.".into()));
    ctx.request_repaint();
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_resume(port: u16, current: &str, pick: Option<usize>) -> ResumeEvent {
    let list = match get_json(port, "/api/history") {
        Ok(list) => list,
        Err(error) => return ResumeEvent::Error(error),
    };
    let rows: Vec<Value> = list
        .get("chats")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter(|r| r.get("id").and_then(Value::as_str) != Some(current)).cloned().collect())
        .unwrap_or_default();
    let Some(pick) = pick else { return ResumeEvent::List(rows) };
    let Some(id) = pick.checked_sub(1).and_then(|i| rows.get(i)).and_then(|r| r.get("id")).and_then(Value::as_str) else {
        return ResumeEvent::OutOfRange;
    };
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return ResumeEvent::OutOfRange;
    }
    match get_json(port, &format!("/api/history/{id}")) {
        Ok(saved) => ResumeEvent::Open(saved),
        Err(error) => ResumeEvent::Error(error),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn get_json(port: u16, path: &str) -> Result<Value, String> {
    let mut bytes = Vec::new();
    request_with(port, "GET", path, &Value::Null, |chunk| bytes.extend_from_slice(chunk))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("AI server sent invalid JSON: {e}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn post_json(port: u16, path: &'static str, body: Value, ctx: egui::Context) {
    std::thread::spawn(move || {
        let _ = request(port, path, &body, |_| {});
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
fn post_json(_port: u16, _path: &'static str, _body: Value, _ctx: egui::Context) {}

#[cfg(not(target_arch = "wasm32"))]
fn stream_request(port: u16, path: &str, body: &Value, mut event: impl FnMut(Value)) -> Result<(), String> {
    let mut sse = String::new();
    request(port, path, body, |bytes| {
        sse.push_str(&String::from_utf8_lossy(bytes));
        if sse.contains('\r') {
            sse = sse.replace("\r\n", "\n");
        }
        while let Some(end) = sse.find("\n\n") {
            let block = sse[..end].to_string();
            sse.drain(..end + 2);
            for line in block.lines() {
                if let Some(data) = line.strip_prefix("data:")
                    && let Ok(value) = serde_json::from_str(data.trim())
                {
                    event(value);
                }
            }
        }
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn request(port: u16, path: &str, body: &Value, chunk: impl FnMut(&[u8])) -> Result<(), String> {
    request_with(port, "POST", path, body, chunk)
}

#[cfg(not(target_arch = "wasm32"))]
fn request_with(port: u16, method: &str, path: &str, body: &Value, mut chunk: impl FnMut(&[u8])) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let json = if method == "GET" { Vec::new() } else { serde_json::to_vec(body).map_err(|e| e.to_string())? };
    let mut stream =
        TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().map_err(|e| format!("invalid assistant port: {e}"))?, Duration::from_secs(3))
            .map_err(|e| format!("AI server is not running on 127.0.0.1:{port}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(190))).map_err(|e| e.to_string())?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nX-VectorCraft-AI: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        json.len()
    )
    .and_then(|()| stream.write_all(&json))
    .map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    let mut chunked = false;
    let mut length = None;
    loop {
        line.clear();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        chunked |= lower == "transfer-encoding: chunked";
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    if !(200..300).contains(&status) {
        let mut error = String::new();
        reader.take(16_384).read_to_string(&mut error).map_err(|e| e.to_string())?;
        return Err(format!("AI server returned HTTP {status}: {error}"));
    }
    if chunked {
        loop {
            line.clear();
            reader.read_line(&mut line).map_err(|e| e.to_string())?;
            let size =
                usize::from_str_radix(line.trim().split(';').next().unwrap_or_default(), 16).map_err(|_| "invalid chunked response".to_string())?;
            if size == 0 {
                break;
            }
            let mut bytes = vec![0; size];
            reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            chunk(&bytes);
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).map_err(|e| e.to_string())?;
        }
    } else if let Some(length) = length {
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        chunk(&bytes);
    } else {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        chunk(&bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_moves_a_present_field() {
        let mut value = json!({"child_limit": 12, "depth": 1});
        rename(&mut value, "child_limit", "childLimit");
        assert_eq!(value, json!({"childLimit": 12, "depth": 1}));
    }

    #[test]
    fn resume_command_parses_only_resume() {
        assert_eq!(resume_command("/resume"), Some(None));
        assert_eq!(resume_command("  /Resume 3 "), Some(Some(3)));
        assert_eq!(resume_command("/resume x"), None);
        assert_eq!(resume_command("/resume 1 2"), None);
        assert_eq!(resume_command("please /resume"), None);
        assert_eq!(resume_command(""), None);
    }

    #[test]
    fn rename_leaves_an_absent_field_alone() {
        let mut value = json!({"depth": 1});
        rename(&mut value, "child_limit", "childLimit");
        assert_eq!(value, json!({"depth": 1}));
    }
}
