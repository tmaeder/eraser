//! Port of internal/browser/browser.go: headless Chrome automation via chromiumoxide.

use anyhow::{Context, Result, anyhow, bail};
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser as CdpBrowser, BrowserConfig as CdpConfig};
use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, EnableParams};
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::DEFAULT_USER_AGENT;
use super::captcha::detect_captcha;
use super::filler::{FillResult, FormFiller};
use crate::config::Profile;

/// Called when waiting for the user (e.g. prompt in terminal); an error cancels.
pub type WaitCallback = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// Browser automation settings.
#[derive(Clone)]
pub struct BrowserConfig {
    pub headless: bool,
    pub timeout: Duration,
    pub screenshot_dir: String,
    pub user_agent: String,
    pub window_width: u32,
    pub window_height: u32,
    /// If true, pause when a CAPTCHA is detected so a human can solve it.
    pub wait_for_user: bool,
    pub wait_callback: Option<WaitCallback>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            headless: true,
            timeout: Duration::from_secs(60), // many broker sites are slow
            screenshot_dir: String::new(),
            user_agent: DEFAULT_USER_AGENT.into(),
            window_width: 1920,
            window_height: 1080,
            wait_for_user: false,
            wait_callback: None,
        }
    }
}

/// Sensible default browser settings.
pub fn default_config() -> BrowserConfig {
    BrowserConfig::default()
}

/// Outcome of a form fill attempt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormResult {
    pub success: bool,
    pub url: String,
    pub broker_id: String,
    pub fields_filled: Vec<String>,
    pub fields_missing: Vec<String>,
    pub captcha_found: bool,
    pub captcha_type: String,
    pub screenshot_path: String,
    pub error_message: String,
    pub submit_attempted: bool,
}

/// One Chrome instance with a single tab, like the Go chromedp context.
pub struct Browser {
    browser: CdpBrowser,
    handler: JoinHandle<()>,
    page: Page,
    config: BrowserConfig,
    profile: Profile,
    user_data_dir: PathBuf,
}

/// `CHROME_PATH` if set, else None (chromiumoxide auto-detects).
pub fn chrome_executable() -> Option<PathBuf> {
    std::env::var_os("CHROME_PATH")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// Chrome's sandbox can't run as root; otherwise keep it unless explicitly disabled.
pub fn needs_no_sandbox() -> bool {
    #[cfg(unix)]
    let root = unsafe { libc::geteuid() } == 0;
    #[cfg(not(unix))]
    let root = false;
    root || std::env::var("ERASER_CHROME_NO_SANDBOX").as_deref() == Ok("1")
}

const SUBMIT_SELECTORS: &[&str] = &[
    "button[type='submit']",
    "input[type='submit']",
    // :contains() is not valid CSS; querySelector throws and these are skipped, as in Go.
    "button:contains('Submit')",
    "button:contains('Remove')",
    "button:contains('Opt Out')",
    "button:contains('Delete')",
    "button:contains('Request')",
    ".submit-button",
    "#submit",
    "#submit-btn",
];

impl Browser {
    /// Launches Chrome with the given settings.
    pub async fn new(cfg: BrowserConfig, profile: Profile) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let user_data_dir =
            std::env::temp_dir().join(format!("eraser-chrome-{}-{nanos}", std::process::id()));

        let mut b = CdpConfig::builder()
            .user_data_dir(&user_data_dir)
            .window_size(cfg.window_width, cfg.window_height)
            .viewport(None)
            .request_timeout(cfg.timeout)
            .arg("--no-default-browser-check")
            .arg("--disable-gpu")
            .arg(("user-agent", cfg.user_agent.as_str()));
        if !cfg.headless {
            b = b.with_head();
        }
        if let Some(path) = chrome_executable() {
            b = b.chrome_executable(path);
        }
        if needs_no_sandbox() {
            b = b.no_sandbox();
        }
        let (browser, mut handler) = CdpBrowser::launch(b.build().map_err(|e| anyhow!(e))?)
            .await
            .context("failed to launch Chrome")?;
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let page = browser.new_page("about:blank").await?;

        Ok(Self {
            browser,
            handler,
            page,
            config: cfg,
            profile,
            user_data_dir,
        })
    }

    /// Shuts Chrome down and removes its temporary profile.
    pub async fn close(&mut self) {
        let _ = self.browser.close().await;
        let _ = self.browser.wait().await;
        self.handler.abort();
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }

    /// The tab used for automation.
    pub fn page(&self) -> &Page {
        &self.page
    }

    /// Navigates to `url` and attempts to fill any opt-out form, within the configured timeout.
    pub async fn navigate_and_fill(
        &self,
        url: &str,
        broker_id: &str,
        auto_submit: bool,
    ) -> Result<FormResult> {
        let mut result = FormResult {
            url: url.into(),
            broker_id: broker_id.into(),
            ..Default::default()
        };
        match tokio::time::timeout(
            self.config.timeout,
            self.fill_flow(&mut result, auto_submit),
        )
        .await
        {
            Ok(r) => r.map(|_| result),
            Err(_) => bail!("timed out after {:?}", self.config.timeout),
        }
    }

    async fn fill_flow(&self, result: &mut FormResult, auto_submit: bool) -> Result<()> {
        let page = &self.page;
        if let Err(e) = page.goto(result.url.as_str()).await {
            result.error_message = format!("navigation failed: {e}");
            bail!(result.error_message.clone());
        }
        if let Err(e) = self.wait_for_navigation().await {
            result.error_message = format!("page load failed: {e}");
            bail!(result.error_message.clone());
        }
        sleep(Duration::from_secs(2)).await; // dynamic content

        // PHASE 1: blocking CAPTCHA gate (e.g. Cloudflare challenge before the form)
        let blocking = detect_captcha(page).await;
        if blocking.is_captcha_blocking() {
            result.captcha_found = true;
            result.captcha_type = blocking.r#type.clone();
            if !self.config.screenshot_dir.is_empty() {
                result.screenshot_path = self
                    .take_screenshot(&result.broker_id, "captcha_gate")
                    .await
                    .unwrap_or_default();
            }
            match self.waiter() {
                Some(wait) => {
                    println!(
                        "⚠️  Blocking CAPTCHA detected: {}",
                        blocking.get_captcha_description()
                    );
                    println!("   Please solve the CAPTCHA in the browser window...");
                    if let Err(e) = wait() {
                        result.error_message = format!("user cancelled: {e}");
                        bail!(result.error_message.clone());
                    }
                    sleep(Duration::from_secs(2)).await;
                }
                None => {
                    result.error_message = format!(
                        "Blocking CAPTCHA detected: {} - use --wait flag to solve manually",
                        blocking.r#type
                    );
                    return Ok(());
                }
            }
        }

        // PHASE 2: fill form fields
        let FillResult {
            filled_fields,
            missing_fields,
            ..
        } = FormFiller::new(self.profile.clone()).fill(page).await;
        result.fields_filled = filled_fields;
        result.fields_missing = missing_fields;
        sleep(Duration::from_secs(1)).await;

        // PHASE 3: form-level CAPTCHA (e.g. reCAPTCHA on the form itself)
        let form_captcha = detect_captcha(page).await;
        if form_captcha.is_captcha_blocking() {
            result.captcha_found = true;
            result.captcha_type = form_captcha.r#type.clone();
            if !self.config.screenshot_dir.is_empty() {
                result.screenshot_path = self
                    .take_screenshot(&result.broker_id, "filled_captcha")
                    .await
                    .unwrap_or_default();
            }
            if let Some(wait) = self.waiter() {
                println!(
                    "⚠️  Form CAPTCHA detected: {}",
                    form_captcha.get_captcha_description()
                );
                println!("   Form has been pre-filled. Please solve the CAPTCHA...");
                if let Err(e) = wait() {
                    result.error_message = format!("user cancelled: {e}");
                    bail!(result.error_message.clone());
                }
                if auto_submit {
                    self.submit_and_record(result, "submit failed after CAPTCHA")
                        .await;
                }
                return Ok(());
            }
            result.error_message = format!(
                "CAPTCHA detected: {} - form filled, solve CAPTCHA and submit",
                form_captcha.r#type
            );
            return Ok(());
        }

        if !self.config.screenshot_dir.is_empty() {
            result.screenshot_path = self
                .take_screenshot(&result.broker_id, "filled")
                .await
                .unwrap_or_default();
        }

        if auto_submit && !result.captcha_found && !result.fields_filled.is_empty() {
            self.submit_and_record(result, "submit failed").await;
        } else if !result.fields_filled.is_empty() {
            result.success = true;
        }
        Ok(())
    }

    fn waiter(&self) -> Option<&WaitCallback> {
        self.config
            .wait_callback
            .as_ref()
            .filter(|_| self.config.wait_for_user)
    }

    async fn submit_and_record(&self, result: &mut FormResult, err_prefix: &str) {
        match self.submit_form().await {
            Err(e) => result.error_message = format!("{err_prefix}: {e}"),
            Ok(()) => {
                result.submit_attempted = true;
                result.success = true;
                sleep(Duration::from_secs(2)).await;
                if !self.config.screenshot_dir.is_empty() {
                    let _ = self.take_screenshot(&result.broker_id, "submitted").await;
                }
            }
        }
    }

    /// Clicks the first matching submit control, else presses Enter.
    async fn submit_form(&self) -> Result<()> {
        for selector in SUBMIT_SELECTORS {
            let js = format!(r#"document.querySelector("{selector}") !== null"#);
            let exists = match self.page.evaluate(js).await {
                Ok(r) => r.into_value::<bool>().unwrap_or(false),
                Err(_) => false,
            };
            if exists
                && let Ok(el) = self.page.find_element(*selector).await
                && el.click().await.is_ok()
            {
                return Ok(());
            }
        }
        // Press Enter on the focused element (chromedp KeyEvent("\r")).
        for kind in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let mut b = DispatchKeyEventParams::builder()
                .r#type(kind.clone())
                .key("Enter")
                .code("Enter")
                .windows_virtual_key_code(13)
                .native_virtual_key_code(13);
            if kind == DispatchKeyEventType::KeyDown {
                b = b.text("\r");
            }
            self.page
                .execute(b.build().map_err(|e| anyhow!(e))?)
                .await
                .map_err(|_| anyhow!("could not find or click submit button"))?;
        }
        Ok(())
    }

    /// Full-page screenshot into `screenshot_dir`; returns the file name (not the full path).
    /// Like chromedp's FullScreenshot(_, 90) this is JPEG data despite the .png extension.
    async fn take_screenshot(&self, broker_id: &str, suffix: &str) -> Result<String> {
        let buf = self
            .page
            .screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Jpeg)
                    .quality(90)
                    .full_page(true)
                    .build(),
            )
            .await?;
        let dir = PathBuf::from(&self.config.screenshot_dir);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o755)
            .create(&dir)?;
        #[cfg(not(unix))]
        std::fs::create_dir_all(&dir)?;
        let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let filename = format!("{broker_id}_{suffix}_{secs}.png");
        crate::config::write_with_mode(&dir.join(&filename), &buf, 0o644)?;
        Ok(filename)
    }

    /// Current page HTML.
    pub async fn get_page_html(&self) -> Result<String> {
        Ok(self.page.content().await?)
    }

    /// Current page title.
    pub async fn get_page_title(&self) -> Result<String> {
        Ok(self.page.get_title().await?.unwrap_or_default())
    }

    /// Waits until the document has a body (chromedp WaitReady("body")).
    pub async fn wait_for_navigation(&self) -> Result<()> {
        loop {
            if self.page.find_element("body").await.is_ok() {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Enables page lifecycle events.
    pub async fn enable_page_events(&self) -> Result<()> {
        self.page.execute(EnableParams::default()).await?;
        Ok(())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.handler.abort();
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }
}
