use std::path::{Path, PathBuf};

use crate::{AppConfig, BlockDeleteController, UiHandle};

pub(crate) struct BackendBootstrap {
    pub(crate) config: AppConfig,
    pub(crate) config_path: PathBuf,
    pub(crate) config_dir: PathBuf,
}

impl BackendBootstrap {
    pub(crate) fn from_config_path(config_path: &Path) -> Result<Self, String> {
        if !config_path.exists() {
            return Err(format!("Config file not found: {}", config_path.display()));
        }

        let config = AppConfig::load(config_path)?;
        let config_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            config,
            config_path: config_path.to_path_buf(),
            config_dir,
        })
    }

    pub(crate) fn build_controller(self, ui: UiHandle) -> Result<BlockDeleteController, String> {
        BlockDeleteController::new(self.config, self.config_path, self.config_dir, ui)
    }
}

