//! CLI entry: parsing, help/error output, dispatch.

mod args;
mod browser_cmds;
mod commands;
mod help;
mod inbox_cmds;
mod scanner_cmds;

use args::{Cli, Cmd};
use clap::{Command, CommandFactory, FromArgMatches};
use std::ffi::OsString;

/// Runs the CLI and returns the process exit code.
pub async fn run(argv: Vec<OsString>) -> i32 {
    let root = Cli::command();
    let matches = match root.clone().try_get_matches_from(&argv) {
        Ok(m) => m,
        Err(e) => return clap_error(&root, &argv, &e),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => return clap_error(&root, &argv, &e),
    };
    let sub = matches
        .subcommand_name()
        .and_then(|n| root.find_subcommand(n));

    let Some(cmd) = cli.command else {
        print!("{}", help::help(&root, None));
        return 0;
    };
    if cli.help {
        print!("{}", help::help(&root, sub));
        return 0;
    }
    if let Cmd::Help { topic } = &cmd {
        match topic.first() {
            None => print!("{}", help::help(&root, None)),
            Some(name) => match root.find_subcommand(name) {
                Some(c) => print!("{}", help::help(&root, Some(c))),
                None => {
                    let quoted: Vec<String> = topic.iter().map(|t| format!("`{t}`")).collect();
                    eprintln!("Unknown help topic [{}]", quoted.join(" "));
                    eprint!("{}", help::usage(&root, None));
                }
            },
        }
        return 0;
    }

    let ctx = commands::Ctx {
        config: cli.config.filter(|p| !p.as_os_str().is_empty()),
        brokers: cli.brokers.filter(|p| !p.as_os_str().is_empty()),
    };
    match commands::dispatch(&ctx, cmd).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    }
}

/// Parse errors: clap's one-line message, then the command's usage.
fn clap_error(root: &Command, argv: &[OsString], e: &clap::Error) -> i32 {
    let sub = argv
        .iter()
        .skip(1)
        .filter_map(|a| a.to_str())
        .find_map(|a| root.find_subcommand(a));
    let rendered = e.render().to_string();
    let msg = rendered.lines().next().unwrap_or_default();
    eprintln!("Error: {}", msg.trim_start_matches("error: "));
    eprint!("{}", help::usage(root, sub));
    1
}
