//! Command implementations (ports of the run* functions in cmd/eraser/main.go).

use super::args::Cmd;
use anyhow::{Context, Result};
use eraser::history::{PipelineStatus, Record, Status, Store};
use eraser::{broker, config, email, history, template};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RULE: &str = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";

pub struct Ctx {
    pub config: Option<PathBuf>,
    pub brokers: Option<PathBuf>,
}

impl Ctx {
    pub fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(config::default_config_path)
    }

    /// --brokers, else ./data/brokers.yaml if present, else data/brokers.yaml next to the binary.
    pub fn broker_path(&self) -> PathBuf {
        if let Some(p) = &self.brokers {
            return p.clone();
        }
        let local = Path::new("data/brokers.yaml");
        if local.exists() {
            return local.to_path_buf();
        }
        let exe = std::env::current_exe().unwrap_or_default();
        exe.parent()
            .unwrap_or(Path::new(""))
            .join("data")
            .join("brokers.yaml")
    }

    pub fn load_config(&self) -> Result<config::Config> {
        config::load(self.config_path()).context("failed to load config")
    }

    pub fn load_brokers(&self) -> Result<broker::BrokerDatabase> {
        broker::load_from_file(self.broker_path()).context("failed to load brokers")
    }
}

pub async fn dispatch(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Init => run_init(ctx),
        Cmd::Send { dry_run } => run_send(ctx, dry_run).await,
        Cmd::Serve {
            port,
            host,
            public_origin,
        } => run_serve(ctx, host, port, public_origin).await,
        Cmd::ListBrokers => run_list_brokers(ctx),
        Cmd::Status { limit } => run_status(limit),
        Cmd::AddBroker => run_add_broker(ctx),
        Cmd::Monitor { days, once, watch } => {
            super::inbox_cmds::run_monitor(ctx, days, once, watch).await
        }
        Cmd::Pipeline => run_pipeline_status(),
        Cmd::Fill {
            broker,
            url,
            headless,
            submit,
            screenshots,
            pending,
            wait,
        } => {
            super::browser_cmds::run_fill(
                ctx,
                broker,
                url,
                headless,
                submit,
                screenshots,
                pending,
                wait,
            )
            .await
        }
        Cmd::Confirm {
            url,
            broker,
            pending,
            validate_domain,
            dry_run,
        } => {
            super::browser_cmds::run_confirm(ctx, url, broker, pending, validate_domain, dry_run)
                .await
        }
        Cmd::CleanupBounces { remove, days } => {
            super::inbox_cmds::run_cleanup_bounces(ctx, remove, days).await
        }
        Cmd::Help { .. } => Ok(()),
    }
}

/// Go's prompt(): print, read a line, trim. EOF or a read error (even mid-line) yields "".
pub fn prompt(input: &mut impl BufRead, message: &str) -> String {
    print!("{message}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(_) if line.ends_with('\n') => line.trim().to_string(),
        _ => String::new(),
    }
}

pub fn open_store(what: &str) -> Result<Store> {
    Store::new(history::default_db_path()).with_context(|| format!("failed to {what} history"))
}

fn run_init(ctx: &Ctx) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();

    println!("🔐 Eraser Configuration Setup");
    println!("==============================");
    println!();

    let mut cfg = config::Config::default();

    println!("📋 Personal Information (used in removal requests)");
    println!();

    let p = &mut cfg.profile;
    p.first_name = prompt(&mut input, "First name: ");
    p.last_name = prompt(&mut input, "Last name: ");
    p.email = prompt(&mut input, "Email address: ");
    p.address = prompt(&mut input, "Street address (optional): ");
    p.city = prompt(&mut input, "City (optional): ");
    p.state = prompt(&mut input, "State/Province (optional): ");
    p.zip_code = prompt(&mut input, "ZIP/Postal code (optional): ");
    p.country = prompt(&mut input, "Country (optional): ");
    p.phone = prompt(&mut input, "Phone number (optional): ");

    println!();
    println!("📧 Email Settings");
    println!();

    cfg.email.provider = "smtp".into();

    println!();
    println!("iCloud Mail SMTP Configuration:");
    println!(
        "  (Create an app-specific password at https://account.apple.com → Sign-In and Security)"
    );
    println!();
    let smtp = &mut cfg.email.smtp;
    smtp.host = "smtp.mail.me.com".into();
    smtp.port = 587;
    smtp.use_tls = true;
    smtp.username = prompt(&mut input, "  iCloud email address (e.g. you@icloud.com): ");
    smtp.password = prompt(
        &mut input,
        "  App-specific password (xxxx-xxxx-xxxx-xxxx): ",
    );
    // iCloud rejects mail whose From isn't one of your iCloud addresses
    cfg.email.from = cfg.email.smtp.username.clone();

    println!();
    println!("⚙️  Options");
    println!();

    let mut template_choice = prompt(
        &mut input,
        "Default template (auto/gdpr/ccpa/fadp/generic) [auto]: ",
    );
    if template_choice.is_empty() {
        template_choice = "auto".into();
    }
    // Not in Go (which saves any string): reject names the engine can't render.
    let available = template::Engine::new()
        .context("failed to initialize templates")?
        .available_templates();
    if template_choice != "auto" && !available.contains(&template_choice) {
        anyhow::bail!(
            "unknown template {template_choice:?} (available: auto, {})",
            available.join(", ")
        );
    }
    cfg.options.template = template_choice;
    cfg.options.rate_limit_ms = 2000;

    let config_path = ctx.config_path();
    config::save(&config_path, &cfg).context("failed to save config")?;

    println!();
    println!("✅ Configuration saved to: {}", config_path.display());
    println!();
    println!("Next steps:");
    println!("  1. Review and edit the config file if needed");
    println!("  2. Run 'eraser list-brokers' to see available brokers");
    println!("  3. Run 'eraser send --dry-run' to preview emails");
    println!("  4. Run 'eraser send' to send removal requests");
    Ok(())
}

async fn run_send(ctx: &Ctx, dry_run: bool) -> Result<()> {
    let mut cfg = ctx.load_config()?;
    cfg.validate().context("invalid config")?;
    if dry_run {
        cfg.options.dry_run = true;
    }
    let dry_run = cfg.options.dry_run;

    let db = ctx.load_brokers()?;
    let brokers = db.filter(&cfg.options.regions, &cfg.options.excluded_brokers);
    if brokers.is_empty() {
        println!("No brokers to process.");
        return Ok(());
    }

    let engine = template::Engine::new().context("failed to initialize templates")?;
    let sender = if dry_run {
        None
    } else {
        Some(email::new_sender(&cfg.email).context("failed to initialize email sender")?)
    };
    let store = open_store("initialize")?;

    if dry_run {
        println!("🔍 DRY RUN MODE - No emails will be sent");
        println!();
    }
    println!("📤 Processing {} brokers...", brokers.len());
    println!();

    let (mut success, mut failed) = (0, 0);
    for (i, b) in brokers.iter().enumerate() {
        println!("[{}/{}] {} ({})", i + 1, brokers.len(), b.name, b.email);

        let msg = match engine.render(&cfg.options.template, &cfg.profile, b) {
            Ok(m) => m,
            Err(e) => {
                println!("  ❌ Failed to render template: {e:#}");
                failed += 1;
                continue;
            }
        };

        let Some(sender) = &sender else {
            println!("  📧 Would send: {}", msg.subject);
            println!("  📍 To: {}", b.email);
            success += 1;
            continue;
        };

        let message = email::Message {
            to: b.email.clone(),
            from: cfg.email.from.clone(),
            subject: msg.subject,
            body: msg.body,
        };
        let result = sender.send(&message, Some(i)).await;

        let mut record = Record {
            broker_id: b.id.clone(),
            broker_name: b.name.clone(),
            email: b.email.clone(),
            template: msg.template.clone(),
            sent_at: Some(chrono::Local::now()),
            ..Default::default()
        };
        if result.success {
            record.status = Status::Sent;
            record.message_id = result.message_id;
            println!("  ✅ Sent successfully");
            success += 1;
        } else {
            let err = result.error.unwrap_or_default();
            record.status = Status::Failed;
            record.error = err.clone();
            println!("  ❌ Failed: {err}");
            failed += 1;
        }
        if let Err(e) = store.add(&mut record) {
            println!("  ⚠️  Failed to record history: {e:#}");
        }

        if i < brokers.len() - 1 {
            let ms = u64::try_from(cfg.options.rate_limit_ms).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    println!();
    println!("{RULE}");
    if dry_run {
        println!("📊 Dry run complete: {success} brokers would receive emails");
    } else {
        println!("📊 Complete: {success} sent, {failed} failed");
    }
    Ok(())
}

fn run_list_brokers(ctx: &Ctx) -> Result<()> {
    let db = ctx.load_brokers()?;
    println!("📋 Data Brokers ({} total)", db.brokers.len());
    println!("{RULE}");
    for b in &db.brokers {
        println!("\n{} [{}]", b.name, b.id);
        println!("  📧 {}", b.email);
        if !b.website.is_empty() {
            println!("  🌐 {}", b.website);
        }
        if !b.opt_out_url.is_empty() {
            println!("  🔗 Opt-out: {}", b.opt_out_url);
        }
        println!("  🌍 Region: {}", b.region);
        if !b.category.is_empty() {
            println!("  📁 Category: {}", b.category);
        }
    }
    Ok(())
}

fn run_status(limit: i64) -> Result<()> {
    let store = open_store("open")?;
    let (total, sent, failed) = store.get_stats().context("failed to get stats")?;
    let (monthly_sent, monthly_failed) = store
        .get_monthly_stats()
        .context("failed to get monthly stats")?;

    println!("📊 Eraser Statistics");
    println!("{RULE}");
    println!();
    println!("All Time:");
    println!("  Total requests: {total}");
    println!("  Sent: {sent}");
    println!("  Failed: {failed}");
    println!();
    println!("This Month:");
    println!("  Sent: {monthly_sent}");
    println!("  Failed: {monthly_failed}");

    let records = store
        .get_recent_requests(limit)
        .context("failed to get recent requests")?;
    if !records.is_empty() {
        println!();
        println!("📜 Recent Requests (last {limit})");
        println!("{RULE}");
        for r in &records {
            let icon = if r.status == Status::Failed {
                "❌"
            } else {
                "✅"
            };
            let at = r
                .sent_at
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "0001-01-01 00:00".into());
            println!("{icon} {at} - {} ({})", r.broker_name, r.template);
            if !r.error.is_empty() {
                println!("   Error: {}", r.error);
            }
        }
    }
    Ok(())
}

fn run_add_broker(ctx: &Ctx) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();

    println!("➕ Add New Data Broker");
    println!("{RULE}");
    println!();

    let mut b = broker::Broker::default();
    b.name = prompt(&mut input, "Broker name: ");
    b.id = b.name.replace(' ', "-").to_lowercase();
    b.email = prompt(&mut input, "Privacy/removal email: ");
    b.website = prompt(&mut input, "Website (optional): ");
    b.opt_out_url = prompt(&mut input, "Opt-out URL (optional): ");
    b.region = prompt(&mut input, "Region (us/eu/global): ");
    b.category = prompt(
        &mut input,
        "Category (people-search/marketing/background-check): ",
    );

    // Unlike the other commands, add-broker has no next-to-binary fallback (as in Go).
    let path = ctx
        .brokers
        .clone()
        .unwrap_or_else(|| PathBuf::from("data/brokers.yaml"));
    let mut db = if path.exists() {
        broker::load_from_file(&path).context("failed to load brokers")?
    } else {
        broker::BrokerDatabase::default()
    };
    let name = b.name.clone();
    db.add(b)?;
    db.save(&path).context("failed to save brokers")?;

    println!();
    println!("✅ Added {name} to broker database");
    Ok(())
}

fn run_pipeline_status() -> Result<()> {
    let store = open_store("open")?;

    println!("🔄 Pipeline Status");
    println!("{RULE}");
    println!();

    let stats = store
        .get_pipeline_stats()
        .context("failed to get pipeline stats")?;
    let n = |s: PipelineStatus| stats.get(&s).copied().unwrap_or(0);
    println!("📊 Pipeline Stage Breakdown:");
    println!(
        "  📧 Email sent:            {}",
        n(PipelineStatus::EmailSent)
    );
    println!(
        "  ⏳ Awaiting response:     {}",
        n(PipelineStatus::AwaitingResponse)
    );
    println!(
        "  📝 Form required:         {}",
        n(PipelineStatus::FormRequired)
    );
    println!(
        "  ✏️  Form filled:           {}",
        n(PipelineStatus::FormFilled)
    );
    println!(
        "  🤖 Awaiting CAPTCHA:      {}",
        n(PipelineStatus::AwaitingCaptcha)
    );
    println!(
        "  ✅ CAPTCHA solved:        {}",
        n(PipelineStatus::CaptchaSolved)
    );
    println!(
        "  🔗 Awaiting confirmation: {}",
        n(PipelineStatus::AwaitingConfirmation)
    );
    println!(
        "  ✅ Confirmed:             {}",
        n(PipelineStatus::Confirmed)
    );
    println!(
        "  ❌ Rejected:              {}",
        n(PipelineStatus::Rejected)
    );
    println!("  💥 Failed:                {}", n(PipelineStatus::Failed));

    match store.get_response_stats() {
        Err(e) => println!("\n⚠️  Could not get response stats: {e:#}"),
        Ok(resp) if !resp.is_empty() => {
            println!();
            println!("📬 Response Classification:");
            // Go iterates a map (random order); sorted here for stable output.
            let mut resp: Vec<_> = resp.into_iter().collect();
            resp.sort();
            for (kind, count) in resp {
                println!("  {kind}: {count}");
            }
        }
        Ok(_) => {}
    }

    match store.get_pending_task_stats() {
        Err(e) => println!("\n⚠️  Could not get task stats: {e:#}"),
        Ok((pending, completed, skipped)) if pending + completed + skipped > 0 => {
            println!();
            println!("📋 Pending Tasks:");
            println!("  ⏳ Pending:   {pending}");
            println!("  ✅ Completed: {completed}");
            println!("  ⏭️  Skipped:   {skipped}");
        }
        Ok(_) => {}
    }

    if let Ok(tasks) = store.get_pending_tasks(None, Some("pending"))
        && !tasks.is_empty()
    {
        println!();
        println!("🎯 Action Required:");
        for t in &tasks {
            println!("  • {} [{}] - {}", t.broker_name, t.task_type, t.form_url);
        }
    }
    Ok(())
}

/// Truncates to `max_len` bytes with "..." (Go slices bytes; we back off to a char boundary).
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len.saturating_sub(3);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

async fn run_serve(
    ctx: &Ctx,
    host: String,
    port: u16,
    public_origin: Option<String>,
) -> Result<()> {
    eraser::web::serve(eraser::web::ServeOptions {
        host,
        port,
        public_origin: public_origin.filter(|o| !o.is_empty()),
        config_path: ctx.config_path(),
        broker_path: ctx.broker_path(),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_trims_and_handles_eof() {
        let mut input = std::io::Cursor::new("  Jane \npartial");
        assert_eq!(prompt(&mut input, ""), "Jane");
        assert_eq!(prompt(&mut input, ""), "");
        assert_eq!(prompt(&mut input, ""), "");
    }

    #[test]
    fn truncate_matches_go() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdefg", 6), "abc...");
    }
}
