#[cfg(not(windows))]
pub mod attention;
#[cfg(not(windows))]
pub mod cli;
pub mod client_bridge;
pub mod client_cli;
pub mod config;
pub mod consult;
pub mod core;
pub mod doctor;
pub mod expert_refresh;
mod expert_search;
pub mod experts;
pub mod fleet;
pub mod hooks;
pub mod model;
pub mod monitor;
pub mod open_history;
pub mod paths;
pub mod process;
pub mod providers;
pub mod resolve;
pub mod scheduler;
pub mod setup;
pub mod setup_preview;
pub mod skill;
pub mod status;
pub mod store;
pub mod terminal;
pub mod tmux;
pub mod update;
pub mod usage;
pub mod wait;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run<I, T>(args: I) -> anyhow::Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    #[cfg(not(windows))]
    {
        cli::run(args)
    }
    #[cfg(windows)]
    {
        client_cli::run(args)
    }
}
