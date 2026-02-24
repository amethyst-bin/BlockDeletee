use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::stdout;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfigRange};
use crossterm::event::{self, Event as CEvent, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use glob::Pattern;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use strsim::normalized_levenshtein;
use vosk::{set_log_level, CompleteResult, DecodingState, LogLevel, Model, Recognizer};

mod backend_bootstrap;
mod ui_qt;
mod ui_tui;

const MIC_SPEAKER_ID: &str = "mic";
const BLOCK_KEY_PREFIX: &str = "block.minecraft.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiMode {
    Tui,
    Qt,
}

impl UiMode {
    pub(crate) fn as_config_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Qt => "qt",
        }
    }

    pub(crate) fn from_config_str(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "tui" => Some(Self::Tui),
            "qt" => Some(Self::Qt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UiSnapshot {
    pub(crate) logs: Vec<String>,
    pub(crate) mic_ok: bool,
    pub(crate) rec_ok: bool,
    pub(crate) rcon_ok: bool,
    pub(crate) player_online: bool,
    pub(crate) player_name: String,
    pub(crate) rcon_host: String,
    pub(crate) rcon_port: u16,
    pub(crate) rcon_password: String,
    pub(crate) ui_mode: UiMode,
    pub(crate) overlay_error: Option<String>,
}

#[derive(Debug, Clone)]
struct UiLogEntry {
    text: String,
    count: usize,
}

#[derive(Debug)]
pub(crate) struct UiState {
    logs: VecDeque<UiLogEntry>,
    mic_ok: bool,
    rec_ok: bool,
    rcon_ok: bool,
    player_online: bool,
    player_name: String,
    rcon_host: String,
    rcon_port: u16,
    rcon_password: String,
    ui_mode: UiMode,
    overlay_error: Option<String>,
}

pub(crate) type UiHandle = Arc<Mutex<UiState>>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SaveSettingsOutcome {
    pub(crate) restart_required: bool,
}

impl UiState {
    pub(crate) fn new(
        player_name: String,
        rcon_host: String,
        rcon_port: u16,
        rcon_password: String,
        ui_mode: UiMode,
    ) -> Self {
        Self {
            logs: VecDeque::with_capacity(128),
            mic_ok: false,
            rec_ok: false,
            rcon_ok: false,
            player_online: false,
            player_name,
            rcon_host,
            rcon_port,
            rcon_password,
            ui_mode,
            overlay_error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterButton {
    Settings,
    Exit,
}

impl FooterButton {
    fn next(self) -> Self {
        match self {
            Self::Settings => Self::Exit,
            Self::Exit => Self::Settings,
        }
    }

    fn prev(self) -> Self {
        self.next()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Host,
    Port,
    Password,
    PlayerName,
    UiMode,
}

impl SettingsField {
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Connection,
    App,
}

impl SettingsTab {
    fn next(self) -> Self {
        match self {
            Self::Connection => Self::App,
            Self::App => Self::Connection,
        }
    }
    fn prev(self) -> Self {
        self.next()
    }
}

#[derive(Debug, Clone, Copy)]
struct TuiControls {
    selected: FooterButton,
    settings_open: bool,
    settings_field: SettingsField,
    settings_editing: bool,
    settings_tab: SettingsTab,
}

#[derive(Debug, Clone)]
struct SettingsDraft {
    host: String,
    port: String,
    password: String,
    player_name: String,
    ui_mode: UiMode,
}

pub(crate) fn ui_snapshot(ui: &UiHandle) -> UiSnapshot {
    let guard = ui.lock().expect("ui mutex poisoned");
    UiSnapshot {
        logs: guard
            .logs
            .iter()
            .map(|item| {
                if item.count > 1 {
                    format!("{} ({}x)", item.text, item.count)
                } else {
                    item.text.clone()
                }
            })
            .collect(),
        mic_ok: guard.mic_ok,
        rec_ok: guard.rec_ok,
        rcon_ok: guard.rcon_ok,
        player_online: guard.player_online,
        player_name: guard.player_name.clone(),
        rcon_host: guard.rcon_host.clone(),
        rcon_port: guard.rcon_port,
        rcon_password: guard.rcon_password.clone(),
        ui_mode: guard.ui_mode,
        overlay_error: guard.overlay_error.clone(),
    }
}

pub(crate) fn ui_log(ui: &UiHandle, msg: impl Into<String>) {
    let mut guard = ui.lock().expect("ui mutex poisoned");
    let msg = msg.into();

    if let Some(last) = guard.logs.back_mut() {
        if last.text == msg {
            last.count += 1;
            return;
        }
    }

    if guard.logs.len() >= 256 {
        let _ = guard.logs.pop_front();
    }
    if let Some(alert) = classify_overlay_error(&msg) {
        guard.overlay_error = Some(alert);
    }
    guard.logs.push_back(UiLogEntry { text: msg, count: 1 });
}

fn ui_set_mic(ui: &UiHandle, ok: bool) {
    if let Ok(mut guard) = ui.lock() {
        guard.mic_ok = ok;
        if ok {
            guard.overlay_error = None;
        }
    }
}

fn ui_set_rec(ui: &UiHandle, ok: bool) {
    if let Ok(mut guard) = ui.lock() {
        guard.rec_ok = ok;
        if ok {
            guard.overlay_error = None;
        }
    }
}

fn ui_set_rcon(ui: &UiHandle, ok: bool) {
    if let Ok(mut guard) = ui.lock() {
        guard.rcon_ok = ok;
        if ok {
            guard.overlay_error = None;
        }
    }
}

fn ui_set_player_online(ui: &UiHandle, online: bool) {
    if let Ok(mut guard) = ui.lock() {
        guard.player_online = online;
        if online {
            guard.overlay_error = None;
        }
    }
}

fn classify_overlay_error(msg: &str) -> Option<String> {
    let lower = msg.to_lowercase();
    if lower.contains("[rcon-error]") || lower.contains("rcon authentication failed") {
        if lower.contains("authentication") || lower.contains("auth") {
            Some("RCON: ошибка аутентификации".to_string())
        } else {
            Some(msg.to_string())
        }
    } else if lower.contains("[rcon-player-error]") {
        Some(msg.to_string())
    } else if lower.contains("[notify-error]") {
        Some(msg.to_string())
    } else if lower.contains("[microphone-status]") || lower.contains("[recognizer-error]") {
        Some(msg.to_string())
    } else if lower.contains("[settings-error]") || lower.contains("[restart-error]") {
        Some(msg.to_string())
    } else if lower.contains("[backend-error]") {
        Some(msg.to_string())
    } else {
        None
    }
}

fn status_spans(icon: &str, label: &str, ok: bool) -> Vec<Span<'static>> {
    let dot = "●";
    let color = if ok { Color::Green } else { Color::Red };
    let mut spans = vec![Span::styled(
        format!("{icon} {label} {dot}"),
        Style::default().fg(color),
    )];
    if !ok {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "!",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

fn log_color(text: &str) -> Color {
    let lower = text.to_lowercase();
    if lower.contains("error") || lower.contains("ошибка") {
        Color::Red
    } else if lower.contains("warning") || lower.contains("warn") {
        Color::Yellow
    } else if lower.contains("[trigger]") || lower.contains("[startup]") || lower.contains("[notify]") {
        Color::Green
    } else if lower.contains("[player]") {
        Color::Cyan
    } else if lower.contains("[recognized") {
        Color::Magenta
    } else if lower.contains("[partial") || lower.contains("[rcon-debug]") {
        Color::DarkGray
    } else {
        Color::White
    }
}

fn is_rcon_error_like(response: &str) -> bool {
    let s = response.to_lowercase();
    s.contains("unknown or incomplete command")
        || s.contains("error")
        || s.contains("ошибка")
}

struct TuiGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TuiGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| format!("raw mode on error: {e}"))?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen).map_err(|e| format!("enter alt screen error: {e}"))?;
        let backend = CrosstermBackend::new(out);
        let terminal = Terminal::new(backend).map_err(|e| format!("terminal init error: {e}"))?;
        Ok(Self { terminal })
    }

    fn draw(
        &mut self,
        ui: &UiHandle,
        controls: &TuiControls,
        draft: &SettingsDraft,
    ) -> Result<(), String> {
        let snap = ui_snapshot(ui);
        self.terminal
            .draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(5),
                        Constraint::Min(1),
                        Constraint::Length(3),
                    ])
                    .split(f.area());

                let mut status_spans_row = Vec::new();
                status_spans_row.extend(status_spans("󰍹", "MIC", snap.mic_ok));
                status_spans_row.push(Span::raw("   "));
                status_spans_row.extend(status_spans("󰋎", "REC", snap.rec_ok));
                status_spans_row.push(Span::raw("   "));
                status_spans_row.extend(status_spans("󰒓", "RCON", snap.rcon_ok));
                status_spans_row.push(Span::raw("   "));
                status_spans_row.extend(status_spans("󰀄", "PLAYER", snap.player_online));
                let status_line = Line::from(status_spans_row);

                let top_has_problem = !(snap.mic_ok && snap.rec_ok && snap.rcon_ok && snap.player_online);
                let top_border_color = if top_has_problem { Color::Red } else { Color::Green };

                let top = Paragraph::new(vec![
                    Line::from("BlockDeletee"),
                    status_line,
                    Line::from(format!("Игрок: {}", snap.player_name)),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(top_border_color))
                        .title("Status"),
                );
                f.render_widget(top, chunks[0]);

                let visible_log_rows = chunks[1].height.saturating_sub(2) as usize;
                let logs_border_color = snap
                    .logs
                    .last()
                    .map(|s| log_color(s))
                    .unwrap_or(Color::DarkGray);
                let mut log_lines: Vec<Line> = if visible_log_rows == 0 {
                    Vec::new()
                } else {
                    let total = snap.logs.len();
                    let start = total.saturating_sub(visible_log_rows);
                    snap.logs
                        .iter()
                        .skip(start)
                        .map(|s| Line::from(Span::styled(s.clone(), Style::default().fg(log_color(s)))))
                        .collect()
                };
                if log_lines.is_empty() {
                    log_lines.push(Line::from("Ожидание событий..."));
                }
                let logs = Paragraph::new(log_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(logs_border_color))
                            .title("󰍩 Logs"),
                    );
                f.render_widget(logs, chunks[1]);

                let compact_footer = chunks[2].width < 78;
                let footer_lines = if compact_footer {
                    vec![
                        Line::from(vec![
                            footer_button_span("Настройки", controls.selected == FooterButton::Settings),
                            Span::raw("  "),
                            footer_button_span("Выйти", controls.selected == FooterButton::Exit),
                        ]),
                        Line::from(vec![
                            Span::styled("←/→", Style::default().fg(Color::Yellow)),
                            Span::raw(" выбор  "),
                            Span::styled("Enter", Style::default().fg(Color::Yellow)),
                            Span::raw(" ок  "),
                            Span::styled("q", Style::default().fg(Color::Yellow)),
                            Span::raw(" выход"),
                        ]),
                    ]
                } else {
                    vec![Line::from(vec![
                        footer_button_span("Настройки", controls.selected == FooterButton::Settings),
                        Span::raw("  "),
                        footer_button_span("Выйти", controls.selected == FooterButton::Exit),
                        Span::raw("   "),
                        Span::styled("←/→", Style::default().fg(Color::Yellow)),
                        Span::raw(" выбор  "),
                        Span::styled("Enter", Style::default().fg(Color::Yellow)),
                        Span::raw(" подтвердить  "),
                        Span::styled("q", Style::default().fg(Color::Yellow)),
                        Span::raw(" быстрый выход"),
                    ])]
                };

                let footer = Paragraph::new(footer_lines)
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                );
                f.render_widget(footer, chunks[2]);

                if controls.settings_open {
                    let popup = centered_rect(70, 55, f.area());
                    let mut settings_lines = vec![
                        Line::from(Span::styled(
                            "Настройки",
                            Style::default().add_modifier(Modifier::BOLD),
                        )),
                        Line::from(""),
                        settings_tab_line(controls.settings_tab),
                        Line::from(""),
                    ];

                    match controls.settings_tab {
                        SettingsTab::Connection => {
                            settings_lines.push(settings_line(
                                "IP",
                                &draft.host,
                                controls.settings_field == SettingsField::Host,
                                controls.settings_editing
                                    && controls.settings_field == SettingsField::Host,
                            ));
                            settings_lines.push(settings_line(
                                "Port",
                                &draft.port,
                                controls.settings_field == SettingsField::Port,
                                controls.settings_editing
                                    && controls.settings_field == SettingsField::Port,
                            ));
                            let masked_password = if draft.password.is_empty() {
                                String::new()
                            } else {
                                "*".repeat(draft.password.chars().count())
                            };
                            settings_lines.push(settings_line(
                                "RCON Password",
                                &masked_password,
                                controls.settings_field == SettingsField::Password,
                                controls.settings_editing
                                    && controls.settings_field == SettingsField::Password,
                            ));
                        }
                        SettingsTab::App => {
                            settings_lines.push(settings_line(
                                "Username",
                                &draft.player_name,
                                controls.settings_field == SettingsField::PlayerName,
                                controls.settings_editing
                                    && controls.settings_field == SettingsField::PlayerName,
                            ));
                            settings_lines.push(settings_line(
                                "UI Mode",
                                draft.ui_mode.as_config_str(),
                                controls.settings_field == SettingsField::UiMode,
                                controls.settings_editing
                                    && controls.settings_field == SettingsField::UiMode,
                            ));
                        }
                    }

                    settings_lines.push(Line::from(""));
                    settings_lines.push(Line::from(vec![
                        Span::styled("←/→", Style::default().fg(Color::Yellow)),
                        Span::raw(" вкладка/переключить UI mode  "),
                        Span::styled("↑↓", Style::default().fg(Color::Yellow)),
                        Span::raw(" поле"),
                    ]));
                    settings_lines.push(Line::from(vec![
                        Span::styled("Enter", Style::default().fg(Color::Yellow)),
                        Span::raw(" ред./ок  "),
                        Span::styled("S", Style::default().fg(Color::Yellow)),
                        Span::raw(" сохранить  "),
                        Span::styled("Esc", Style::default().fg(Color::Yellow)),
                        Span::raw(" закрыть"),
                    ]));
                    settings_lines.push(Line::from(vec![
                        Span::styled("Примечание:", Style::default().fg(Color::Yellow)),
                        Span::raw(" UI mode / username / RCON password применятся после перезапуска"),
                    ]));

                    let popup_widget = Paragraph::new(settings_lines)
                        .alignment(Alignment::Left)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(Color::Cyan))
                                .title("󰢻 Settings"),
                        );
                    f.render_widget(popup_widget, popup);
                }

                if let Some(err) = &snap.overlay_error {
                    let width = f.area().width.clamp(24, 54);
                    let overlay = Rect {
                        x: f.area().right().saturating_sub(width + 1),
                        y: 1,
                        width,
                        height: 4,
                    };
                    let overlay_widget = Paragraph::new(err.as_str())
                        .wrap(Wrap { trim: true })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(Color::Red))
                                .title(Span::styled(
                                    " Ошибка",
                                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                                )),
                        );
                    f.render_widget(overlay_widget, overlay);
                }
            })
            .map_err(|e| format!("terminal draw error: {e}"))?;
        Ok(())
    }
}

fn settings_line(label: &str, value: &str, selected: bool, editing: bool) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{label}: "),
            Style::default()
                .fg(if selected { Color::Yellow } else { Color::Cyan })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        ),
        Span::styled(
            value.to_string(),
            Style::default().fg(Color::White).bg(if editing { Color::DarkGray } else { Color::Reset }),
        ),
    ];
    if editing {
        spans.push(Span::styled(
            "  ✎",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn settings_tab_line(active: SettingsTab) -> Line<'static> {
    let tab = |label: &str, is_active: bool| {
        let text = if is_active {
            format!("[{label}]")
        } else {
            format!(" {label} ")
        };
        let style = if is_active {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        Span::styled(text, style)
    };
    Line::from(vec![
        tab("Connection", active == SettingsTab::Connection),
        Span::raw(" "),
        tab("App", active == SettingsTab::App),
    ])
}

fn settings_fields_for_tab(tab: SettingsTab) -> &'static [SettingsField] {
    match tab {
        SettingsTab::Connection => &[SettingsField::Host, SettingsField::Port, SettingsField::Password],
        SettingsTab::App => &[SettingsField::PlayerName, SettingsField::UiMode],
    }
}

fn settings_field_next_in_tab(current: SettingsField, tab: SettingsTab) -> SettingsField {
    let fields = settings_fields_for_tab(tab);
    let idx = fields.iter().position(|f| *f == current).unwrap_or(0);
    fields[(idx + 1) % fields.len()]
}

fn settings_field_prev_in_tab(current: SettingsField, tab: SettingsTab) -> SettingsField {
    let fields = settings_fields_for_tab(tab);
    let idx = fields.iter().position(|f| *f == current).unwrap_or(0);
    fields[(idx + fields.len() - 1) % fields.len()]
}

fn default_field_for_tab(tab: SettingsTab) -> SettingsField {
    settings_fields_for_tab(tab)[0]
}

fn footer_button_span(label: &str, selected: bool) -> Span<'static> {
    let text = if selected {
        format!("[ {label} ]")
    } else {
        format!("  {label}  ")
    };
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Span::styled(text, style)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn save_rcon_settings_to_config(path: &Path, host: &str, port: u16) -> Result<(), String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("Не удалось прочитать config `{}`: {e}", path.display()))?;
    let mut json: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Ошибка JSON в config: {e}"))?;

    let root = json
        .as_object_mut()
        .ok_or_else(|| "config.json должен быть объектом".to_string())?;
    let minecraft = root
        .entry("minecraft")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "config.minecraft должен быть объектом".to_string())?;

    minecraft.insert("rcon_host".to_string(), Value::String(host.to_string()));
    minecraft.insert(
        "rcon_port".to_string(),
        Value::Number(serde_json::Number::from(port)),
    );

    let pretty = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Не удалось сериализовать config: {e}"))?;
    fs::write(path, pretty).map_err(|e| format!("Не удалось сохранить config `{}`: {e}", path.display()))
}

fn save_rcon_password_to_config(path: &Path, password: &str) -> Result<(), String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("Не удалось прочитать config `{}`: {e}", path.display()))?;
    let mut json: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Ошибка JSON в config: {e}"))?;

    let root = json
        .as_object_mut()
        .ok_or_else(|| "config.json должен быть объектом".to_string())?;
    let minecraft = root
        .entry("minecraft")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "config.minecraft должен быть объектом".to_string())?;
    minecraft.insert(
        "rcon_password".to_string(),
        Value::String(password.trim().to_string()),
    );

    let pretty = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Не удалось сериализовать config: {e}"))?;
    fs::write(path, pretty)
        .map_err(|e| format!("Не удалось сохранить config `{}`: {e}", path.display()))
}

fn save_ui_mode_to_config(path: &Path, mode: UiMode) -> Result<(), String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("Не удалось прочитать config `{}`: {e}", path.display()))?;
    let mut json: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Ошибка JSON в config: {e}"))?;

    let root = json
        .as_object_mut()
        .ok_or_else(|| "config.json должен быть объектом".to_string())?;
    let ui = root
        .entry("ui")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "config.ui должен быть объектом".to_string())?;
    ui.insert(
        "mode".to_string(),
        Value::String(mode.as_config_str().to_string()),
    );

    let pretty = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Не удалось сериализовать config: {e}"))?;
    fs::write(path, pretty)
        .map_err(|e| format!("Не удалось сохранить config `{}`: {e}", path.display()))
}

fn save_player_name_to_config(path: &Path, player_name: &str) -> Result<(), String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("Не удалось прочитать config `{}`: {e}", path.display()))?;
    let mut json: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Ошибка JSON в config: {e}"))?;

    let root = json
        .as_object_mut()
        .ok_or_else(|| "config.json должен быть объектом".to_string())?;
    let microphone = root
        .entry("microphone")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "config.microphone должен быть объектом".to_string())?;
    microphone.insert(
        "player_name".to_string(),
        Value::String(player_name.trim().to_string()),
    );

    let pretty = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Не удалось сериализовать config: {e}"))?;
    fs::write(path, pretty)
        .map_err(|e| format!("Не удалось сохранить config `{}`: {e}", path.display()))
}

pub(crate) fn restart_current_process() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("Не удалось получить путь exe: {e}"))?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    Command::new(&exe)
        .args(args)
        .spawn()
        .map_err(|e| format!("Не удалось перезапустить `{}`: {e}", exe.display()))?;
    Ok(())
}

fn choose_ui_mode_tui() -> Result<UiMode, String> {
    let mut selected = UiMode::Tui;

    enable_raw_mode().map_err(|e| format!("raw mode on error: {e}"))?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).map_err(|e| format!("enter alt screen error: {e}"))?;
    let backend = CrosstermBackend::new(out);
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("terminal init error: {e}"))?;

    loop {
        terminal
            .draw(|f| {
                let area = centered_rect(70, 40, f.area());
                let lines = vec![
                    Line::from(Span::styled(
                        "Выбери интерфейс",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        mode_option_span("TUI", selected == UiMode::Tui),
                        Span::raw("  "),
                        Span::raw("терминальный интерфейс (быстро, стабильно)"),
                    ]),
                    Line::from(vec![
                        mode_option_span("QT", selected == UiMode::Qt),
                        Span::raw("  "),
                        Span::raw("desktop GUI (Qt/QML)"),
                    ]),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
                        Span::raw(" выбор  "),
                        Span::styled("Enter", Style::default().fg(Color::Yellow)),
                        Span::raw(" подтвердить"),
                    ]),
                ];
                let widget = Paragraph::new(lines)
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .title("Startup"),
                    );
                f.render_widget(widget, area);
            })
            .map_err(|e| format!("terminal draw error: {e}"))?;

        if event::poll(Duration::from_millis(200)).map_err(|e| format!("event poll error: {e}"))? {
            if let CEvent::Key(key) =
                event::read().map_err(|e| format!("event read error: {e}"))?
            {
                match key.code {
                    KeyCode::Up | KeyCode::Left => {
                        selected = match selected {
                            UiMode::Tui => UiMode::Qt,
                            UiMode::Qt => UiMode::Tui,
                        }
                    }
                    KeyCode::Down | KeyCode::Right => {
                        selected = match selected {
                            UiMode::Tui => UiMode::Qt,
                            UiMode::Qt => UiMode::Tui,
                        }
                    }
                    KeyCode::Enter => break,
                    KeyCode::Char('q') | KeyCode::Esc => {
                        selected = UiMode::Tui;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    Ok(selected)
}

fn mode_option_span(label: &str, selected: bool) -> Span<'static> {
    let text = if selected {
        format!("▶ {label}")
    } else {
        format!("  {label}")
    };
    let style = if selected {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Span::styled(text, style)
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Voice-driven block deletion challenge tool (Rust port)"
)]
struct Args {
    #[arg(long, default_value = "config.json")]
    config: PathBuf,

    #[arg(long = "list-audio-devices")]
    list_audio_devices: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrManyStrings {
    One(String),
    Many(Vec<String>),
}

impl OneOrManyStrings {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum DeviceSelector {
    Index(i64),
    Name(String),
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawBlocksConfig {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    extra_aliases: HashMap<String, OneOrManyStrings>,
    #[serde(default)]
    shared_aliases: HashMap<String, OneOrManyStrings>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawMicrophoneConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    player_name: Option<String>,
    #[serde(default)]
    samplerate: Option<u32>,
    #[serde(default)]
    blocksize: Option<u32>,
    #[serde(default)]
    device: Option<DeviceSelector>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawSpeechConfig {
    #[serde(default)]
    model_path: Option<String>,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    cooldown_seconds: Option<f64>,
    #[serde(default)]
    fuzzy_threshold: Option<f64>,
    #[serde(default)]
    use_grammar: Option<bool>,
    #[serde(default)]
    log_partials: Option<bool>,
    #[serde(default)]
    log_recognized: Option<bool>,
    #[serde(default)]
    min_phrase_chars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawMinecraftConfig {
    #[serde(default)]
    rcon_host: Option<String>,
    #[serde(default)]
    rcon_port: Option<u16>,
    #[serde(default)]
    rcon_password: Option<String>,
    #[serde(default)]
    fill_max_blocks: Option<usize>,
    #[serde(default)]
    dimension_y_limits: HashMap<String, [i32; 2]>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawUiConfig {
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawAppConfig {
    #[serde(default)]
    ui: RawUiConfig,
    #[serde(default)]
    blocks: RawBlocksConfig,
    #[serde(default)]
    microphone: RawMicrophoneConfig,
    #[serde(default)]
    speech: RawSpeechConfig,
    #[serde(default)]
    minecraft: RawMinecraftConfig,
}

#[derive(Debug, Clone)]
struct UiConfig {
    mode: Option<UiMode>,
}

#[derive(Debug, Clone)]
struct BlocksConfig {
    file: String,
    extra_aliases: HashMap<String, Vec<String>>,
    shared_aliases: HashMap<String, Vec<String>>,
}

impl BlocksConfig {
    fn custom_alias_phrases(&self) -> Vec<String> {
        let mut out = Vec::new();
        for aliases in self.extra_aliases.values() {
            out.extend(aliases.clone());
        }
        out.extend(self.shared_aliases.keys().cloned());
        out
    }
}

#[derive(Debug, Clone)]
struct MicrophoneConfig {
    enabled: bool,
    player_name: String,
    samplerate: u32,
    blocksize: u32,
    device: Option<DeviceSelector>,
}

#[derive(Debug, Clone)]
struct SpeechConfig {
    model_path: String,
    sample_rate: u32,
    cooldown_seconds: f64,
    fuzzy_threshold: f64,
    use_grammar: bool,
    log_partials: bool,
    log_recognized: bool,
    min_phrase_chars: usize,
}

#[derive(Debug, Clone)]
struct MinecraftConfig {
    rcon_host: String,
    rcon_port: u16,
    rcon_password: String,
    fill_max_blocks: usize,
    dimension_y_limits: HashMap<String, (i32, i32)>,
}

#[derive(Debug, Clone)]
pub(crate) struct AppConfig {
    ui: UiConfig,
    blocks: BlocksConfig,
    microphone: MicrophoneConfig,
    speech: SpeechConfig,
    minecraft: MinecraftConfig,
}

impl AppConfig {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("Не удалось прочитать config `{}`: {e}", path.display()))?;
        let parsed: RawAppConfig =
            serde_json::from_str(&raw).map_err(|e| format!("Ошибка JSON в config: {e}"))?;

        let blocks = BlocksConfig {
            file: nonempty_or(parsed.blocks.file, "blocks.json"),
            extra_aliases: clean_alias_map(parsed.blocks.extra_aliases),
            shared_aliases: clean_alias_map(parsed.blocks.shared_aliases),
        };

        let ui = UiConfig {
            mode: parsed
                .ui
                .mode
                .as_deref()
                .and_then(UiMode::from_config_str),
        };

        let microphone = MicrophoneConfig {
            enabled: parsed.microphone.enabled.unwrap_or(true),
            player_name: parsed.microphone.player_name.unwrap_or_default().trim().to_string(),
            samplerate: parsed.microphone.samplerate.unwrap_or(48_000),
            blocksize: parsed.microphone.blocksize.unwrap_or(9_600),
            device: parsed.microphone.device.and_then(|d| match d {
                DeviceSelector::Name(s) if s.trim().is_empty() => None,
                other => Some(other),
            }),
        };

        let mut fuzzy_threshold = parsed.speech.fuzzy_threshold.unwrap_or(0.70);
        if fuzzy_threshold > 0.0 {
            fuzzy_threshold = fuzzy_threshold.clamp(0.5, 0.99);
        }

        let speech = SpeechConfig {
            model_path: nonempty_or(parsed.speech.model_path, "models/vosk-model-small-ru-0.22"),
            sample_rate: parsed.speech.sample_rate.unwrap_or(48_000),
            cooldown_seconds: parsed.speech.cooldown_seconds.unwrap_or(2.0),
            fuzzy_threshold,
            use_grammar: parsed.speech.use_grammar.unwrap_or(false),
            log_partials: parsed.speech.log_partials.unwrap_or(false),
            log_recognized: parsed.speech.log_recognized.unwrap_or(false),
            min_phrase_chars: parsed.speech.min_phrase_chars.unwrap_or(2),
        };

        let mut limits = HashMap::from([
            ("minecraft:overworld".to_string(), (-64, 319)),
            ("minecraft:the_nether".to_string(), (0, 127)),
            ("minecraft:the_end".to_string(), (0, 255)),
        ]);
        for (dim, pair) in parsed.minecraft.dimension_y_limits {
            let mut y1 = pair[0];
            let mut y2 = pair[1];
            if y1 > y2 {
                std::mem::swap(&mut y1, &mut y2);
            }
            let key = dim.trim().to_string();
            if !key.is_empty() {
                limits.insert(key, (y1, y2));
            }
        }

        let minecraft = MinecraftConfig {
            rcon_host: nonempty_or(parsed.minecraft.rcon_host, "127.0.0.1"),
            rcon_port: parsed.minecraft.rcon_port.unwrap_or(25575),
            rcon_password: parsed.minecraft.rcon_password.unwrap_or_default().trim().to_string(),
            fill_max_blocks: parsed.minecraft.fill_max_blocks.unwrap_or(32768).max(1),
            dimension_y_limits: limits,
        };

        Ok(Self {
            ui,
            blocks,
            microphone,
            speech,
            minecraft,
        })
    }
}

fn nonempty_or(value: Option<String>, default: &str) -> String {
    value
        .unwrap_or_else(|| default.to_string())
        .trim()
        .to_string()
        .if_empty(default)
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

fn clean_alias_map(input: HashMap<String, OneOrManyStrings>) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for (key, values) in input {
        let k = key.trim().to_string();
        if k.is_empty() {
            continue;
        }
        let cleaned: Vec<String> = values
            .into_vec()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !cleaned.is_empty() {
            out.insert(k, cleaned);
        }
    }
    out
}

fn normalize_text(text: &str) -> String {
    let lowered = text.to_lowercase().replace('ё', "е").replace('э', "е");
    let mut buf = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch.is_whitespace() {
            buf.push(ch);
        } else {
            buf.push(' ');
        }
    }
    buf.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn block_id_from_language_key(key: &str) -> Option<String> {
    if !key.starts_with(BLOCK_KEY_PREFIX) {
        return None;
    }
    let path = &key[BLOCK_KEY_PREFIX.len()..];
    if path.is_empty() || path.contains('.') {
        return None;
    }
    if !path
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '/' | '-'))
    {
        return None;
    }
    Some(format!("minecraft:{path}"))
}

fn normalize_block_target(raw_target: &str) -> String {
    let target = raw_target.trim();
    if target.starts_with(BLOCK_KEY_PREFIX) {
        return block_id_from_language_key(target).unwrap_or_else(|| target.to_string());
    }
    let has_glob = target.contains('*') || target.contains('?') || target.contains('[');
    if !target.contains(':') && !has_glob {
        format!("minecraft:{target}")
    } else {
        target.to_string()
    }
}

#[derive(Debug, Clone)]
struct BlockCatalog {
    alias_to_blocks: HashMap<String, Vec<String>>,
    aliases_by_word_count: HashMap<usize, Vec<String>>,
    sorted_aliases: Vec<String>,
}

impl BlockCatalog {
    fn load(
        blocks_file: &Path,
        extra_aliases: &HashMap<String, Vec<String>>,
        shared_aliases: &HashMap<String, Vec<String>>,
    ) -> Result<Self, String> {
        let raw = fs::read_to_string(blocks_file).map_err(|e| {
            format!(
                "Не удалось прочитать blocks.json `{}`: {e}",
                blocks_file.display()
            )
        })?;
        let parsed: Value =
            serde_json::from_str(&raw).map_err(|e| format!("Ошибка JSON в blocks.json: {e}"))?;
        let object = parsed
            .as_object()
            .ok_or_else(|| "blocks.json должен быть объектом JSON".to_string())?;

        let mut mapping: HashMap<String, HashSet<String>> = HashMap::new();
        let mut known_block_ids: HashSet<String> = HashSet::new();

        for (key, localized_name) in object {
            let Some(block_id) = block_id_from_language_key(key) else {
                continue;
            };
            known_block_ids.insert(block_id.clone());

            let aliases = match localized_name {
                Value::String(s) => vec![s.clone()],
                Value::Array(arr) => arr
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>(),
                other => vec![other.to_string()],
            };
            for alias in aliases {
                let n = normalize_text(&alias);
                if !n.is_empty() {
                    mapping.entry(n).or_default().insert(block_id.clone());
                }
            }
        }

        for (block_id, aliases) in extra_aliases {
            let normalized_block_id = normalize_block_target(block_id);
            for alias in aliases {
                let n = normalize_text(alias);
                if !n.is_empty() {
                    mapping
                        .entry(n)
                        .or_default()
                        .insert(normalized_block_id.clone());
                }
            }
        }

        for (alias, targets) in shared_aliases {
            let normalized_alias = normalize_text(alias);
            if normalized_alias.is_empty() {
                continue;
            }

            for target in targets {
                let normalized_target = normalize_block_target(target);
                if normalized_target.contains('*')
                    || normalized_target.contains('?')
                    || normalized_target.contains('[')
                {
                    let Ok(pattern) = Pattern::new(&normalized_target) else {
                        continue;
                    };
                    for block_id in &known_block_ids {
                        if pattern.matches(block_id) {
                            mapping
                                .entry(normalized_alias.clone())
                                .or_default()
                                .insert(block_id.clone());
                        }
                    }
                } else {
                    mapping
                        .entry(normalized_alias.clone())
                        .or_default()
                        .insert(normalized_target);
                }
            }
        }

        let alias_to_blocks: HashMap<String, Vec<String>> = mapping
            .into_iter()
            .map(|(alias, set)| {
                let mut blocks: Vec<String> = set.into_iter().collect();
                blocks.sort();
                (alias, blocks)
            })
            .collect();

        let mut aliases_by_word_count: HashMap<usize, Vec<String>> = HashMap::new();
        let mut sorted_aliases: Vec<String> = alias_to_blocks.keys().cloned().collect();
        for alias in &sorted_aliases {
            let wc = alias.split_whitespace().count();
            aliases_by_word_count.entry(wc).or_default().push(alias.clone());
        }
        sorted_aliases.sort_by(|a, b| {
            let a_wc = a.split_whitespace().count();
            let b_wc = b.split_whitespace().count();
            b_wc.cmp(&a_wc)
                .then_with(|| b.len().cmp(&a.len()))
                .then_with(|| a.cmp(b))
        });

        Ok(Self {
            alias_to_blocks,
            aliases_by_word_count,
            sorted_aliases,
        })
    }

    fn exact_match_aliases(&self, normalized_text: &str) -> HashSet<String> {
        let padded = format!(" {normalized_text} ");
        let mut exact = HashSet::new();
        for alias in &self.sorted_aliases {
            let token = format!(" {alias} ");
            if padded.contains(&token) {
                exact.insert(alias.clone());
            }
        }
        exact
    }

    fn make_ngrams(words: &[&str], n: usize) -> Vec<String> {
        if n == 0 || n > words.len() {
            return Vec::new();
        }
        (0..=words.len() - n)
            .map(|i| words[i..i + n].join(" "))
            .collect()
    }

    fn is_plausible_length(alias: &str, candidate: &str) -> bool {
        let diff = alias.len().abs_diff(candidate.len());
        let max_diff = 2usize.max((alias.len() as f64 * 0.35) as usize);
        diff <= max_diff
    }

    fn fuzzy_match_aliases(
        &self,
        normalized_text: &str,
        threshold: f64,
        already_matched: &HashSet<String>,
    ) -> HashSet<String> {
        let words: Vec<&str> = normalized_text.split_whitespace().collect();
        if words.is_empty() {
            return HashSet::new();
        }

        let max_alias_words = self.aliases_by_word_count.keys().copied().max().unwrap_or(1);
        let max_n = max_alias_words.min(words.len());
        let mut ngram_cache: HashMap<usize, Vec<String>> = HashMap::new();
        for n in 1..=max_n {
            ngram_cache.insert(n, Self::make_ngrams(&words, n));
        }

        let mut collapsed_cache: HashMap<usize, Vec<String>> = HashMap::new();
        for n in 2..=3.min(words.len()) {
            let mut items = Vec::new();
            for i in 0..=words.len() - n {
                items.push(words[i..i + n].join(""));
            }
            collapsed_cache.insert(n, items);
        }

        let mut fuzzy = HashSet::new();
        for (word_count, aliases) in &self.aliases_by_word_count {
            let mut candidates = ngram_cache.get(word_count).cloned().unwrap_or_default();
            if *word_count == 1 {
                if let Some(v) = collapsed_cache.get(&2) {
                    candidates.extend(v.clone());
                }
                if let Some(v) = collapsed_cache.get(&3) {
                    candidates.extend(v.clone());
                }
            }
            if candidates.is_empty() {
                continue;
            }

            for alias in aliases {
                if already_matched.contains(alias) || alias.chars().count() < 5 {
                    continue;
                }
                let alias_first = alias.chars().next();
                for candidate in &candidates {
                    if !Self::is_plausible_length(alias, candidate) {
                        continue;
                    }
                    if alias_first != candidate.chars().next() {
                        continue;
                    }
                    if normalized_levenshtein(alias, candidate) >= threshold {
                        fuzzy.insert(alias.clone());
                        break;
                    }
                }
            }
        }
        fuzzy
    }

    fn match_blocks(&self, text: &str, fuzzy_threshold: f64) -> Vec<String> {
        let normalized = normalize_text(text);
        if normalized.is_empty() {
            return Vec::new();
        }

        let exact_aliases = self.exact_match_aliases(&normalized);
        let mut matched_aliases = exact_aliases.clone();
        if fuzzy_threshold > 0.0 {
            let fuzzy = self.fuzzy_match_aliases(&normalized, fuzzy_threshold, &matched_aliases);
            matched_aliases.extend(fuzzy);
        }

        let mut matched_blocks = Vec::new();
        let mut seen = HashSet::new();
        for alias in &self.sorted_aliases {
            if !matched_aliases.contains(alias) {
                continue;
            }
            if let Some(blocks) = self.alias_to_blocks.get(alias) {
                for block_id in blocks {
                    if seen.insert(block_id.clone()) {
                        matched_blocks.push(block_id.clone());
                    }
                }
            }
        }
        matched_blocks
    }

    fn alias_count(&self) -> usize {
        self.alias_to_blocks.len()
    }

    fn aliases(&self) -> Vec<String> {
        self.alias_to_blocks.keys().cloned().collect()
    }
}

#[derive(Debug, Clone)]
struct RecognizedPhraseEvent {
    speaker_id: String,
    text: String,
    is_partial: bool,
}

#[derive(Debug, Clone)]
struct RepeatGateState {
    last_seen: Instant,
    count: usize,
}

#[derive(Debug, Clone)]
struct CachedChunkContext {
    fetched_at: Instant,
    context: PlayerChunkContext,
}

#[derive(Debug, Clone, Default)]
struct PartialProgressState {
    last_partial: String,
    processed_committed_words: usize,
}

fn list_input_devices() -> Result<Vec<String>, String> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    let devices = host
        .input_devices()
        .map_err(|e| format!("Не удалось получить список аудио-устройств: {e}"))?;

    let mut lines = Vec::new();
    for (index, device) in devices.enumerate() {
        let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
        let marker = if name == default_name { "*" } else { " " };
        let (channels, samplerate) = device
            .default_input_config()
            .map(|cfg| (cfg.channels(), cfg.sample_rate().0))
            .unwrap_or((0, 0));
        lines.push(format!(
            "{marker} {index}: {name} | in={channels} | default_samplerate={samplerate}"
        ));
    }
    Ok(lines)
}

fn resolve_input_device(selector: &Option<DeviceSelector>) -> Result<Device, String> {
    let host = cpal::default_host();
    match selector {
        None => host
            .default_input_device()
            .ok_or_else(|| "Не найдено устройство ввода по умолчанию".to_string()),
        Some(DeviceSelector::Index(index)) => {
            if *index < 0 {
                return Err(format!("Неверный индекс устройства: {index}"));
            }
            let idx = *index as usize;
            let mut devices = host
                .input_devices()
                .map_err(|e| format!("Не удалось получить аудио-устройства: {e}"))?;
            devices
                .nth(idx)
                .ok_or_else(|| format!("Устройство ввода с индексом {idx} не найдено"))
        }
        Some(DeviceSelector::Name(name)) => {
            let needle = name.trim().to_lowercase();
            if needle.is_empty() {
                return host
                    .default_input_device()
                    .ok_or_else(|| "Не найдено устройство ввода по умолчанию".to_string());
            }
            let devices = host
                .input_devices()
                .map_err(|e| format!("Не удалось получить аудио-устройства: {e}"))?;
            for device in devices {
                let dev_name = device.name().unwrap_or_default();
                if dev_name.to_lowercase().contains(&needle) {
                    return Ok(device);
                }
            }
            Err(format!("Устройство ввода с именем `{name}` не найдено"))
        }
    }
}

fn choose_input_config(
    device: &Device,
    sample_rate: u32,
    blocksize: u32,
) -> Result<(SupportedStreamConfigRange, StreamConfig), String> {
    let ranges: Vec<SupportedStreamConfigRange> = device
        .supported_input_configs()
        .map_err(|e| format!("Не удалось получить поддерживаемые аудио-конфиги: {e}"))?
        .collect();

    let format_rank = |fmt: SampleFormat| -> u8 {
        match fmt {
            SampleFormat::I16 => 0,
            SampleFormat::F32 => 1,
            SampleFormat::I32 => 2,
            SampleFormat::U16 => 3,
            SampleFormat::I8 => 4,
            SampleFormat::U8 => 5,
            SampleFormat::U32 => 6,
            SampleFormat::F64 => 7,
            _ => 100,
        }
    };

    let mut candidates: Vec<SupportedStreamConfigRange> = ranges
        .into_iter()
        .filter(|range| {
            sample_rate >= range.min_sample_rate().0
                && sample_rate <= range.max_sample_rate().0
                && range.channels() > 0
        })
        .collect();

    candidates.sort_by_key(|r| (format_rank(r.sample_format()), r.channels().saturating_sub(1)));

    for range in candidates {
        let cfg = StreamConfig {
            channels: range.channels(),
            sample_rate: SampleRate(sample_rate),
            buffer_size: BufferSize::Fixed(blocksize),
        };
        return Ok((range, cfg));
    }

    let def = device
        .default_input_config()
        .map_err(|e| format!("Не удалось получить default input config: {e}"))?;
    Err(format!(
        "Не найден поддерживаемый аудио-конфиг для sample_rate={sample_rate}. \
default={}/{}",
        def.channels(),
        def.sample_rate().0
    ))
}

struct MicrophoneSource {
    stream: Option<Stream>,
}

impl MicrophoneSource {
    fn start(
        samplerate: u32,
        blocksize: u32,
        device_selector: &Option<DeviceSelector>,
        ui: UiHandle,
        on_pcm: impl Fn(Vec<i16>) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let device = resolve_input_device(device_selector)?;
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
        let (supported_range, stream_config) = choose_input_config(&device, samplerate, blocksize)?;
        let sample_format = supported_range.sample_format();
        let channels = stream_config.channels as usize;
        let on_pcm = Arc::new(on_pcm);

        let err_fn = {
            let ui = Arc::clone(&ui);
            move |err| {
                ui_set_mic(&ui, false);
                ui_log(&ui, format!("[microphone-status] {err}"));
            }
        };

        let stream = match sample_format {
            SampleFormat::I8 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[i8], _| {
                            let mono = to_mono_i8(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (i8): {e}"))?
            }
            SampleFormat::U8 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[u8], _| {
                            let mono = to_mono_u8(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (u8): {e}"))?
            }
            SampleFormat::I16 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[i16], _| {
                            let mono = to_mono_i16(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (i16): {e}"))?
            }
            SampleFormat::U16 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[u16], _| {
                            let mono = to_mono_u16(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (u16): {e}"))?
            }
            SampleFormat::F32 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[f32], _| {
                            let mono = to_mono_f32(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (f32): {e}"))?
            }
            SampleFormat::I32 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[i32], _| {
                            let mono = to_mono_i32(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (i32): {e}"))?
            }
            SampleFormat::U32 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[u32], _| {
                            let mono = to_mono_u32(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (u32): {e}"))?
            }
            SampleFormat::F64 => {
                let on_pcm = Arc::clone(&on_pcm);
                device
                    .build_input_stream(
                        &stream_config,
                        move |data: &[f64], _| {
                            let mono = to_mono_f64(data, channels);
                            on_pcm(mono);
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Не удалось создать аудио-поток (f64): {e}"))?
            }
            other => {
                return Err(format!(
                    "Неподдерживаемый формат аудио `{other:?}`. Попробуй другое устройство."
                ))
            }
        };

        stream
            .play()
            .map_err(|e| format!("Не удалось запустить аудио-поток: {e}"))?;
        ui_set_mic(&ui, true);
        ui_log(
            &ui,
            format!(
            "[microphone] запущен: {device_name} | channels={} | sample_rate={} | format={:?}",
            stream_config.channels,
            stream_config.sample_rate.0,
            sample_format
        ),
        );

        Ok(Self {
            stream: Some(stream),
        })
    }

    fn stop(&mut self) {
        self.stream.take();
        // UI status is flipped by caller on shutdown.
    }
}

fn to_mono_i8(data: &[i8], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data.iter().map(|v| (*v as i16) << 8).collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: i32 = chunk.iter().map(|v| *v as i32).sum();
            ((sum / chunk.len() as i32) as i16) << 8
        })
        .collect()
}

fn to_mono_u8(data: &[u8], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data
            .iter()
            .map(|v| (((*v as i32) - 128) << 8) as i16)
            .collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: i32 = chunk.iter().map(|v| *v as i32).sum();
            let avg = sum / chunk.len() as i32;
            ((avg - 128) << 8) as i16
        })
        .collect()
}

fn to_mono_i16(data: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: i32 = chunk.iter().map(|v| *v as i32).sum();
            (sum / chunk.len() as i32) as i16
        })
        .collect()
}

fn to_mono_u16(data: &[u16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data.iter().map(|v| (*v as i32 - 32768) as i16).collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: i64 = chunk.iter().map(|v| *v as i64).sum();
            let avg = (sum / chunk.len() as i64) as i32;
            (avg - 32768) as i16
        })
        .collect()
}

fn to_mono_i32(data: &[i32], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data.iter().map(|v| (*v >> 16) as i16).collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: i64 = chunk.iter().map(|v| *v as i64).sum();
            let avg = (sum / chunk.len() as i64) as i32;
            (avg >> 16) as i16
        })
        .collect()
}

fn to_mono_u32(data: &[u32], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data
            .iter()
            .map(|v| (((*v as i64) - 2_147_483_648_i64) >> 16) as i16)
            .collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: i128 = chunk.iter().map(|v| *v as i128).sum();
            let avg = (sum / chunk.len() as i128) as i64;
            ((avg - 2_147_483_648_i64) >> 16) as i16
        })
        .collect()
}

fn to_mono_f32(data: &[f32], channels: usize) -> Vec<i16> {
    fn conv(v: f32) -> i16 {
        let x = v.clamp(-1.0, 1.0);
        (x * i16::MAX as f32) as i16
    }
    if channels <= 1 {
        return data.iter().map(|v| conv(*v)).collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: f32 = chunk.iter().copied().sum();
            conv(sum / chunk.len() as f32)
        })
        .collect()
}

fn to_mono_f64(data: &[f64], channels: usize) -> Vec<i16> {
    fn conv(v: f64) -> i16 {
        let x = v.clamp(-1.0, 1.0);
        (x * i16::MAX as f64) as i16
    }
    if channels <= 1 {
        return data.iter().map(|v| conv(*v)).collect();
    }
    data.chunks(channels)
        .map(|chunk| {
            let sum: f64 = chunk.iter().copied().sum();
            conv(sum / chunk.len() as f64)
        })
        .collect()
}

fn spawn_recognizer_worker(
    model_path: PathBuf,
    sample_rate: u32,
    log_partials: bool,
    grammar_phrases: Option<Vec<String>>,
    ui: UiHandle,
    shutdown: Arc<AtomicBool>,
    pcm_rx: Receiver<Vec<i16>>,
    text_tx: Sender<RecognizedPhraseEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        set_log_level(LogLevel::Warn);
        let Some(model) = Model::new(model_path.to_string_lossy().to_string()) else {
            ui_set_rec(&ui, false);
            ui_log(
                &ui,
                format!(
                "[recognizer-error] Не удалось загрузить Vosk model из `{}`",
                model_path.display()
            ),
            );
            return;
        };

        let mut recognizer = if let Some(phrases) = grammar_phrases.as_ref().filter(|p| !p.is_empty()) {
            let refs: Vec<&str> = phrases.iter().map(|s| s.as_str()).collect();
            match Recognizer::new_with_grammar(&model, sample_rate as f32, &refs) {
                Some(r) => r,
                None => {
                    ui_log(
                        &ui,
                        "[recognizer-warning] grammar mode unavailable for current model, fallback to default"
                            .to_string(),
                    );
                    match Recognizer::new(&model, sample_rate as f32) {
                        Some(r) => r,
                        None => {
                            ui_set_rec(&ui, false);
                            ui_log(&ui, "[recognizer-error] Не удалось создать Vosk recognizer");
                            return;
                        }
                    }
                }
            }
        } else {
            match Recognizer::new(&model, sample_rate as f32) {
                Some(r) => r,
                None => {
                    ui_set_rec(&ui, false);
                    ui_log(&ui, "[recognizer-error] Не удалось создать Vosk recognizer");
                    return;
                }
            }
        };
        recognizer.set_words(false);
        recognizer.set_partial_words(false);
        ui_set_rec(&ui, true);
        ui_log(&ui, "[recognizer] запущен");
        let mut last_partial_sent = String::new();

        loop {
            match pcm_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => match recognizer.accept_waveform(&chunk) {
                    Ok(DecodingState::Finalized) => {
                        if let Some(text) = extract_complete_text(recognizer.result()) {
                            last_partial_sent.clear();
                            let _ = text_tx.send(RecognizedPhraseEvent {
                                speaker_id: MIC_SPEAKER_ID.to_string(),
                                text,
                                is_partial: false,
                            });
                        }
                    }
                    Ok(DecodingState::Running) => {
                        let partial = recognizer.partial_result().partial.to_string();
                        let partial_trimmed = partial.trim().to_string();
                        if !partial_trimmed.is_empty() && partial_trimmed != last_partial_sent {
                            last_partial_sent = partial_trimmed.clone();
                            let _ = text_tx.send(RecognizedPhraseEvent {
                                speaker_id: MIC_SPEAKER_ID.to_string(),
                                text: partial_trimmed.clone(),
                                is_partial: true,
                            });
                        }
                        if log_partials && !partial_trimmed.is_empty() {
                                ui_log(&ui, format!("[partial:{MIC_SPEAKER_ID}] {}", partial));
                        }
                    }
                    Ok(DecodingState::Failed) => {
                        ui_log(&ui, "[recognizer-error] Vosk decoding failed");
                    }
                    Err(err) => {
                        ui_log(&ui, format!("[recognizer-error] accept_waveform: {err}"));
                    }
                },
                Err(RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if let Some(text) = extract_complete_text(recognizer.final_result()) {
            let _ = text_tx.send(RecognizedPhraseEvent {
                speaker_id: MIC_SPEAKER_ID.to_string(),
                text,
                is_partial: false,
            });
        }
        ui_set_rec(&ui, false);
        ui_log(&ui, "[recognizer] остановлен");
    })
}

fn extract_complete_text(result: CompleteResult<'_>) -> Option<String> {
    match result {
        CompleteResult::Single(single) => {
            let text = single.text.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        CompleteResult::Multiple(multi) => multi
            .alternatives
            .first()
            .map(|a| a.text.trim().to_string())
            .filter(|t| !t.is_empty()),
    }
}

#[derive(Debug)]
struct RconPacket {
    id: i32,
    kind: i32,
    body: String,
}

struct MinecraftRconClient {
    stream: TcpStream,
    next_id: i32,
}

impl MinecraftRconClient {
    fn connect(host: &str, port: u16, password: &str) -> Result<Self, String> {
        let addr = format!("{host}:{port}");
        let resolved = addr
            .to_socket_addrs()
            .map_err(|e| format!("RCON resolve error `{addr}`: {e}"))?
            .next()
            .ok_or_else(|| format!("RCON address not resolved: {addr}"))?;
        let stream =
            TcpStream::connect_timeout(&resolved, Duration::from_secs(3)).map_err(|e| {
                format!("RCON connect error `{addr}`: {e}")
            })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| format!("RCON set_read_timeout error: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| format!("RCON set_write_timeout error: {e}"))?;

        let mut client = Self { stream, next_id: 1 };
        let auth_id = client.send_packet(3, password)?;

        loop {
            let packet = client.read_packet()?;
            if packet.kind == 2 {
                if packet.id == -1 {
                    return Err("RCON authentication failed".to_string());
                }
                if packet.id != auth_id {
                    continue;
                }
                break;
            }
        }

        Ok(client)
    }

    fn cmd(&mut self, cmd: &str) -> Result<String, String> {
        if cmd.len() > 1413 {
            return Err("RCON command too long for Minecraft (>1413 bytes)".to_string());
        }
        let _command_id = self.send_packet(2, cmd)?;
        thread::sleep(Duration::from_millis(3));

        let mut result = String::new();
        loop {
            let packet = self.read_packet()?;
            if packet.kind == 0 || packet.kind == 2 {
                result.push_str(&packet.body);
            }

            if !self.has_pending_data()? {
                return Ok(result.trim().to_string());
            }
        }
    }

    fn has_pending_data(&mut self) -> Result<bool, String> {
        self.stream
            .set_nonblocking(true)
            .map_err(|e| format!("RCON set_nonblocking(true) error: {e}"))?;
        let mut one = [0u8; 1];
        let pending = match self.stream.peek(&mut one) {
            Ok(0) => false,
            Ok(_) => true,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(err) => {
                let _ = self.stream.set_nonblocking(false);
                return Err(format!("RCON peek error: {err}"));
            }
        };
        self.stream
            .set_nonblocking(false)
            .map_err(|e| format!("RCON set_nonblocking(false) error: {e}"))?;
        Ok(pending)
    }

    fn send_packet(&mut self, kind: i32, body: &str) -> Result<i32, String> {
        let id: i32 = 0;
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);

        let body_bytes = body.as_bytes();
        let length = 4 + 4 + body_bytes.len() + 2;
        let mut packet = Vec::with_capacity(4 + length);
        packet.extend_from_slice(&(length as i32).to_le_bytes());
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&kind.to_le_bytes());
        packet.extend_from_slice(body_bytes);
        packet.extend_from_slice(&[0, 0]);
        self.stream
            .write_all(&packet)
            .map_err(|e| format!("RCON write error: {e}"))?;
        Ok(id)
    }

    fn read_packet(&mut self) -> Result<RconPacket, String> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .map_err(|e| format!("RCON read length error: {e}"))?;
        let length = i32::from_le_bytes(len_buf);
        if !(10..=1024 * 1024).contains(&length) {
            return Err(format!("RCON invalid packet length: {length}"));
        }
        let mut rest = vec![0u8; length as usize];
        self.stream
            .read_exact(&mut rest)
            .map_err(|e| format!("RCON read packet error: {e}"))?;
        if rest.len() < 10 {
            return Err("RCON packet too short".to_string());
        }
        let id = i32::from_le_bytes(rest[0..4].try_into().unwrap());
        let kind = i32::from_le_bytes(rest[4..8].try_into().unwrap());
        let body_bytes = &rest[8..rest.len().saturating_sub(2)];
        let body = String::from_utf8_lossy(body_bytes).to_string();
        Ok(RconPacket { id, kind, body })
    }
}

#[derive(Debug)]
struct RconError(String);
#[derive(Debug)]
struct PlayerLookupError(String);

impl std::fmt::Display for RconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::fmt::Display for PlayerLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RconError {}
impl std::error::Error for PlayerLookupError {}

#[derive(Debug, Clone)]
struct ChunkDeleteResult {
    player_name: String,
    block_id: String,
    dimension: String,
    chunk_x: i32,
    chunk_z: i32,
    commands_sent: usize,
}

#[derive(Debug, Clone)]
struct PlayerChunkContext {
    player_name: String,
    dimension: String,
    chunk_x: i32,
    chunk_z: i32,
    x1: i32,
    x2: i32,
    z1: i32,
    z2: i32,
    segments: Vec<(i32, i32)>,
}

struct RconRuntime {
    host: String,
    port: u16,
    client: Option<MinecraftRconClient>,
}

struct MinecraftRconService {
    ui: UiHandle,
    password: String,
    fill_max_blocks: usize,
    dimension_y_limits: HashMap<String, (i32, i32)>,
    runtime: Mutex<RconRuntime>,
    coord_block_re: Regex,
    nbt_pos_re: Regex,
    float_re: Regex,
    dimension_re: Regex,
    nbt_dimension_re: Regex,
    player_re: Regex,
    block_re: Regex,
}

impl MinecraftRconService {
    fn new(config: &MinecraftConfig, ui: UiHandle) -> Result<Self, String> {
        Ok(Self {
            ui,
            password: config.rcon_password.clone(),
            fill_max_blocks: config.fill_max_blocks,
            dimension_y_limits: config.dimension_y_limits.clone(),
            runtime: Mutex::new(RconRuntime {
                host: config.rcon_host.clone(),
                port: config.rcon_port,
                client: None,
            }),
            coord_block_re: Regex::new(r"\[([^\]]+)\]").unwrap(),
            nbt_pos_re: Regex::new(r#"Pos:\s*\[([^\]]+)\]"#).unwrap(),
            float_re: Regex::new(r"-?\d+(?:\.\d+)?").unwrap(),
            dimension_re: Regex::new(r"(minecraft:[a-z0-9_./-]+)").unwrap(),
            nbt_dimension_re: Regex::new(r#"Dimension:\s*"(minecraft:[a-z0-9_./-]+)""#).unwrap(),
            player_re: Regex::new(r"^[A-Za-z0-9_]{1,16}$").unwrap(),
            block_re: Regex::new(r"^minecraft:[a-z0-9_./-]+$").unwrap(),
        })
    }

    fn close(&self) {
        if let Ok(mut guard) = self.runtime.lock() {
            guard.client = None;
        }
    }

    fn update_endpoint(&self, host: String, port: u16) {
        if let Ok(mut guard) = self.runtime.lock() {
            guard.host = host;
            guard.port = port;
            guard.client = None;
        }
        ui_set_rcon(&self.ui, false);
    }

    fn run_command(&self, command: &str) -> Result<String, RconError> {
        let mut last_err: Option<String> = None;
        let mut guard = self
            .runtime
            .lock()
            .map_err(|_| RconError("RCON mutex poisoned".into()))?;
        for _ in 0..2 {
            if guard.client.is_none() {
                match MinecraftRconClient::connect(&guard.host, guard.port, &self.password) {
                    Ok(client) => guard.client = Some(client),
                    Err(e) => {
                        last_err = Some(e);
                        guard.client = None;
                        continue;
                    }
                }
            }

            if let Some(client) = guard.client.as_mut() {
                match client.cmd(command) {
                    Ok(resp) => {
                        ui_set_rcon(&self.ui, true);
                        return Ok(resp);
                    }
                    Err(e) => {
                        ui_set_rcon(&self.ui, false);
                        last_err = Some(e);
                        guard.client = None;
                    }
                }
            }
        }
        ui_set_rcon(&self.ui, false);
        Err(RconError(format!(
            "Ошибка при вводе команды на RCON `{command}`: {}",
            last_err.unwrap_or_else(|| "unknown".to_string())
        )))
    }

    fn validate_player_name(&self, player_name: &str) -> Result<String, String> {
        if self.player_re.is_match(player_name) {
            Ok(player_name.to_string())
        } else {
            Err(format!(
                "Неверный никнейм игрока `{player_name}`. Только буквы, числа, _."
            ))
        }
    }

    fn validate_block_id(&self, block_id: &str) -> Result<String, String> {
        if self.block_re.is_match(block_id) {
            Ok(block_id.to_string())
        } else {
            Err(format!(
                "Неверный блок `{block_id}`. Ожидаемый формат minecraft:dirt."
            ))
        }
    }

    fn get_player_pos(&self, player_name: &str) -> Result<(f64, f64, f64), PlayerLookupError> {
        let safe_name = self
            .validate_player_name(player_name)
            .map_err(PlayerLookupError)?;
        let response = self
            .run_command(&format!("data get entity {safe_name} Pos"))
            .map_err(|e| PlayerLookupError(e.to_string()))?;
        if let Some(pos) = self.try_parse_pos_from_response(&response) {
            return Ok(pos);
        }
        if is_rcon_error_like(&response) {
            ui_log(&self.ui, format!("[rcon-debug] Pos response: {}", response));
        }

        if let Some(pos) = self.try_get_player_pos_by_indices(&safe_name) {
            return Ok(pos);
        }

        // Fallback: some servers/plugins mangle `... Pos` replies, but full NBT can still be parsed.
        let fallback_response = self
            .run_command(&format!("data get entity {safe_name}"))
            .map_err(|e| PlayerLookupError(e.to_string()))?;
        if let Some(pos) = self.try_parse_pos_from_nbt_response(&fallback_response) {
            return Ok(pos);
        }
        if is_rcon_error_like(&fallback_response) {
            ui_log(
                &self.ui,
                format!("[rcon-debug] Full NBT response: {}", fallback_response),
            );
        }

        Err(PlayerLookupError(format!(
            "Cannot parse 3 coordinates for `{player_name}`. Response: {response}"
        )))
    }

    fn try_get_player_pos_by_indices(&self, safe_name: &str) -> Option<(f64, f64, f64)> {
        let mut coords = [0.0_f64; 3];
        for i in 0..3 {
            let cmd = format!("data get entity {safe_name} Pos[{i}]");
            let response = self.run_command(&cmd).ok()?;
            if is_rcon_error_like(&response) {
                ui_log(&self.ui, format!("[rcon-debug] Pos[{i}] response: {}", response));
            }

            let value = self
                .float_re
                .find_iter(&response)
                .filter_map(|m| m.as_str().parse::<f64>().ok())
                .next()?;
            coords[i] = value;
        }
        Some((coords[0], coords[1], coords[2]))
    }

    fn get_player_dimension(&self, player_name: &str) -> Result<String, PlayerLookupError> {
        let safe_name = self
            .validate_player_name(player_name)
            .map_err(PlayerLookupError)?;
        let response = self
            .run_command(&format!("data get entity {safe_name} Dimension"))
            .map_err(|e| PlayerLookupError(e.to_string()))?;
        if let Some(caps) = self.dimension_re.captures(&response) {
            return Ok(caps.get(1).unwrap().as_str().to_string());
        }
        if is_rcon_error_like(&response) {
            ui_log(&self.ui, format!("[rcon-debug] Dimension response: {}", response));
        }

        let fallback_response = self
            .run_command(&format!("data get entity {safe_name}"))
            .map_err(|e| PlayerLookupError(e.to_string()))?;
        if let Some(caps) = self.nbt_dimension_re.captures(&fallback_response) {
            return Ok(caps.get(1).unwrap().as_str().to_string());
        }
        if is_rcon_error_like(&fallback_response) {
            ui_log(
                &self.ui,
                format!(
                    "[rcon-debug] Full NBT response (dimension fallback): {}",
                    fallback_response
                ),
            );
        }

        Err(PlayerLookupError(format!(
            "Cannot parse player dimension for `{player_name}`. Response: {response}"
        )))
    }

    fn try_parse_pos_from_response(&self, response: &str) -> Option<(f64, f64, f64)> {
        let caps = self.coord_block_re.captures(response)?;
        let part = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        self.try_parse_pos_triplet(part)
    }

    fn try_parse_pos_from_nbt_response(&self, response: &str) -> Option<(f64, f64, f64)> {
        let caps = self.nbt_pos_re.captures(response)?;
        let part = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        self.try_parse_pos_triplet(part)
    }

    fn try_parse_pos_triplet(&self, text: &str) -> Option<(f64, f64, f64)> {
        let vals: Vec<f64> = self
            .float_re
            .find_iter(text)
            .filter_map(|m| m.as_str().parse::<f64>().ok())
            .collect();
        if vals.len() < 3 {
            return None;
        }
        Some((vals[0], vals[1], vals[2]))
    }

    fn resolve_y_limits(&self, dimension: &str) -> (i32, i32) {
        self.dimension_y_limits
            .get(dimension)
            .copied()
            .or_else(|| self.dimension_y_limits.get("minecraft:overworld").copied())
            .unwrap_or((-64, 319))
    }

    fn build_vertical_segments(&self, y_min: i32, y_max: i32) -> Vec<(i32, i32)> {
        let area = 16usize * 16usize;
        let max_height = (self.fill_max_blocks / area).max(1) as i32;
        let mut segments = Vec::new();
        let mut start = y_min;
        while start <= y_max {
            let end = y_max.min(start + max_height - 1);
            segments.push((start, end));
            start = end + 1;
        }
        segments
    }

    fn chunk_origin(value: f64) -> i32 {
        ((value.floor() as i32).div_euclid(16)) * 16
    }

    fn get_player_chunk_context(&self, player_name: &str) -> Result<PlayerChunkContext, Box<dyn std::error::Error>> {
        let safe_name = self.validate_player_name(player_name)?;
        let (x, _y, z) = self.get_player_pos(&safe_name)?;
        let dimension = self.get_player_dimension(&safe_name)?;

        let chunk_x_origin = Self::chunk_origin(x);
        let chunk_z_origin = Self::chunk_origin(z);
        let chunk_x = chunk_x_origin / 16;
        let chunk_z = chunk_z_origin / 16;
        let (y_min, y_max) = self.resolve_y_limits(&dimension);
        let segments = self.build_vertical_segments(y_min, y_max);

        Ok(PlayerChunkContext {
            player_name: safe_name,
            dimension,
            chunk_x,
            chunk_z,
            x1: chunk_x_origin,
            x2: chunk_x_origin + 15,
            z1: chunk_z_origin,
            z2: chunk_z_origin + 15,
            segments,
        })
    }

    fn delete_block_in_chunk_context(
        &self,
        context: &PlayerChunkContext,
        block_id: &str,
    ) -> Result<ChunkDeleteResult, Box<dyn std::error::Error>> {
        let safe_block = self.validate_block_id(block_id)?;
        let mut commands_sent = 0usize;
        for (seg_y_min, seg_y_max) in &context.segments {
            let command = format!(
                "execute in {} run fill {} {} {} {} {} {} air replace {}",
                context.dimension,
                context.x1,
                seg_y_min,
                context.z1,
                context.x2,
                seg_y_max,
                context.z2,
                safe_block
            );
            self.run_command(&command)?;
            commands_sent += 1;
        }
        Ok(ChunkDeleteResult {
            player_name: context.player_name.clone(),
            block_id: safe_block,
            dimension: context.dimension.clone(),
            chunk_x: context.chunk_x,
            chunk_z: context.chunk_z,
            commands_sent,
        })
    }

    fn send_private_message(&self, player_name: &str, message: &str) -> Result<(), Box<dyn std::error::Error>> {
        let safe_name = self.validate_player_name(player_name)?;
        let safe_message = message.replace('\n', " ");
        let _ = self.run_command(&format!("tell {safe_name} {safe_message}"))?;
        Ok(())
    }
}

pub(crate) struct BlockDeleteController {
    config: AppConfig,
    config_path: PathBuf,
    config_dir: PathBuf,
    catalog: BlockCatalog,
    rcon: Arc<MinecraftRconService>,
    ui: UiHandle,
}

impl BlockDeleteController {
    pub(crate) fn new(config: AppConfig, config_path: PathBuf, config_dir: PathBuf, ui: UiHandle) -> Result<Self, String> {
        let blocks_path = resolve_path(&config_dir, &config.blocks.file);
        let catalog = BlockCatalog::load(
            &blocks_path,
            &config.blocks.extra_aliases,
            &config.blocks.shared_aliases,
        )?;
        let rcon = Arc::new(MinecraftRconService::new(&config.minecraft, Arc::clone(&ui))?);
        Ok(Self {
            config,
            config_path,
            config_dir,
            catalog,
            rcon,
            ui,
        })
    }

    fn validate_runtime_config(&self) -> Result<(), String> {
        if self.config.minecraft.rcon_password.is_empty() {
            return Err("minecraft.rcon_password пустой в config.json".to_string());
        }
        if !self.config.microphone.enabled {
            return Err("microphone.enabled=false, включи микрофон в config.json".to_string());
        }
        if self.config.microphone.samplerate != self.config.speech.sample_rate {
            return Err(
                "microphone.samplerate должен совпадать с speech.sample_rate (рекомендую 48000)"
                    .to_string(),
            );
        }
        if self.config.microphone.player_name.trim().is_empty() {
            return Err(
                "microphone.player_name пустой, но microphone.enabled=true. Укажи никнейм."
                    .to_string(),
            );
        }
        Ok(())
    }

    pub(crate) fn run(&self) -> Result<(), String> {
        self.validate_runtime_config()?;

        let grammar_phrases = if self.config.speech.use_grammar {
            let mut phrases: Vec<String> = self
                .config
                .blocks
                .custom_alias_phrases()
                .into_iter()
                .map(|s| normalize_text(&s))
                .filter(|s| !s.is_empty())
                .collect();
            if phrases.is_empty() {
                phrases = self.catalog.aliases();
            }
            phrases.sort();
            phrases.dedup();
            Some(phrases)
        } else {
            None
        };

        ui_log(
            &self.ui,
            format!(
                "[startup] блоков={}, microphone_enabled={}, fuzzy_threshold={}",
                self.catalog.alias_count(),
                self.config.microphone.enabled,
                self.config.speech.fuzzy_threshold
            ),
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        {
            let shutdown = Arc::clone(&shutdown);
            ctrlc::set_handler(move || {
                shutdown.store(true, Ordering::SeqCst);
            })
            .map_err(|e| format!("Не удалось установить Ctrl+C handler: {e}"))?;
        }

        let (pcm_tx, pcm_rx) = bounded::<Vec<i16>>(512);
        let (text_tx, text_rx) = bounded::<RecognizedPhraseEvent>(512);
        let recognizer_handle = spawn_recognizer_worker(
            resolve_path(&self.config_dir, &self.config.speech.model_path),
            self.config.speech.sample_rate,
            self.config.speech.log_partials,
            grammar_phrases,
            Arc::clone(&self.ui),
            Arc::clone(&shutdown),
            pcm_rx,
            text_tx,
        );

        let mut microphone = MicrophoneSource::start(
            self.config.microphone.samplerate,
            self.config.microphone.blocksize,
            &self.config.microphone.device,
            Arc::clone(&self.ui),
            {
                let pcm_tx = pcm_tx.clone();
                move |pcm: Vec<i16>| {
                    let _ = pcm_tx.try_send(pcm);
                }
            },
        )?;

        let event_worker = self.spawn_event_worker(Arc::clone(&shutdown), text_rx);
        let presence_worker = self.spawn_presence_watcher(Arc::clone(&shutdown));

        let mut tui = TuiGuard::enter()?;
        let mut controls = TuiControls {
            selected: FooterButton::Settings,
            settings_open: false,
            settings_field: SettingsField::Host,
            settings_editing: false,
            settings_tab: SettingsTab::Connection,
        };
        let mut settings_draft = SettingsDraft {
            host: self.config.minecraft.rcon_host.clone(),
            port: self.config.minecraft.rcon_port.to_string(),
            password: self.config.minecraft.rcon_password.clone(),
            player_name: self.config.microphone.player_name.clone(),
            ui_mode: self.config.ui.mode.unwrap_or(UiMode::Tui),
        };
        let mut restart_after_tui_exit = false;
        ui_log(&self.ui, "[ui] q - выйти");

        while !shutdown.load(Ordering::Relaxed) {
            tui.draw(&self.ui, &controls, &settings_draft)?;
            if event::poll(Duration::from_millis(100)).map_err(|e| format!("event poll error: {e}"))? {
                if let CEvent::Key(key) =
                    event::read().map_err(|e| format!("event read error: {e}"))?
                {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            shutdown.store(true, Ordering::SeqCst);
                        }
                        KeyCode::Left => {
                            if controls.settings_open && !controls.settings_editing {
                                if controls.settings_field == SettingsField::UiMode {
                                    settings_draft.ui_mode = match settings_draft.ui_mode {
                                        UiMode::Tui => UiMode::Qt,
                                        UiMode::Qt => UiMode::Tui,
                                    };
                                } else {
                                    controls.settings_tab = controls.settings_tab.prev();
                                    controls.settings_field = default_field_for_tab(controls.settings_tab);
                                }
                            } else if !controls.settings_open {
                                controls.selected = controls.selected.prev();
                            }
                        }
                        KeyCode::Right => {
                            if controls.settings_open && !controls.settings_editing {
                                if controls.settings_field == SettingsField::UiMode {
                                    settings_draft.ui_mode = match settings_draft.ui_mode {
                                        UiMode::Tui => UiMode::Qt,
                                        UiMode::Qt => UiMode::Tui,
                                    };
                                } else {
                                    controls.settings_tab = controls.settings_tab.next();
                                    controls.settings_field = default_field_for_tab(controls.settings_tab);
                                }
                            } else if !controls.settings_open {
                                controls.selected = controls.selected.next();
                            }
                        }
                        KeyCode::Up => {
                            if controls.settings_open && !controls.settings_editing {
                                controls.settings_field =
                                    settings_field_prev_in_tab(controls.settings_field, controls.settings_tab);
                            }
                        }
                        KeyCode::Down => {
                            if controls.settings_open && !controls.settings_editing {
                                controls.settings_field =
                                    settings_field_next_in_tab(controls.settings_field, controls.settings_tab);
                            }
                        }
                        KeyCode::Esc => {
                            if controls.settings_editing {
                                controls.settings_editing = false;
                            } else {
                                controls.settings_open = false;
                            }
                        }
                        KeyCode::Enter => {
                            if controls.settings_open {
                                if controls.settings_field == SettingsField::UiMode {
                                    settings_draft.ui_mode = match settings_draft.ui_mode {
                                        UiMode::Tui => UiMode::Qt,
                                        UiMode::Qt => UiMode::Tui,
                                    };
                                } else {
                                    controls.settings_editing = !controls.settings_editing;
                                }
                            } else {
                                match controls.selected {
                                    FooterButton::Settings => {
                                        let snap = ui_snapshot(&self.ui);
                                        settings_draft.host = snap.rcon_host;
                                        settings_draft.port = snap.rcon_port.to_string();
                                        settings_draft.password = snap.rcon_password;
                                        settings_draft.player_name = snap.player_name;
                                        settings_draft.ui_mode = snap.ui_mode;
                                        controls.settings_tab = SettingsTab::Connection;
                                        controls.settings_field = SettingsField::Host;
                                        controls.settings_editing = false;
                                        controls.settings_open = true;
                                    }
                                    FooterButton::Exit => shutdown.store(true, Ordering::SeqCst),
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            if controls.settings_open && controls.settings_editing {
                                match controls.settings_field {
                                    SettingsField::Host => {
                                        settings_draft.host.pop();
                                    }
                                    SettingsField::Port => {
                                        settings_draft.port.pop();
                                    }
                                    SettingsField::Password => {
                                        settings_draft.password.pop();
                                    }
                                    SettingsField::PlayerName => {
                                        settings_draft.player_name.pop();
                                    }
                                    SettingsField::UiMode => {}
                                }
                            }
                        }
                        KeyCode::Char('s') | KeyCode::Char('S') => {
                            if controls.settings_open && !controls.settings_editing {
                                let host = settings_draft.host.trim().to_string();
                                let port = settings_draft
                                    .port
                                    .trim()
                                    .parse::<u16>()
                                    .map_err(|_| "Порт должен быть числом 1..65535".to_string())?;

                                if host.is_empty() {
                                    ui_log(&self.ui, "[settings-error] IP/host пустой");
                                } else {
                                    match self.save_settings_bundle(
                                        host.clone(),
                                        port,
                                        settings_draft.password.clone(),
                                        settings_draft.player_name.clone(),
                                        settings_draft.ui_mode,
                                    ) {
                                        Ok(outcome) => {
                                            controls.settings_open = false;
                                            if outcome.restart_required {
                                                restart_after_tui_exit = true;
                                                shutdown.store(true, Ordering::SeqCst);
                                            }
                                        }
                                        Err(err) => ui_log(&self.ui, format!("[settings-error] {err}")),
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if controls.settings_open && controls.settings_editing {
                                match controls.settings_field {
                                    SettingsField::Host => {
                                        if !c.is_control() {
                                            settings_draft.host.push(c);
                                        }
                                    }
                                    SettingsField::Port => {
                                        if c.is_ascii_digit() {
                                            settings_draft.port.push(c);
                                        }
                                    }
                                    SettingsField::Password => {
                                        if !c.is_control() {
                                            settings_draft.password.push(c);
                                        }
                                    }
                                    SettingsField::PlayerName => {
                                        if c.is_ascii_alphanumeric() || c == '_' {
                                            settings_draft.player_name.push(c);
                                        }
                                    }
                                    SettingsField::UiMode => {
                                        if matches!(c, 't' | 'T' | 'q' | 'Q') {
                                            settings_draft.ui_mode = UiMode::Tui;
                                        } else if matches!(c, 'g' | 'G') {
                                            // ignore accidental russian layout noise
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        drop(pcm_tx);
        microphone.stop();
        ui_set_mic(&self.ui, false);
        self.rcon.close();
        let _ = recognizer_handle.join();
        drop(event_worker);
        drop(presence_worker);
        drop(tui);
        if restart_after_tui_exit {
            restart_current_process()?;
        }
        Ok(())
    }

    pub(crate) fn run_headless_with_shutdown(
        &self,
        shutdown: Arc<AtomicBool>,
    ) -> Result<(), String> {
        self.validate_runtime_config()?;

        let grammar_phrases = if self.config.speech.use_grammar {
            let mut phrases: Vec<String> = self
                .config
                .blocks
                .custom_alias_phrases()
                .into_iter()
                .map(|s| normalize_text(&s))
                .filter(|s| !s.is_empty())
                .collect();
            if phrases.is_empty() {
                phrases = self.catalog.aliases();
            }
            phrases.sort();
            phrases.dedup();
            Some(phrases)
        } else {
            None
        };

        ui_log(
            &self.ui,
            format!(
                "[startup] блоков={}, microphone_enabled={}, fuzzy_threshold={}",
                self.catalog.alias_count(),
                self.config.microphone.enabled,
                self.config.speech.fuzzy_threshold
            ),
        );

        let (pcm_tx, pcm_rx) = bounded::<Vec<i16>>(512);
        let (text_tx, text_rx) = bounded::<RecognizedPhraseEvent>(512);
        let recognizer_handle = spawn_recognizer_worker(
            resolve_path(&self.config_dir, &self.config.speech.model_path),
            self.config.speech.sample_rate,
            self.config.speech.log_partials,
            grammar_phrases,
            Arc::clone(&self.ui),
            Arc::clone(&shutdown),
            pcm_rx,
            text_tx,
        );

        let mut microphone = MicrophoneSource::start(
            self.config.microphone.samplerate,
            self.config.microphone.blocksize,
            &self.config.microphone.device,
            Arc::clone(&self.ui),
            {
                let pcm_tx = pcm_tx.clone();
                move |pcm: Vec<i16>| {
                    let _ = pcm_tx.try_send(pcm);
                }
            },
        )?;

        let event_worker = self.spawn_event_worker(Arc::clone(&shutdown), text_rx);
        let presence_worker = self.spawn_presence_watcher(Arc::clone(&shutdown));

        while !shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }

        drop(pcm_tx);
        microphone.stop();
        ui_set_mic(&self.ui, false);
        self.rcon.close();
        let _ = recognizer_handle.join();
        drop(event_worker);
        drop(presence_worker);
        Ok(())
    }

    pub(crate) fn save_settings_bundle(
        &self,
        host: String,
        port: u16,
        rcon_password: String,
        player_name: String,
        ui_mode: UiMode,
    ) -> Result<SaveSettingsOutcome, String> {
        let host = host.trim().to_string();
        let rcon_password = rcon_password.trim().to_string();
        let player_name = player_name.trim().to_string();
        if host.is_empty() {
            return Err("IP/host пустой".to_string());
        }
        if rcon_password.is_empty() {
            return Err("RCON password пустой".to_string());
        }
        if player_name.is_empty() {
            return Err("Username/player_name пустой".to_string());
        }

        let old_player_name = self.config.microphone.player_name.trim().to_string();
        let old_ui_mode = self.config.ui.mode.unwrap_or(UiMode::Tui);
        let old_rcon_password = self.config.minecraft.rcon_password.trim().to_string();
        let restart_required = old_player_name != player_name
            || old_ui_mode != ui_mode
            || old_rcon_password != rcon_password;

        save_rcon_settings_to_config(&self.config_path, &host, port)?;
        save_rcon_password_to_config(&self.config_path, &rcon_password)?;
        save_player_name_to_config(&self.config_path, &player_name)?;
        save_ui_mode_to_config(&self.config_path, ui_mode)?;

        if let Ok(mut ui) = self.ui.lock() {
            ui.rcon_host = host.clone();
            ui.rcon_port = port;
            ui.rcon_password = rcon_password.clone();
            ui.player_name = player_name.clone();
            ui.ui_mode = ui_mode;
        }

        self.rcon.update_endpoint(host.clone(), port);
        ui_log(
            &self.ui,
            format!(
                "[settings] сохранено: {}:{}, user={}, ui_mode={}",
                host,
                port,
                player_name,
                ui_mode.as_config_str()
            ),
        );
        if restart_required {
            ui_log(
                &self.ui,
                "[settings] ui_mode/player_name/rcon_password изменены, выполняю автоперезапуск...",
            );
        }
        Ok(SaveSettingsOutcome { restart_required })
    }

    fn spawn_event_worker(
        &self,
        shutdown: Arc<AtomicBool>,
        text_rx: Receiver<RecognizedPhraseEvent>,
    ) -> thread::JoinHandle<()> {
        let player_name = self.config.microphone.player_name.clone();
        let min_phrase_chars = self.config.speech.min_phrase_chars;
        let log_recognized = self.config.speech.log_recognized;
        let fuzzy_threshold = self.config.speech.fuzzy_threshold;
        let cooldown_seconds = self.config.speech.cooldown_seconds;
        let catalog = self.catalog.clone();
        let rcon = Arc::clone(&self.rcon);
        let ui = Arc::clone(&self.ui);

        thread::spawn(move || {
            let mut last_trigger: BTreeMap<(String, String), Instant> = BTreeMap::new();
            let mut repeat_gate: HashMap<(String, String), RepeatGateState> = HashMap::new();
            let repeat_window = Duration::from_secs(1);
            let mut partial_progress: HashMap<String, PartialProgressState> = HashMap::new();
            let mut cached_chunk: Option<CachedChunkContext> = None;
            let chunk_cache_ttl = Duration::from_millis(700);

            loop {
                match text_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(event) => {
                        let cleaned = normalize_text(&event.text);
                        if cleaned.chars().count() < min_phrase_chars {
                            continue;
                        }
                        if log_recognized {
                            ui_log(&ui, format!("[recognized:{}] {}", event.speaker_id, cleaned));
                        }

                        if event.speaker_id != MIC_SPEAKER_ID {
                            ui_log(&ui, format!("[mapping-warning] нет никнейма для {}", event.speaker_id));
                            continue;
                        }

                        let candidates: Vec<String> = if event.is_partial {
                            let st = partial_progress
                                .entry(event.speaker_id.clone())
                                .or_default();

                            let current_words: Vec<&str> = cleaned.split_whitespace().collect();
                            let committed_count = current_words.len().saturating_sub(1);

                            // Если partial "откатился"/пересобрался, начинаем индекс заново.
                            if !cleaned.starts_with(&st.last_partial) {
                                st.processed_committed_words = 0;
                            }

                            let start_idx = st.processed_committed_words.min(committed_count);
                            let mut out = Vec::new();
                            for word in current_words.iter().skip(start_idx).take(committed_count.saturating_sub(start_idx)) {
                                if word.len() >= 2 {
                                    out.push((*word).to_string());
                                }
                            }
                            st.last_partial = cleaned.clone();
                            st.processed_committed_words = committed_count;
                            out
                        } else {
                            partial_progress.remove(&event.speaker_id);
                            vec![cleaned.clone()]
                        };

                        let mut block_ids: Vec<String> = Vec::new();
                        let mut seen_blocks = HashSet::new();

                        for candidate in candidates {
                            let key = (event.speaker_id.clone(), candidate.clone());
                            let now = Instant::now();
                            let state = repeat_gate.entry(key).or_insert(RepeatGateState {
                                last_seen: now,
                                count: 0,
                            });
                            if now.duration_since(state.last_seen) > repeat_window {
                                state.count = 0;
                            }
                            state.last_seen = now;
                            state.count += 1;

                            // 1, 9, 17, ... => пропускаем 7 из каждых 8 одинаковых повторов за секунду
                            if (state.count - 1) % 8 != 0 {
                                continue;
                            }

                            for block_id in catalog.match_blocks(&candidate, fuzzy_threshold) {
                                if seen_blocks.insert(block_id.clone()) {
                                    block_ids.push(block_id);
                                }
                            }
                        }

                        if block_ids.is_empty() {
                            continue;
                        }

                        let chunk_context = if let Some(cache) = &cached_chunk {
                            if cache.context.player_name == player_name
                                && cache.fetched_at.elapsed() <= chunk_cache_ttl
                            {
                                cache.context.clone()
                            } else {
                                match rcon.get_player_chunk_context(&player_name) {
                                    Ok(ctx) => {
                                        cached_chunk = Some(CachedChunkContext {
                                            fetched_at: Instant::now(),
                                            context: ctx.clone(),
                                        });
                                        ctx
                                    }
                                    Err(err) => {
                                        cached_chunk = None;
                                        if err.downcast_ref::<PlayerLookupError>().is_some() {
                                            ui_set_player_online(&ui, false);
                                            ui_log(&ui, format!("[rcon-player-error] {err}"));
                                        } else {
                                            ui_log(&ui, format!("[rcon-error] {err}"));
                                        }
                                        continue;
                                    }
                                }
                            }
                        } else {
                            match rcon.get_player_chunk_context(&player_name) {
                                Ok(ctx) => {
                                    cached_chunk = Some(CachedChunkContext {
                                        fetched_at: Instant::now(),
                                        context: ctx.clone(),
                                    });
                                    ctx
                                }
                                Err(err) => {
                                    if err.downcast_ref::<PlayerLookupError>().is_some() {
                                        ui_set_player_online(&ui, false);
                                        ui_log(&ui, format!("[rcon-player-error] {err}"));
                                    } else {
                                        ui_log(&ui, format!("[rcon-error] {err}"));
                                    }
                                    continue;
                                }
                            }
                        };

                        for block_id in block_ids {
                            let key = (player_name.clone(), block_id.clone());
                            let now = Instant::now();
                            if let Some(prev) = last_trigger.get(&key) {
                                if now.duration_since(*prev).as_secs_f64() < cooldown_seconds {
                                    continue;
                                }
                            }
                            last_trigger.insert(key, now);

                            match rcon.delete_block_in_chunk_context(&chunk_context, &block_id) {
                                Ok(result) => {
                                    ui_set_player_online(&ui, true);
                                    ui_log(
                                        &ui,
                                        format!(
                                        "[trigger] speaker=Microphone -> player={}, block={}, dimension={}, chunk=({},{}), fill_commands={}",
                                        result.player_name,
                                        result.block_id,
                                        result.dimension,
                                        result.chunk_x,
                                        result.chunk_z,
                                        result.commands_sent
                                    ),
                                    );
                                }
                                Err(err) => {
                                    if err.downcast_ref::<PlayerLookupError>().is_some() {
                                        ui_set_player_online(&ui, false);
                                        ui_log(&ui, format!("[rcon-player-error] {err}"));
                                    } else {
                                        ui_log(&ui, format!("[rcon-error] {err}"));
                                    }
                                }
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
    }

    fn spawn_presence_watcher(&self, shutdown: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        let player_name = self.config.microphone.player_name.clone();
        let rcon = Arc::clone(&self.rcon);
        let ui = Arc::clone(&self.ui);

        thread::spawn(move || {
            let mut was_online = false;
            while !shutdown.load(Ordering::Relaxed) {
                match rcon.get_player_chunk_context(&player_name) {
                    Ok(_) => {
                        ui_set_player_online(&ui, true);
                        if !was_online {
                            was_online = true;
                            ui_log(&ui, format!("[player] {} зашел на сервер", player_name));
                            match rcon.send_private_message(
                                &player_name,
                                "[BlockDelete] все успешно работает",
                            ) {
                                Ok(()) => ui_log(&ui, "[notify] отправлено личное сообщение игроку"),
                                Err(err) => ui_log(&ui, format!("[notify-error] {err}")),
                            }
                        }
                    }
                    Err(err) => {
                        if err.downcast_ref::<PlayerLookupError>().is_some() {
                            if was_online {
                                ui_log(&ui, format!("[player] {} вышел с сервера", player_name));
                            }
                            was_online = false;
                            ui_set_player_online(&ui, false);
                        } else {
                            ui_set_player_online(&ui, false);
                        }
                    }
                }
                thread::sleep(Duration::from_secs(2));
            }
        })
    }
}

fn resolve_path(config_dir: &Path, value: &str) -> PathBuf {
    let p = PathBuf::from(value);
    if p.is_absolute() {
        p
    } else {
        config_dir.join(p)
    }
}

fn main() {
    if let Err(err) = real_main() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let args = Args::parse();

    if args.list_audio_devices {
        for line in list_input_devices()? {
            println!("{line}");
        }
        return Ok(());
    }

    let bootstrap = backend_bootstrap::BackendBootstrap::from_config_path(&args.config)?;
    let mut config = bootstrap.config.clone();
    let ui_mode = match config.ui.mode {
        Some(mode) => mode,
        None => {
            let selected = choose_ui_mode_tui()?;
            save_ui_mode_to_config(&args.config, selected)?;
            config.ui.mode = Some(selected);
            selected
        }
    };

    match ui_mode {
        UiMode::Tui => {
            let bootstrap = backend_bootstrap::BackendBootstrap {
                config,
                config_path: bootstrap.config_path,
                config_dir: bootstrap.config_dir,
            };
            ui_tui::run_tui_mode(bootstrap)
        }
        UiMode::Qt => {
            let bootstrap = backend_bootstrap::BackendBootstrap {
                config,
                config_path: bootstrap.config_path,
                config_dir: bootstrap.config_dir,
            };
            ui_qt::run_qt_mode(bootstrap)
        }
    }
}
