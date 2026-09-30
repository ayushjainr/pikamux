mod activity_feed;
#[cfg(unix)]
mod activity_lease;
#[cfg(not(windows))]
mod activity_observer;
#[cfg(unix)]
mod assistant;
#[cfg(unix)]
mod assistant_author;
pub mod assistant_briefing;
pub mod assistant_client;
#[cfg(unix)]
mod assistant_consultation_permissions;
mod assistant_context;
mod assistant_continuity;
#[cfg(unix)]
mod assistant_control;
pub mod assistant_coordinator;
pub mod assistant_decisions;
pub mod assistant_evolution;
#[cfg(unix)]
mod assistant_feedback;
#[cfg(unix)]
mod assistant_native;
#[cfg(unix)]
mod assistant_native_helpers;
#[cfg(unix)]
mod assistant_native_profile;
#[cfg(unix)]
mod assistant_native_recovery;
#[cfg(unix)]
mod assistant_native_tools;
#[cfg(unix)]
mod assistant_native_turns;
// Shared assistant domain types remain portable, but their authority-side
// operations are driven only by the Unix host. Windows is a remote client.
// Keep dead-code enforcement on the hosting targets rather than exporting
// internal mutation APIs merely to satisfy a client-only build.
#[cfg_attr(windows, allow(dead_code))]
mod assistant_guidance;
#[cfg(unix)]
mod assistant_host;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_investigation;
#[cfg(unix)]
pub mod assistant_investigation_provider;
#[cfg(unix)]
pub mod assistant_investigation_service;
#[cfg(unix)]
pub mod assistant_learning;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_lifecycle;
#[cfg_attr(windows, allow(dead_code))]
mod assistant_maintenance;
#[cfg(unix)]
mod assistant_maintenance_driver;
#[cfg(unix)]
mod assistant_maintenance_worker;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_memory;
#[cfg_attr(windows, allow(dead_code))]
mod assistant_method;
#[cfg(unix)]
mod assistant_method_controls;
#[cfg(unix)]
mod assistant_method_service;
#[cfg(unix)]
mod assistant_observation;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_policy;
mod assistant_preferences;
#[cfg(unix)]
mod assistant_presentation;
#[cfg(unix)]
mod assistant_profile_views;
pub mod assistant_provider;
pub mod assistant_recovery;
pub mod assistant_recovery_service;
#[cfg(unix)]
mod assistant_remote;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_retention;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_runtime;
#[cfg(unix)]
mod assistant_self_awareness;
pub mod assistant_service;
#[cfg(unix)]
mod assistant_session;
#[cfg(unix)]
mod assistant_startup;
#[cfg_attr(windows, allow(dead_code))]
mod assistant_storage;
#[cfg(unix)]
pub mod assistant_transport;
#[cfg_attr(windows, allow(dead_code))]
pub mod assistant_workshop;
#[cfg_attr(windows, allow(dead_code))]
mod assistant_workshop_handoff;
pub mod assistant_workshop_ui;
#[cfg(not(windows))]
pub mod attention;
mod board_add;
mod board_catalog;
mod claude_quota;
#[cfg(not(windows))]
pub mod cli;
pub mod client_board;
pub mod client_bridge;
pub mod client_cli;
pub mod config;
pub mod consult;
pub mod consult_telemetry;
pub mod core;
pub mod doctor;
pub mod expert_refresh;
mod expert_search;
pub mod experts;
#[cfg(not(windows))]
mod files_data;
#[cfg(not(windows))]
mod files_layout;
#[cfg(not(windows))]
mod files_markdown;
#[cfg(not(windows))]
mod files_syntax;
#[cfg(not(windows))]
mod files_view;
pub mod fleet;
pub mod hooks;
mod machine_settings;
pub mod model;
pub mod monitor;
mod muse;
mod named_discovery;
mod onboarding;
pub mod open_history;
pub mod paths;
mod preview;
pub mod process;
pub mod providers;
mod quota;
pub mod resolve;
pub mod scheduler;
pub mod setup;
pub mod setup_preview;
pub mod skill;
pub mod status;
pub mod store;
pub mod terminal;
mod terminal_frame;
pub mod tmux;
pub mod update;
mod update_check;
pub mod usage;
pub mod wait;
#[cfg(windows)]
mod windows_io;
mod windows_update;

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
