//! cardinal-gui: standalone binary entry point.
//!
//! Loads the gateway configuration and launches the GUI as a
//! management/monitoring surface for the standalone `cardinal-gateway`
//! process.

use std::path::{Path, PathBuf};

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "cardinal-gui",
    version,
    about = "management/monitoring GUI for the standalone cardinal-gateway"
)]
struct Cli {
    /// Path to the main config file; defaults to `~/.ox/config.toml`.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    let config_path = resolve_config_path(cli.config.as_deref());
    let gateway_config = load_gateway(config_path.as_deref());
    let gui = config_path
        .as_deref()
        .map(cardinal_gui::load::gui_section)
        .unwrap_or_default();

    cardinal_gui::run(cardinal_gui::Configs {
        gateway: gateway_config,
        gui,
        config_path,
    });
}

/// The config file to open: `--config` when given, else `~/.ox/config.toml`.
/// `None` only when neither resolves (no home directory and no flag).
fn resolve_config_path(path: Option<&Path>) -> Option<PathBuf> {
    path.map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".ox").join("config.toml")))
}

/// The GUI's gateway config: the `[gateway]` section of the main config file.
/// `--config` overrides the default `~/.ox/config.toml` path (same posture as
/// `cardinal-gateway`); a missing/unreadable file yields the defaults with a
/// stderr note — the GUI must still open so the user can see the gateway is
/// unreachable.
fn load_gateway(path: Option<&Path>) -> gateway::config::Config {
    match path {
        Some(path) => gateway::config::Config::load(path).unwrap_or_else(|error| {
            eprintln!(
                "warning: failed to load gateway config from {}: {error}; using defaults",
                path.display()
            );
            gateway::config::Config::default()
        }),
        None => gateway::config::Config::default(),
    }
}
