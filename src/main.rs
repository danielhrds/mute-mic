#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod audio;
mod input;
mod startup;
mod storage;
mod ui;
mod volume;

fn main() -> std::process::ExitCode {
    if let Err(error) = app::run() {
        app::show_error(&format!("MuteMic could not start.\n\n{error:#}"));
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
