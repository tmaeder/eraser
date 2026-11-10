//! `scan`, `companies`, `unsubscribe`, `request` (inbox company scanner).

use super::commands::{Ctx, RULE, open_store, prompt, truncate, wait_turn};
use anyhow::{Result, bail};
use eraser::config::Config;
use eraser::email::{self, Sender};
use eraser::history::{Company, Store};
use eraser::inbox::monitor::Monitor;
use eraser::scanner::actions::{self, Unsubscribe, request_target};
use eraser::scanner::discovery::Discoverer;
use eraser::scanner::{self, Identity};

fn load_inbox_config(ctx: &Ctx) -> Result<Config> {
    let cfg = ctx.load_config()?;
    if let Err(e) = cfg.validate_inbox() {
        bail!("{e:#}. Configure the inbox section in config.yaml (see 'eraser monitor --help')");
    }
    Ok(cfg)
}

pub async fn run_scan(ctx: &Ctx, days: i64, folders: Vec<String>, discover: bool) -> Result<()> {
    let cfg = load_inbox_config(ctx)?;
    let store = open_store("initialize")?;
    let id = Identity::from_config(&cfg);
    let mut monitor = Monitor::new(cfg.inbox.clone(), &[]);
    monitor
        .connect()
        .await
        .map_err(|e| e.context("failed to connect to inbox"))?;
    let result = async {
        let existing = monitor.list_folders().await?;
        let picked = scanner::pick_folders(&existing, &cfg.inbox, &folders);
        if picked.is_empty() {
            bail!(
                "none of the requested folders exist (available: {})",
                existing.join(", ")
            );
        }
        println!(
            "🔍 Scanning {} (last {days} days, headers only)...",
            picked.join(", ")
        );
        scanner::scan_folders(
            &mut monitor,
            &store,
            &id,
            &picked,
            days,
            |f, done, total| {
                println!("   {f}: {done}/{total}");
            },
        )
        .await
    }
    .await;
    let _ = monitor.disconnect().await;
    let r = result?;
    println!("✅ {} messages, {} new", r.total, r.new);

    if discover {
        let db = ctx.load_brokers().unwrap_or_default();
        let pending: Vec<String> = store
            .list_companies()?
            .into_iter()
            .filter(|c| active(c) && c.contact_status.is_empty())
            .map(|c| c.agg.domain)
            .collect();
        if !pending.is_empty() {
            println!();
            println!(
                "🌐 Looking up privacy contacts for {} companies...",
                pending.len()
            );
            for (domain, c) in Discoverer::new()?
                .discover_and_store(&store, &db.brokers, &pending)
                .await
            {
                let found = if c.email.is_empty() {
                    "manual".into()
                } else {
                    c.email
                };
                println!("   {domain}: {found}");
            }
        }
    }
    println!();
    print_companies(&store, false, false, false)
}

fn active(c: &Company) -> bool {
    c.excluded_reason.is_empty() && c.decision != "ignored"
}

fn print_companies(store: &Store, lists: bool, manual: bool, all: bool) -> Result<()> {
    let companies: Vec<Company> = store
        .list_companies()?
        .into_iter()
        .filter(|c| all || active(c))
        .filter(|c| !lists || c.agg.is_mailing_list)
        .filter(|c| !manual || c.contact_status == "manual")
        .collect();
    if companies.is_empty() {
        println!("No companies. Run 'eraser scan' first.");
        return Ok(());
    }
    println!(
        "{:<28} {:>5}  {:<4} {:<32} {:<10} STATUS",
        "DOMAIN", "MAILS", "LIST", "PRIVACY CONTACT", "UNSUB"
    );
    println!("{RULE}{RULE}");
    for c in &companies {
        let contact = match (c.contact_email.as_str(), c.contact_status.as_str()) {
            ("", "") => "(not checked)".to_string(),
            ("", s) => format!("({s})"),
            (e, _) => e.to_string(),
        };
        let mut status = Vec::new();
        if !c.excluded_reason.is_empty() {
            status.push(format!("excluded:{}", c.excluded_reason));
        }
        if c.decision == "ignored" {
            status.push("ignored".into());
        }
        if c.requested_at.is_some() {
            status.push("requested".into());
        }
        if c.unsubscribed_at.is_some() {
            status.push("unsubscribed".into());
        }
        println!(
            "{:<28} {:>5}  {:<4} {:<32} {:<10} {}",
            truncate(&c.agg.domain, 28),
            c.agg.message_count,
            if c.agg.is_mailing_list { "yes" } else { "" },
            truncate(&contact, 32),
            Unsubscribe::for_company(c).describe().0,
            status.join(" ")
        );
    }
    println!();
    println!("{} companies", companies.len());
    Ok(())
}

pub fn run_companies(lists: bool, manual: bool, all: bool) -> Result<()> {
    print_companies(&open_store("open")?, lists, manual, all)
}

fn lookup(store: &Store, domains: &[String]) -> Result<Vec<Company>> {
    if domains.is_empty() {
        bail!("requires at least one company domain (see 'eraser companies')");
    }
    domains
        .iter()
        .map(|d| {
            let d = d.trim().to_ascii_lowercase();
            store
                .get_company(&d)?
                .ok_or_else(|| anyhow::anyhow!("unknown company {d:?} (run 'eraser scan' first)"))
        })
        .collect()
}

fn confirm(yes: bool, dry_run: bool, n: usize) -> bool {
    if yes || dry_run {
        return true;
    }
    let stdin = std::io::stdin();
    let answer = prompt(
        &mut stdin.lock(),
        &format!("Contact these {n} companies? [y/N]: "),
    );
    matches!(answer.to_lowercase().as_str(), "y" | "yes")
}

async fn pause(cfg: &Config, i: usize, n: usize) {
    if i + 1 < n {
        tokio::time::sleep(cfg.pacing().min_interval).await;
    }
}

pub async fn run_request(ctx: &Ctx, domains: Vec<String>, dry_run: bool, yes: bool) -> Result<()> {
    let cfg = ctx.load_config()?;
    if !dry_run {
        cfg.validate()?;
    }
    let store = open_store("open")?;
    let companies = lookup(&store, &domains)?;
    println!("📧 Deletion requests (template {}):", cfg.options.template);
    let mut runnable = Vec::new();
    for c in &companies {
        match request_target(c) {
            Ok(to) => {
                println!("   {} → {to}  (source: {})", c.agg.domain, c.contact_source);
                runnable.push(c);
            }
            Err(e) => println!("   {} → skipped: {e}", c.agg.domain),
        }
    }
    if runnable.is_empty() {
        bail!("no company has a privacy contact to send to");
    }
    if !confirm(yes, dry_run, runnable.len()) {
        println!("Aborted.");
        return Ok(());
    }
    let sender: Box<dyn Sender> = email::new_sender(&cfg.email)?;
    let engine = eraser::template::Engine::new()?;
    let (mut ok, mut failed) = (0, 0);
    for (i, c) in runnable.iter().enumerate() {
        if !dry_run && let Err(hold) = wait_turn(&store, &cfg).await {
            println!("   ⛔ {hold}");
            break;
        }
        let r = actions::send_request(&store, &engine, sender.as_ref(), &cfg, c, dry_run).await;
        if !r.success && email::trip_on_rejection(&store, &cfg, r.error.as_deref().unwrap_or("")) {
            failed += 1;
            println!(
                "   ⛔ {}",
                email::Hold::Blocked(r.error.unwrap_or_default())
            );
            ctx.pause_autopilot_if_blocked(&store);
            break;
        }
        if r.success {
            ok += 1;
            println!(
                "   ✅ {}{}",
                c.agg.domain,
                if dry_run { " (dry run)" } else { "" }
            );
        } else {
            failed += 1;
            println!("   ❌ {}: {}", c.agg.domain, r.error.unwrap_or_default());
        }
        if !dry_run {
            pause(&cfg, i, runnable.len()).await;
        }
    }
    println!("Done: {ok} ok, {failed} failed");
    Ok(())
}

pub async fn run_unsubscribe(
    ctx: &Ctx,
    domains: Vec<String>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let cfg = ctx.load_config()?;
    let store = open_store("open")?;
    let companies = lookup(&store, &domains)?;
    println!("✂️  Unsubscribe:");
    let mut runnable = Vec::new();
    for c in &companies {
        match Unsubscribe::for_company(c) {
            Unsubscribe::OneClick(u) => {
                println!("   {} → one-click POST {u}", c.agg.domain);
                runnable.push(c);
            }
            Unsubscribe::Mailto(u) => {
                println!("   {} → email {u}", c.agg.domain);
                runnable.push(c);
            }
            Unsubscribe::Manual(u) => {
                println!("   {} → open this link yourself: {u}", c.agg.domain);
            }
            Unsubscribe::None => println!("   {} → no List-Unsubscribe header", c.agg.domain),
        }
    }
    if runnable.is_empty() {
        return Ok(());
    }
    if !confirm(yes, dry_run, runnable.len()) {
        println!("Aborted.");
        return Ok(());
    }
    let needs_mail = runnable
        .iter()
        .any(|c| Unsubscribe::for_company(c).sends_email());
    if needs_mail && !dry_run {
        cfg.validate()?;
    }
    let sender = email::new_sender(&cfg.email)?;
    let http = actions::http_client();
    for (i, c) in runnable.iter().enumerate() {
        let emails = Unsubscribe::for_company(c).sends_email();
        if emails
            && !dry_run
            && let Err(hold) = wait_turn(&store, &cfg).await
        {
            println!("   ⛔ {hold}");
            break;
        }
        let r = actions::unsubscribe(&store, &http, sender.as_ref(), &cfg, c, dry_run).await;
        if emails && email::trip_on_rejection(&store, &cfg, r.error.as_deref().unwrap_or("")) {
            println!(
                "   ⛔ {}",
                email::Hold::Blocked(r.error.unwrap_or_default())
            );
            ctx.pause_autopilot_if_blocked(&store);
            break;
        }
        match r.error {
            None => println!(
                "   ✅ {}{}",
                c.agg.domain,
                if dry_run { " (dry run)" } else { "" }
            ),
            Some(e) => println!("   ❌ {}: {e}", c.agg.domain),
        }
        if !dry_run {
            pause(&cfg, i, runnable.len()).await;
        }
    }
    Ok(())
}
