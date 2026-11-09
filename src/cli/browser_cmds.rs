//! `fill` and `confirm` (browser automation / confirmation links).

use super::commands::{Ctx, RULE, open_store, truncate};
use anyhow::{Context, Result, anyhow};
use eraser::browser::{self, Browser, ConfirmationHandler};
use eraser::history::{PendingTask, PipelineStatus, Store, TaskType};
use std::io::BufRead;
use std::sync::Arc;
use std::time::Duration;

struct Target {
    broker_id: String,
    url: String,
}

/// Picks targets from --url, --broker (first matching response) or --pending.
/// `Ok(None)` means --pending found nothing.
fn select_targets(
    store: &Store,
    response_type: &str,
    url: String,
    broker_id: String,
    pending: bool,
    url_of: fn(&eraser::history::BrokerResponse) -> &str,
    not_found: &str,
) -> Result<Option<Vec<Target>>> {
    if !url.is_empty() {
        return Ok(Some(vec![Target { broker_id, url }]));
    }
    if broker_id.is_empty() && !pending {
        return Err(anyhow!("please specify --url, --broker, or --pending"));
    }
    let responses = store
        .get_broker_responses(Some(response_type), false, 100)
        .context("failed to get broker responses")?;
    let mut targets = responses
        .iter()
        .filter(|r| !url_of(r).is_empty())
        .filter(|r| broker_id.is_empty() || r.broker_id == broker_id)
        .map(|r| Target {
            broker_id: r.broker_id.clone(),
            url: url_of(r).to_string(),
        });
    if !broker_id.is_empty() {
        return match targets.next() {
            Some(t) => Ok(Some(vec![t])),
            None => Err(anyhow!("{not_found}: {broker_id}")),
        };
    }
    let targets: Vec<Target> = targets.collect();
    Ok((!targets.is_empty()).then_some(targets))
}

#[allow(clippy::too_many_arguments)]
pub async fn run_fill(
    ctx: &Ctx,
    broker_id: String,
    form_url: String,
    headless: bool,
    auto_submit: bool,
    screenshot_dir: String,
    pending: bool,
    wait_for_captcha: bool,
) -> Result<()> {
    let cfg = ctx.load_config()?;
    let screenshot_dir = if screenshot_dir.is_empty() {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".eraser")
            .join("screenshots")
            .to_string_lossy()
            .into_owned()
    } else {
        screenshot_dir
    };
    let store = open_store("initialize")?;

    let mut bcfg = browser::default_config();
    bcfg.headless = headless;
    bcfg.screenshot_dir = screenshot_dir;
    if cfg.pipeline.browser_timeout_sec > 0 {
        bcfg.timeout = Duration::from_secs(cfg.pipeline.browser_timeout_sec as u64);
    }
    if wait_for_captcha {
        if headless {
            println!("⚠️  Warning: --wait requires --headless=false to be useful");
        }
        bcfg.wait_for_user = true;
        bcfg.timeout = Duration::from_secs(5 * 60); // Longer timeout when waiting for user
        bcfg.wait_callback = Some(Arc::new(|| {
            println!();
            println!("       ⏸️  CAPTCHA detected! Solve it in the browser window.");
            println!("       Press ENTER when done (or Ctrl+C to cancel)...");
            println!();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            if !line.ends_with('\n') {
                return Err(anyhow!("EOF"));
            }
            Ok(())
        }));
    }

    let mut b = Browser::new(bcfg, cfg.profile.clone())
        .await
        .context("failed to create browser")?;
    let result = fill_forms(&b, &cfg, &store, broker_id, form_url, auto_submit, pending).await;
    b.close().await;
    result
}

async fn fill_forms(
    b: &Browser,
    cfg: &eraser::config::Config,
    store: &Store,
    broker_id: String,
    form_url: String,
    auto_submit: bool,
    pending: bool,
) -> Result<()> {
    println!("🌐 Browser Automation");
    println!("{RULE}");
    println!();

    let Some(forms) = select_targets(
        store,
        "form_required",
        form_url,
        broker_id,
        pending,
        |r| &r.form_url,
        "no form URL found for broker",
    )?
    else {
        println!("✅ No pending forms to fill");
        return Ok(());
    };

    println!("📋 Forms to process: {}", forms.len());
    println!();

    for (i, form) in forms.iter().enumerate() {
        println!("[{}/{}] Processing {}", i + 1, forms.len(), form.url);
        if !form.broker_id.is_empty() {
            println!("       Broker: {}", form.broker_id);
        }

        let result = match b
            .navigate_and_fill(&form.url, &form.broker_id, auto_submit)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                println!("       ❌ Error: {e:#}");
                continue;
            }
        };

        if !result.fields_filled.is_empty() {
            println!(
                "       ✅ Filled fields: {}",
                result.fields_filled.join(", ")
            );
        }
        if !result.fields_missing.is_empty() {
            println!(
                "       ⚠️  Missing profile data for: {}",
                result.fields_missing.join(", ")
            );
        }

        if result.captcha_found {
            println!("       🤖 CAPTCHA detected: {}", result.captcha_type);
            let p = &cfg.profile;
            // Profile data for the CAPTCHA helper page (serde_json sorts keys like Go).
            let profile_json = serde_json::json!({
                "email": p.email,
                "firstName": p.first_name,
                "lastName": p.last_name,
                "phone": p.phone,
                "address": p.address,
                "city": p.city,
                "state": p.state,
                "zipCode": p.zip_code,
                "country": p.country,
            });
            let mut task = PendingTask {
                broker_id: form.broker_id.clone(),
                broker_name: form.broker_id.clone(), // Will need broker lookup for proper name
                task_type: TaskType::Captcha,
                form_url: form.url.clone(),
                browser_state: profile_json.to_string(),
                screenshot_path: result.screenshot_path.clone(),
                status: "pending".into(),
                ..Default::default()
            };
            match store.add_pending_task(&mut task) {
                Err(e) => println!("       ⚠️  Failed to create task: {e:#}"),
                Ok(()) => println!("       📝 Created CAPTCHA task for manual solving"),
            }
            let _ = store.update_pipeline_status(&form.broker_id, PipelineStatus::AwaitingCaptcha);
        } else if result.submit_attempted {
            println!("       📨 Form submitted!");
            let _ = store.update_pipeline_status(&form.broker_id, PipelineStatus::FormFilled);
        } else if result.success {
            println!("       ✅ Form filled (not submitted)");
            let _ = store.update_pipeline_status(&form.broker_id, PipelineStatus::FormFilled);
        }

        if !result.screenshot_path.is_empty() {
            println!("       📸 Screenshot: {}", result.screenshot_path);
        }
        println!();

        // Small delay between forms to be respectful
        if i < forms.len() - 1 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    println!("{RULE}");
    println!("✅ Processed {} forms", forms.len());
    Ok(())
}

/// Hostnames from broker websites, plus the bare domain for "www." hosts.
fn broker_domains(db: &eraser::broker::BrokerDatabase) -> Vec<String> {
    let mut domains = Vec::new();
    for b in db.brokers.iter().filter(|b| !b.website.is_empty()) {
        let d = b.website.strip_prefix("https://").unwrap_or(&b.website);
        let d = d.strip_prefix("http://").unwrap_or(d);
        let d = d.strip_suffix('/').unwrap_or(d);
        let d = d.split('/').next().unwrap_or(d);
        domains.push(d.to_string());
        if let Some(bare) = d.strip_prefix("www.") {
            domains.push(bare.to_string());
        }
    }
    domains
}

pub async fn run_confirm(
    ctx: &Ctx,
    confirm_url: String,
    broker_id: String,
    pending: bool,
    validate_domain: bool,
    dry_run: bool,
) -> Result<()> {
    let db = ctx.load_brokers()?;
    let store = open_store("initialize")?;
    let handler = ConfirmationHandler::new(&broker_domains(&db));

    println!("🔗 Confirmation Link Handler");
    println!("{RULE}");
    println!();

    let Some(links) = select_targets(
        &store,
        "confirmation_required",
        confirm_url,
        broker_id,
        pending,
        |r| &r.confirm_url,
        "no confirmation URL found for broker",
    )?
    else {
        println!("✅ No pending confirmation links");
        return Ok(());
    };

    println!("📋 Confirmation links to process: {}", links.len());
    if dry_run {
        println!("🔍 DRY RUN MODE - Links will not be clicked");
    }
    println!();

    let (mut success, mut failed) = (0, 0);
    for (i, link) in links.iter().enumerate() {
        println!("[{}/{}] Processing confirmation link", i + 1, links.len());
        if !link.broker_id.is_empty() {
            println!("       Broker: {}", link.broker_id);
        }
        println!("       URL: {}", truncate(&link.url, 60));

        if validate_domain {
            match handler.validate_domain(&link.url) {
                Err(e) => {
                    println!("       ❌ Invalid URL: {e:#}");
                    failed += 1;
                    continue;
                }
                Ok((false, domain)) => {
                    println!("       ⚠️  Domain {domain} is not a known broker domain");
                    println!("       Use --validate-domain=false to override");
                    failed += 1;
                    continue;
                }
                Ok((true, domain)) => println!("       ✓ Domain validated: {domain}"),
            }
        }

        if dry_run {
            println!("       📋 Would click this link (dry run)");
            success += 1;
            println!();
            continue;
        }

        // Domain already validated above
        let result = match handler.click_confirmation_link(&link.url, false).await {
            Ok(r) => r,
            Err(e) => {
                println!("       ❌ Error: {e:#}");
                failed += 1;
                continue;
            }
        };

        println!("       HTTP Status: {}", result.status_code);
        if result.redirect_path.len() > 1 {
            println!("       Redirects: {} hops", result.redirect_path.len() - 1);
        }
        if result.final_url != link.url {
            println!("       Final URL: {}", truncate(&result.final_url, 60));
        }

        let status = handler.extract_confirmation_status(&result);
        if result.success {
            println!("       ✅ {status}");
            success += 1;
            if !link.broker_id.is_empty() {
                let _ = store.update_pipeline_status(&link.broker_id, PipelineStatus::Confirmed);
            }
        } else {
            println!("       ⚠️  {status}");
            failed += 1;
            // Still update status to indicate we tried
            if !link.broker_id.is_empty() {
                let _ = store.update_pipeline_status(&link.broker_id, PipelineStatus::Failed);
            }
        }
        println!();

        if i < links.len() - 1 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    println!("{RULE}");
    if dry_run {
        println!(
            "📊 Dry run complete: {} links would be clicked",
            links.len()
        );
    } else {
        println!("📊 Complete: {success} confirmed, {failed} failed");
    }
    Ok(())
}
