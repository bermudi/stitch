mod ancestor;
mod cli;
mod commands;
mod config;
mod error;
mod fsutil;
mod hooks;
mod linker;
mod plan;
mod plan_exec;
mod plan_file;
mod plan_validate;
mod platform;
mod render;
mod report;
mod safety;
mod scan;
mod store;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let json = cli.json;
    let command_name = commands::command_name(&cli.command);
    if let Err(e) = commands::run(cli) {
        if json {
            report::write_error(command_name, &e, Vec::new());
        } else {
            eprintln!("error: {e}");
            if let Some(hint) = e.hint() {
                eprintln!("hint: {hint}");
            }
        }
        std::process::exit(e.exit_code());
    }
}
