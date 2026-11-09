//! Cobra-style help/usage rendering from the clap command tree, so `--help` and
//! error output match the Go binary byte for byte.

use clap::{Arg, Command};

const GLOBAL_ARGS: [&str; 2] = ["config", "brokers"];

struct FlagLine {
    name: String,
    left: String,
    usage: String,
}

fn flag_line(arg: &Arg) -> FlagLine {
    let name = arg.get_long().unwrap_or_default().to_string();
    let mut left = match arg.get_short() {
        Some(s) => format!("  -{s}, --{name}"),
        None => format!("      --{name}"),
    };
    let kind = arg
        .get_value_names()
        .and_then(|v| v.first())
        .map(|v| v.to_string())
        .unwrap_or_default();
    // Bool flags are the ones with an optional value (`--flag[=false]`).
    let is_bool = arg.get_num_args().is_some_and(|n| n.min_values() == 0);
    if !is_bool && !kind.is_empty() {
        left.push(' ');
        left.push_str(&kind);
    }
    let mut usage = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
    if let Some(def) = arg.get_default_values().first() {
        let def = def.to_string_lossy();
        match (is_bool, kind.as_str()) {
            (true, _) if def == "true" => usage.push_str(" (default true)"),
            (false, "int") if def != "0" => usage.push_str(&format!(" (default {def})")),
            (false, "string") if !def.is_empty() => usage.push_str(&format!(" (default {def:?})")),
            _ => {}
        }
    }
    FlagLine { name, left, usage }
}

fn help_flag(cmd_name: &str) -> FlagLine {
    FlagLine {
        name: "help".into(),
        left: "  -h, --help".into(),
        usage: format!("help for {cmd_name}"),
    }
}

/// pflag's FlagUsages: sorted by name, usage column aligned three spaces past the widest flag.
fn render_flags(mut lines: Vec<FlagLine>) -> String {
    lines.sort_by(|a, b| a.name.cmp(&b.name));
    let width = lines.iter().map(|l| l.left.len()).max().unwrap_or(0);
    lines
        .iter()
        .map(|l| format!("{:width$}   {}", l.left, l.usage).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn global_flags(root: &Command) -> Vec<FlagLine> {
    root.get_arguments()
        .filter(|a| GLOBAL_ARGS.contains(&a.get_id().as_str()))
        .map(flag_line)
        .collect()
}

/// Cobra's UsageString for the root (`sub = None`) or a subcommand.
pub fn usage(root: &Command, sub: Option<&Command>) -> String {
    let mut out = String::from("Usage:\n");
    match sub {
        None => {
            out.push_str("  eraser [command]\n\nAvailable Commands:");
            let mut cmds: Vec<(String, String)> = root
                .get_subcommands()
                .map(|c| {
                    let about = c.get_about().map(|a| a.to_string()).unwrap_or_default();
                    (c.get_name().to_string(), about)
                })
                .collect();
            cmds.sort();
            let pad = cmds.iter().map(|c| c.0.len()).max().unwrap_or(0);
            for (name, about) in &cmds {
                out.push_str(&format!("\n  {name:pad$} {about}"));
            }
            let mut flags = global_flags(root);
            flags.push(help_flag("eraser"));
            out.push_str(&format!("\n\nFlags:\n{}", render_flags(flags)));
            out.push_str(
                "\n\nUse \"eraser [command] --help\" for more information about a command.\n",
            );
        }
        Some(sub) => {
            let name = sub.get_name();
            out.push_str(&format!("  eraser {name} [flags]\n\nFlags:\n"));
            let mut flags: Vec<FlagLine> = sub
                .get_arguments()
                .filter(|a| a.get_long().is_some() && !a.is_hide_set())
                .filter(|a| !GLOBAL_ARGS.contains(&a.get_id().as_str()) && a.get_id() != "help")
                .map(flag_line)
                .collect();
            flags.push(help_flag(name));
            out.push_str(&render_flags(flags));
            out.push_str(&format!(
                "\n\nGlobal Flags:\n{}\n",
                render_flags(global_flags(root))
            ));
        }
    }
    out
}

/// Cobra's help output: Long (or Short), blank line, usage.
pub fn help(root: &Command, sub: Option<&Command>) -> String {
    let cmd = sub.unwrap_or(root);
    let text = cmd
        .get_long_about()
        .or(cmd.get_about())
        .map(|t| t.to_string())
        .unwrap_or_default();
    format!("{}\n\n{}", text.trim_end(), usage(root, sub))
}
