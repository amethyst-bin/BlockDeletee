use std::sync::{Arc, Mutex};

use crate::backend_bootstrap::BackendBootstrap;
use crate::{UiState, UiHandle};

pub(crate) fn run_tui_mode(bootstrap: BackendBootstrap) -> Result<(), String> {
    let ui: UiHandle = Arc::new(Mutex::new(UiState::new(
        bootstrap.config.microphone.player_name.clone(),
        bootstrap.config.minecraft.rcon_host.clone(),
        bootstrap.config.minecraft.rcon_port,
        bootstrap.config.minecraft.rcon_password.clone(),
        bootstrap.config.ui.mode.unwrap_or(crate::UiMode::Tui),
    )));
    let controller = bootstrap.build_controller(ui)?;
    controller.run()
}
