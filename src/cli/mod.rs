//! CLI entry: parsing, cobra-compatible help/error output, dispatch.

mod args;
mod browser_cmds;
mod commands;
mod help;
mod inbox_cmds;
mod scanner_cmds;

use args::{Cli, Cmd};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, Command, CommandFactory, FromArgMatches};
use std::ffi::OsString;

/// Runs the CLI and returns the process exit code.
pub async fn run(argv: Vec<OsString>) -> i32 {
    // Cobra subcommands silently accept positional args; mirror that.
    let root = Cli::command().mut_subcommands(|c| {
        if matches!(c.get_name(), "help" | "unsubscribe" | "request") {
            c
        } else {
            c.arg(Arg::new("ignored_args").num_args(0..).hide(true))
        }
    });
    let matches = match root.clone().try_get_matches_from(&argv) {
        Ok(m) => m,
        Err(e) => return clap_error(&root, &argv, e),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => return clap_error(&root, &argv, e),
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
            fail(&root, sub, &format!("{e:#}"));
            1
        }
    }
}

/// Cobra's failure output: "Error: msg", the command usage, then main.go prints msg again.
fn fail(root: &Command, sub: Option<&Command>, msg: &str) {
    eprintln!("Error: {msg}");
    eprintln!("{}", help::usage(root, sub));
    eprintln!("{msg}");
}

fn context_str(e: &clap::Error, kind: ContextKind) -> String {
    match e.get(kind) {
        Some(ContextValue::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn clap_error(root: &Command, argv: &[OsString], e: clap::Error) -> i32 {
    let sub = argv
        .iter()
        .skip(1)
        .filter_map(|a| a.to_str())
        .find_map(|a| root.find_subcommand(a));
    match e.kind() {
        ErrorKind::InvalidSubcommand => {
            let name = context_str(&e, ContextKind::InvalidSubcommand);
            eprintln!("Error: unknown command {name:?} for \"eraser\"");
            eprintln!("Run 'eraser --help' for usage.");
            eprintln!("unknown command {name:?} for \"eraser\"");
        }
        ErrorKind::UnknownArgument => {
            let arg = context_str(&e, ContextKind::InvalidArg);
            let arg = arg.split('=').next().unwrap_or_default();
            let msg = match arg.strip_prefix('-') {
                Some(s) if !s.starts_with('-') => {
                    format!(
                        "unknown shorthand flag: '{}' in {arg}",
                        &s[..1.min(s.len())]
                    )
                }
                _ => format!("unknown flag: {arg}"),
            };
            fail(root, sub, &msg);
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            let arg = context_str(&e, ContextKind::InvalidArg);
            let flag = arg.split([' ', '=', '[']).next().unwrap_or_default();
            let value = context_str(&e, ContextKind::InvalidValue);
            let kind = sub
                .and_then(|c| {
                    c.get_arguments()
                        .find(|a| a.get_long() == flag.strip_prefix("--"))
                })
                .and_then(|a| {
                    a.get_value_names()
                        .and_then(|v| v.first())
                        .map(|v| v.to_string())
                });
            // Go's strconv wording.
            let reason = match kind.as_deref() {
                Some("int")
                    if !value.is_empty()
                        && value
                            .trim_start_matches('-')
                            .bytes()
                            .all(|b| b.is_ascii_digit()) =>
                {
                    format!("strconv.ParseInt: parsing {value:?}: value out of range")
                }
                Some("int") => format!("strconv.ParseInt: parsing {value:?}: invalid syntax"),
                _ => format!("strconv.ParseBool: parsing {value:?}: invalid syntax"),
            };
            fail(
                root,
                sub,
                &format!("invalid argument {value:?} for {flag:?} flag: {reason}"),
            );
        }
        _ => {
            let rendered = e.render().to_string();
            let msg = rendered
                .lines()
                .next()
                .unwrap_or_default()
                .trim_start_matches("error: ")
                .to_string();
            fail(root, sub, &msg);
        }
    }
    1
}
