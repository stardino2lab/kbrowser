#[macro_use]
mod json;
mod actions;
mod cdp;
mod cli;
mod daemon;
mod i18n;
mod ipc;
mod overlay;
mod pins;
mod platform;
mod snapshot;
mod term;
mod util;
mod view;
mod ws;

fn main() {
    let code = cli::run(std::env::args().skip(1).collect());
    std::process::exit(code);
}
