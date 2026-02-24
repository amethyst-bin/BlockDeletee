use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

use qmetaobject::prelude::*;

use crate::backend_bootstrap::BackendBootstrap;
use crate::{
    restart_current_process, ui_log, ui_snapshot, BlockDeleteController, UiHandle, UiMode, UiState,
};

static QT_CTX: OnceLock<QtFrontendContext> = OnceLock::new();

struct QtFrontendContext {
    ui: UiHandle,
    controller: Arc<BlockDeleteController>,
    shutdown: Arc<AtomicBool>,
}

fn qt_ctx() -> &'static QtFrontendContext {
    QT_CTX.get().expect("qt context not initialized")
}

#[derive(QObject, Default)]
struct QtBackendBridge {
    base: qt_base_class!(trait QObject),

    mic_ok: qt_property!(bool; NOTIFY mic_ok_changed),
    mic_ok_changed: qt_signal!(),
    rec_ok: qt_property!(bool; NOTIFY rec_ok_changed),
    rec_ok_changed: qt_signal!(),
    rcon_ok: qt_property!(bool; NOTIFY rcon_ok_changed),
    rcon_ok_changed: qt_signal!(),
    player_online: qt_property!(bool; NOTIFY player_online_changed),
    player_online_changed: qt_signal!(),

    player_name: qt_property!(QString; NOTIFY player_name_changed),
    player_name_changed: qt_signal!(),
    logs_text: qt_property!(QString; NOTIFY logs_text_changed),
    logs_text_changed: qt_signal!(),
    logs_html: qt_property!(QString; NOTIFY logs_html_changed),
    logs_html_changed: qt_signal!(),
    logs_border_color: qt_property!(QString; NOTIFY logs_border_color_changed),
    logs_border_color_changed: qt_signal!(),

    settings_open: qt_property!(bool; NOTIFY settings_open_changed),
    settings_open_changed: qt_signal!(),
    settings_host: qt_property!(QString; NOTIFY settings_host_changed),
    settings_host_changed: qt_signal!(),
    settings_port: qt_property!(QString; NOTIFY settings_port_changed),
    settings_port_changed: qt_signal!(),
    settings_player_name: qt_property!(QString; NOTIFY settings_player_name_changed),
    settings_player_name_changed: qt_signal!(),
    settings_ui_mode: qt_property!(QString; NOTIFY settings_ui_mode_changed),
    settings_ui_mode_changed: qt_signal!(),

    tick: qt_method!(fn tick(&mut self) {
        let snap = ui_snapshot(&qt_ctx().ui);
        if self.mic_ok != snap.mic_ok {
            self.mic_ok = snap.mic_ok;
            self.mic_ok_changed();
        }
        if self.rec_ok != snap.rec_ok {
            self.rec_ok = snap.rec_ok;
            self.rec_ok_changed();
        }
        if self.rcon_ok != snap.rcon_ok {
            self.rcon_ok = snap.rcon_ok;
            self.rcon_ok_changed();
        }
        if self.player_online != snap.player_online {
            self.player_online = snap.player_online;
            self.player_online_changed();
        }

        let player_name: QString = snap.player_name.into();
        if self.player_name != player_name {
            self.player_name = player_name;
            self.player_name_changed();
        }

        let logs = snap.logs.join("\n");
        let logs_qs: QString = logs.into();
        if self.logs_text != logs_qs {
            self.logs_text = logs_qs;
            self.logs_text_changed();
        }

        let html = logs_to_html(&snap.logs);
        let html_qs: QString = html.into();
        if self.logs_html != html_qs {
            self.logs_html = html_qs;
            self.logs_html_changed();
        }

        let border = snap
            .logs
            .last()
            .map(|s| qt_log_color_hex(s))
            .unwrap_or("#3a404b")
            .to_string();
        let border_qs: QString = border.into();
        if self.logs_border_color != border_qs {
            self.logs_border_color = border_qs;
            self.logs_border_color_changed();
        }
    }),

    open_settings: qt_method!(fn open_settings(&mut self) {
        let snap = ui_snapshot(&qt_ctx().ui);
        self.settings_host = snap.rcon_host.into();
        self.settings_port = snap.rcon_port.to_string().into();
        self.settings_player_name = snap.player_name.into();
        self.settings_ui_mode = snap.ui_mode.as_config_str().to_string().into();
        self.settings_open = true;
        self.settings_host_changed();
        self.settings_port_changed();
        self.settings_player_name_changed();
        self.settings_ui_mode_changed();
        self.settings_open_changed();
    }),

    close_settings: qt_method!(fn close_settings(&mut self) {
        if self.settings_open {
            self.settings_open = false;
            self.settings_open_changed();
        }
    }),

    set_settings_host: qt_method!(fn set_settings_host(&mut self, value: QString) {
        self.settings_host = value;
        self.settings_host_changed();
    }),

    set_settings_port: qt_method!(fn set_settings_port(&mut self, value: QString) {
        self.settings_port = value;
        self.settings_port_changed();
    }),

    set_settings_player_name: qt_method!(fn set_settings_player_name(&mut self, value: QString) {
        self.settings_player_name = value;
        self.settings_player_name_changed();
    }),

    set_settings_ui_mode: qt_method!(fn set_settings_ui_mode(&mut self, value: QString) {
        self.settings_ui_mode = value;
        self.settings_ui_mode_changed();
    }),

    save_settings: qt_method!(fn save_settings(&mut self) {
        let host = self.settings_host.to_string();
        let port_text = self.settings_port.to_string();
        let player_name = self.settings_player_name.to_string();
        let ui_mode = match UiMode::from_config_str(&self.settings_ui_mode.to_string()) {
            Some(m) => m,
            None => {
                ui_log(&qt_ctx().ui, "[settings-error] ui_mode должен быть `tui` или `qt`");
                return;
            }
        };
        let port = match port_text.trim().parse::<u16>() {
            Ok(v) => v,
            Err(_) => {
                ui_log(&qt_ctx().ui, "[settings-error] Порт должен быть числом 1..65535");
                return;
            }
        };

        match qt_ctx().controller.save_settings_bundle(host, port, player_name, ui_mode) {
            Ok(outcome) => {
                self.settings_open = false;
                self.settings_open_changed();
                if outcome.restart_required {
                    match restart_current_process() {
                        Ok(()) => std::process::exit(0),
                        Err(err) => ui_log(&qt_ctx().ui, format!("[restart-error] {err}")),
                    }
                }
            }
            Err(err) => ui_log(&qt_ctx().ui, format!("[settings-error] {err}")),
        }
    }),

    request_exit: qt_method!(fn request_exit(&mut self) {
        qt_ctx().shutdown.store(true, Ordering::SeqCst);
    }),
}

pub(crate) fn run_qt_mode(bootstrap: BackendBootstrap) -> Result<(), String> {
    let ui: UiHandle = Arc::new(std::sync::Mutex::new(UiState::new(
        bootstrap.config.microphone.player_name.clone(),
        bootstrap.config.minecraft.rcon_host.clone(),
        bootstrap.config.minecraft.rcon_port,
        bootstrap.config.ui.mode.unwrap_or(crate::UiMode::Qt),
    )));
    let controller = Arc::new(bootstrap.build_controller(Arc::clone(&ui))?);
    let shutdown = Arc::new(AtomicBool::new(false));

    QT_CTX
        .set(QtFrontendContext {
            ui: Arc::clone(&ui),
            controller: Arc::clone(&controller),
            shutdown: Arc::clone(&shutdown),
        })
        .map_err(|_| "Qt context already initialized".to_string())?;

    let backend_thread = {
        let controller = Arc::clone(&controller);
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            if let Err(err) = controller.run_headless_with_shutdown(shutdown) {
                ui_log(&ui, format!("[backend-error] {err}"));
            }
        })
    };

    let mut engine = QmlEngine::new();
    let bridge = QObjectBox::new(QtBackendBridge::default());
    engine.set_object_property("backend".into(), bridge.pinned());
    engine.load_data(QML_MAIN.into());
    engine.exec();

    qt_ctx().shutdown.store(true, Ordering::SeqCst);
    let _ = backend_thread.join();
    Ok(())
}

fn logs_to_html(lines: &[String]) -> String {
    if lines.is_empty() {
        return "<span style=\"color:#9aa3b2;\">Ожидание событий...</span>".to_string();
    }
    lines.iter()
        .map(|line| {
            let color = qt_log_color_hex(line);
            format!(
                "<div style=\"color:{}; margin-bottom:4px;\">{}</div>",
                color,
                html_escape(line)
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn qt_log_color_hex(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    if lower.contains("error") || lower.contains("ошибка") {
        "#e25555"
    } else if lower.contains("warning") || lower.contains("warn") {
        "#f2c14e"
    } else if lower.contains("[trigger]") || lower.contains("[startup]") || lower.contains("[notify]") {
        "#26c281"
    } else if lower.contains("[player]") {
        "#49c6e5"
    } else if lower.contains("[recognized") {
        "#b565ff"
    } else if lower.contains("[partial") || lower.contains("[rcon-debug]") {
        "#7c8698"
    } else {
        "#d7dbe2"
    }
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const QML_MAIN: &str = r##"
import QtQuick 2.15
import QtQuick.Controls 2.15
import QtQuick.Layouts 1.15

ApplicationWindow {
    id: root
    visible: true
    width: 980
    height: 640
    minimumWidth: 720
    minimumHeight: 420
    title: "BlockDeletee (Qt)"
    color: "#101214"

    function badgeColor(ok) { return ok ? "#26c281" : "#e25555" }

    component FrameButton: Button {
        id: btn
        property color frameColor: "#3a404b"
        property color accentColor: "#f2c14e"
        implicitHeight: 38
        implicitWidth: 120
        font.bold: true
        background: Rectangle {
            radius: 10
            color: btn.pressed ? "#262b33" : (btn.hovered ? "#1f242c" : "#171a1f")
            border.width: 2
            border.color: btn.activeFocus ? btn.accentColor : btn.frameColor
        }
        contentItem: Label {
            text: btn.text
            horizontalAlignment: Text.AlignHCenter
            verticalAlignment: Text.AlignVCenter
            color: "white"
            font.bold: true
        }
    }

    Timer {
        interval: 120
        running: true
        repeat: true
        onTriggered: backend.tick()
    }

    onClosing: {
        backend.request_exit()
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.margins: 14
        spacing: 12

        Rectangle {
            Layout.fillWidth: true
            Layout.preferredHeight: 120
            radius: 14
            color: "#171a1f"
            border.width: 2
            border.color: (backend.mic_ok && backend.rec_ok && backend.rcon_ok && backend.player_online) ? "#26c281" : "#e25555"

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 12
                spacing: 8

                Label {
                    text: "BlockDeletee"
                    color: "white"
                    font.pixelSize: 22
                    font.bold: true
                }

                RowLayout {
                    spacing: 18
                    Label { text: "󰍹 MIC " + (backend.mic_ok ? "●" : "●" + " !"); color: root.badgeColor(backend.mic_ok); font.bold: !backend.mic_ok }
                    Label { text: "󰋎 REC " + (backend.rec_ok ? "●" : "●" + " !"); color: root.badgeColor(backend.rec_ok); font.bold: !backend.rec_ok }
                    Label { text: "󰒓 RCON " + (backend.rcon_ok ? "●" : "●" + " !"); color: root.badgeColor(backend.rcon_ok); font.bold: !backend.rcon_ok }
                    Label { text: "󰀄 PLAYER " + (backend.player_online ? "●" : "●" + " !"); color: root.badgeColor(backend.player_online); font.bold: !backend.player_online }
                }

                Label {
                    text: "Игрок: " + backend.player_name
                    color: "#b9c0cc"
                    font.pixelSize: 14
                }
            }
        }

        Rectangle {
            Layout.fillWidth: true
            Layout.fillHeight: true
            radius: 14
            color: "#171a1f"
            border.width: 2
            border.color: backend.logs_border_color

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 10
                spacing: 8

                Label {
                    text: "󰍩 Logs"
                    color: "white"
                    font.bold: true
                }

                ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true

                    TextArea {
                        text: backend.logs_html
                        readOnly: true
                        selectByMouse: true
                        wrapMode: TextEdit.Wrap
                        textFormat: TextEdit.RichText
                        color: "#d7dbe2"
                        background: null
                        font.family: "monospace"
                    }
                }
            }
        }

        Rectangle {
            Layout.fillWidth: true
            Layout.preferredHeight: 64
            radius: 14
            color: "#171a1f"
            border.width: 2
            border.color: "#3a404b"

            RowLayout {
                anchors.fill: parent
                anchors.margins: 10
                spacing: 10

                FrameButton {
                    text: "Настройки"
                    frameColor: "#49a7ff"
                    accentColor: "#49a7ff"
                    onClicked: backend.open_settings()
                }

                FrameButton {
                    text: "Выйти"
                    frameColor: "#e25555"
                    accentColor: "#e25555"
                    onClicked: {
                        backend.request_exit()
                        Qt.quit()
                    }
                }

                Item { Layout.fillWidth: true }
            }
        }
    }

    Popup {
        id: settingsPopup
        modal: true
        focus: true
        closePolicy: Popup.NoAutoClose
        x: (root.width - width) / 2
        y: (root.height - height) / 2
        width: Math.min(root.width - 40, 520)
        height: 360
        visible: backend.settings_open

        onVisibleChanged: if (!visible) backend.close_settings()
        background: Rectangle {
            radius: 14
            color: "#171a1f"
            border.width: 2
            border.color: "#49a7ff"
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 14
            spacing: 10

            Label { text: "Настройки"; color: "white"; font.bold: true; font.pixelSize: 18 }

            TabBar {
                id: settingsTabs
                Layout.fillWidth: true
                currentIndex: 0
                background: Rectangle {
                    radius: 10
                    color: "#13161b"
                    border.width: 1
                    border.color: "#2f3540"
                }
                TabButton {
                    text: "Connection"
                    font.bold: true
                    contentItem: Label {
                        text: parent.text
                        color: parent.checked ? "#101214" : "#d7dbe2"
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                        font.bold: true
                    }
                    background: Rectangle {
                        radius: 8
                        color: parent.checked ? "#49a7ff" : "transparent"
                        border.width: parent.checked ? 0 : 1
                        border.color: "#2f3540"
                    }
                }
                TabButton {
                    text: "App"
                    font.bold: true
                    contentItem: Label {
                        text: parent.text
                        color: parent.checked ? "#101214" : "#d7dbe2"
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                        font.bold: true
                    }
                    background: Rectangle {
                        radius: 8
                        color: parent.checked ? "#49a7ff" : "transparent"
                        border.width: parent.checked ? 0 : 1
                        border.color: "#2f3540"
                    }
                }
            }

            StackLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                currentIndex: settingsTabs.currentIndex

                ColumnLayout {
                    spacing: 8
                    Label { text: "RCON IP"; color: "#9ecfff" }
                    TextField {
                        text: backend.settings_host
                        onTextChanged: backend.set_settings_host(text)
                        Layout.fillWidth: true
                    }

                    Label { text: "RCON Port"; color: "#9ecfff" }
                    TextField {
                        text: backend.settings_port
                        inputMethodHints: Qt.ImhDigitsOnly
                        onTextChanged: backend.set_settings_port(text)
                        Layout.fillWidth: true
                    }
                    Item { Layout.fillHeight: true }
                }

                ColumnLayout {
                    spacing: 8
                    Label { text: "Username"; color: "#9ecfff" }
                    TextField {
                        text: backend.settings_player_name
                        onTextChanged: backend.set_settings_player_name(text)
                        Layout.fillWidth: true
                    }

                    Label { text: "UI Mode"; color: "#9ecfff" }
                    ComboBox {
                        id: uiModeBox
                        Layout.fillWidth: true
                        model: ["tui", "qt"]
                        currentIndex: backend.settings_ui_mode === "qt" ? 1 : 0
                        onCurrentTextChanged: backend.set_settings_ui_mode(currentText)
                        font.bold: true

                        contentItem: Label {
                            text: uiModeBox.displayText
                            color: "#ffffff"
                            verticalAlignment: Text.AlignVCenter
                            leftPadding: 10
                            rightPadding: 30
                            font.bold: true
                        }

                        indicator: Label {
                            x: uiModeBox.width - width - 10
                            y: (uiModeBox.height - height) / 2
                            text: "▾"
                            color: "#9ecfff"
                            font.bold: true
                        }

                        background: Rectangle {
                            radius: 10
                            color: uiModeBox.pressed ? "#262b33" : "#171a1f"
                            border.width: 2
                            border.color: uiModeBox.visualFocus ? "#49a7ff" : "#3a404b"
                        }

                        popup: Popup {
                            y: uiModeBox.height + 4
                            width: uiModeBox.width
                            implicitHeight: contentItem.implicitHeight + 8
                            padding: 4
                            background: Rectangle {
                                radius: 10
                                color: "#171a1f"
                                border.width: 2
                                border.color: "#49a7ff"
                            }
                            contentItem: ListView {
                                clip: true
                                implicitHeight: contentHeight
                                model: uiModeBox.popup.visible ? uiModeBox.delegateModel : null
                                currentIndex: uiModeBox.highlightedIndex
                            }
                        }

                        delegate: ItemDelegate {
                            width: uiModeBox.width - 8
                            text: modelData
                            highlighted: uiModeBox.highlightedIndex === index
                            font.bold: true
                            contentItem: Label {
                                text: modelData
                                color: "#ffffff"
                                verticalAlignment: Text.AlignVCenter
                                leftPadding: 10
                                font.bold: true
                            }
                            background: Rectangle {
                                radius: 8
                                color: (uiModeBox.highlightedIndex === index) ? "#232a33" : "transparent"
                                border.width: (uiModeBox.highlightedIndex === index) ? 1 : 0
                                border.color: "#49a7ff"
                            }
                        }
                    }

                    Label {
                        text: "UI mode и username полностью применятся после перезапуска"
                        color: "#f2c14e"
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                    }
                    Item { Layout.fillHeight: true }
                }
            }

            RowLayout {
                Layout.alignment: Qt.AlignRight
                Layout.fillWidth: true
                Layout.topMargin: 4
                Layout.preferredHeight: 42
                spacing: 8
                Item { Layout.fillWidth: true }
                FrameButton {
                    text: "Отмена"
                    frameColor: "#8b93a1"
                    accentColor: "#8b93a1"
                    onClicked: backend.close_settings()
                }
                FrameButton {
                    text: "Применить"
                    frameColor: "#26c281"
                    accentColor: "#26c281"
                    onClicked: backend.save_settings()
                }
            }
        }
    }
}
"##;
