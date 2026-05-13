mod agent;
mod app;
mod approval;
mod event;
mod config;
mod context;
mod fs;
mod git;
mod model;
mod session;
mod shell;
mod tools;
mod tui;

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    match runtime.block_on(app::run()) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
