//! `monitor` and `cleanup-bounces` (IMAP-backed commands).

use super::commands::{Ctx, RULE, open_store, truncate};
use anyhow::{Result, anyhow};
use chrono::Local;
use eraser::history::{BrokerResponse, PipelineStatus};
use eraser::inbox::Email;
use eraser::inbox::classifier::{
    ClassifiedResponse, ResponseType, classify_response, summarize_responses,
};
use eraser::inbox::monitor::Monitor;
use eraser::inbox::parser::extract_bounced_recipient;
use tokio::sync::watch;

fn print_inbox_setup_help() {
    println!("📧 Inbox monitoring is not configured.");
    println!();
    println!("To enable inbox monitoring, add the following to your config.yaml:");
    println!();
    println!("inbox:");
    println!("  enabled: true");
    println!("  provider: icloud");
    println!("  email: you@icloud.com");
    println!(
        "  password: xxxx-xxxx-xxxx-xxxx  # App-specific password, not your Apple ID password"
    );
    println!();
    println!("For iCloud, you'll need to:");
    println!("  1. Enable two-factor authentication for your Apple ID");
    println!(
        "  2. Generate an app-specific password at https://account.apple.com (Sign-In and Security)"
    );
}

fn broker_response(email: &Email, c: &ClassifiedResponse) -> BrokerResponse {
    BrokerResponse {
        broker_id: email.broker_id.clone(),
        broker_name: email.broker_name.clone(),
        response_type: c.r#type.to_string(),
        email_from: email.from.clone(),
        email_subject: email.subject.clone(),
        form_url: c.form_url.clone(),
        confirm_url: c.confirm_url.clone(),
        confidence: c.confidence,
        needs_review: c.needs_review,
        received_at: Some(email.received_at.with_timezone(&Local)),
        ..Default::default()
    }
}

fn pipeline_status_for(t: ResponseType) -> PipelineStatus {
    match t {
        ResponseType::Success => PipelineStatus::Confirmed,
        ResponseType::FormRequired => PipelineStatus::FormRequired,
        ResponseType::ConfirmationRequired => PipelineStatus::AwaitingConfirmation,
        ResponseType::Rejected => PipelineStatus::Rejected,
        _ => PipelineStatus::AwaitingResponse,
    }
}

fn print_classified_response(r: &ClassifiedResponse) {
    let icon = match r.r#type {
        ResponseType::Success => "✅",
        ResponseType::FormRequired => "📝",
        ResponseType::ConfirmationRequired => "🔗",
        ResponseType::Rejected => "❌",
        ResponseType::Pending => "⏳",
        _ => "❓",
    };
    println!("{icon} {} - {}", r.email.broker_name, r.r#type);
    println!("   Subject: {}", r.email.subject);
    if !r.form_url.is_empty() {
        println!("   📝 Form URL: {}", r.form_url);
    }
    if !r.confirm_url.is_empty() {
        println!("   🔗 Confirm URL: {}", r.confirm_url);
    }
    if r.needs_review {
        println!(
            "   ⚠️  Confidence: {:.0}% - manual review recommended",
            r.confidence * 100.0
        );
    }
}

/// Like Go's signal.Notify(SIGINT, SIGTERM): the first signal prints a notice and
/// flips the returned flag instead of killing the process.
fn shutdown_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        println!("\nShutting down...");
        let _ = tx.send(true);
    });
    rx
}

pub async fn run_monitor(ctx: &Ctx, days: i64, once: bool, watch_mode: bool) -> Result<()> {
    let cfg = ctx.load_config()?;
    if let Err(e) = cfg.validate_inbox() {
        print_inbox_setup_help();
        return Err(e);
    }
    let db = ctx.load_brokers()?;
    let store = open_store("initialize")?;
    let mut monitor = Monitor::new(cfg.inbox.clone(), &db.brokers);
    let stop = shutdown_signal();

    monitor
        .connect()
        .await
        .map_err(|e| e.context("failed to connect to inbox"))?;
    let result = monitor_session(&mut monitor, &cfg, &store, days, once, watch_mode, stop).await;
    let _ = monitor.disconnect().await;
    result
}

async fn monitor_session(
    monitor: &mut Monitor,
    cfg: &eraser::config::Config,
    store: &eraser::history::Store,
    days: i64,
    once: bool,
    watch_mode: bool,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    println!("📬 Monitoring inbox for broker responses (last {days} days)...");
    println!();

    let emails = monitor
        .fetch_broker_emails(days)
        .await
        .map_err(|e| e.context("failed to fetch emails"))?;
    if emails.is_empty() {
        println!("No emails from known brokers found.");
        if !watch_mode {
            return Ok(());
        }
    }

    println!("Found {} emails from data brokers", emails.len());
    println!();

    let mut responses = Vec::new();
    for email in &emails {
        let classified = classify_response(email);
        let mut resp = broker_response(email, &classified);
        if let Err(e) = store.add_broker_response(&mut resp) {
            println!("⚠️  Failed to store response: {e:#}");
        }
        // Ignore errors (no matching record), as in Go.
        let _ =
            store.update_pipeline_status(&email.broker_id, pipeline_status_for(classified.r#type));
        print_classified_response(&classified);
        responses.push(classified);
    }

    if cfg.inbox.auto_archive && !emails.is_empty() {
        let folder = &cfg.inbox.archive_folder;
        if let Err(e) = monitor.ensure_folder_exists(folder).await {
            println!("⚠️  Could not create archive folder: {e:#}");
        } else {
            let uids: Vec<u32> = emails.iter().map(|e| e.uid).filter(|&u| u > 0).collect();
            if !uids.is_empty() {
                match monitor.archive_emails(&uids, folder).await {
                    Err(e) => println!("⚠️  Could not archive emails: {e:#}"),
                    Ok(()) => println!("📁 Archived {} emails to '{folder}'", uids.len()),
                }
            }
        }
    }

    let s = summarize_responses(&responses);
    println!();
    println!("{RULE}");
    println!("📊 Summary:");
    println!("  Total responses:     {}", s.total);
    println!("  ✅ Success:          {}", s.success);
    println!("  📝 Form required:    {}", s.form_required);
    println!("  🔗 Confirm required: {}", s.confirm_required);
    println!("  ❌ Rejected:         {}", s.rejected);
    println!("  ⏳ Pending:          {}", s.pending);
    println!("  ❓ Unknown:          {}", s.unknown);
    println!("  👁️  Need review:      {}", s.need_review);

    if once || !watch_mode {
        return Ok(());
    }

    println!();
    println!("👀 Watching for new emails... (Ctrl+C to stop)");
    let stopped = async move {
        let _ = stop.wait_for(|v| *v).await;
    };
    monitor
        .watch_for_new_emails(
            |email| {
                println!();
                println!("📨 New email from {} ({})", email.broker_name, email.from);
                let classified = classify_response(&email);
                print_classified_response(&classified);
                let _ = store.add_broker_response(&mut broker_response(&email, &classified));
            },
            stopped,
        )
        .await
        .map_err(|e| e.context("watch error"))
}

pub async fn run_cleanup_bounces(ctx: &Ctx, remove: bool, days: i64) -> Result<()> {
    let cfg = ctx.load_config()?;
    if !cfg.inbox.enabled {
        return Err(anyhow!(
            "inbox monitoring not configured. Run 'eraser init' to set up"
        ));
    }
    let broker_path = ctx.broker_path();
    let mut db = ctx.load_brokers()?;

    println!("🔍 Scanning inbox for bounced emails...");
    println!("{RULE}");
    println!();

    let mut monitor = Monitor::new(cfg.inbox.clone(), &db.brokers);
    monitor
        .connect()
        .await
        .map_err(|e| e.context("failed to connect to inbox"))?;
    let bounces = monitor.fetch_bounce_emails(days).await;
    let _ = monitor.disconnect().await;
    let bounces = bounces.map_err(|e| e.context("failed to fetch bounce emails"))?;

    if bounces.is_empty() {
        println!("✓ No bounced emails found!");
        return Ok(());
    }
    println!("Found {} bounced email(s):\n", bounces.len());

    // (bounced address, broker name)
    let mut bounced: Vec<(String, String)> = Vec::new();
    for email in &bounces {
        let recipient = extract_bounced_recipient(email);
        if recipient.is_empty() {
            println!(
                "⚠️  Could not extract bounced address from: {}",
                email.subject
            );
            continue;
        }
        let Some(b) = db.find_by_email(&recipient) else {
            println!("⚠️  {recipient} - not found in broker database");
            continue;
        };
        println!("❌ {recipient}");
        println!("   Broker: {} ({})", b.name, b.id);
        println!("   Subject: {}", truncate(&email.subject, 60));
        println!("   Date: {}", email.received_at.format("%Y-%m-%d"));
        println!();
        bounced.push((recipient, b.name.clone()));
    }

    if bounced.is_empty() {
        println!("✓ No broker email addresses need to be removed");
        return Ok(());
    }

    println!("{RULE}");

    if !remove {
        println!(
            "\n📊 Found {} broker(s) with invalid email addresses",
            bounced.len()
        );
        println!("Run with --remove to delete these brokers from the database");
        return Ok(());
    }

    println!(
        "\n🗑️  Removing {} broker(s) from database...\n",
        bounced.len()
    );
    let mut removed = 0;
    for (addr, name) in &bounced {
        if db.remove_by_email(addr).is_some() {
            println!("✓ Removed {name} ({addr})");
            removed += 1;
        }
    }

    db.save_with_backup(&broker_path)
        .map_err(|e| e.context("failed to save broker database"))?;

    println!();
    println!("{RULE}");
    println!("✓ Removed {removed} broker(s) with invalid email addresses");
    println!("  Backup saved to: {}.bak", broker_path.display());
    Ok(())
}
