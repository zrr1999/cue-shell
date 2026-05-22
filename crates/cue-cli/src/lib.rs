#[cfg(feature = "tui")]
pub mod config;
#[cfg(feature = "tui")]
pub mod path_lookup;
#[cfg(feature = "tui")]
mod tui;

#[cfg(feature = "tui")]
pub fn run_tui() -> anyhow::Result<()> {
    tui::run()
}
