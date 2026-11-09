//! Browser automation for opt-out forms (port of internal/browser).
//! CAPTCHAs are only detected, so the task can be handed to a human.

#[allow(clippy::module_inception)]
mod browser;
pub mod captcha;
pub mod confirm;
pub mod filler;

pub use browser::{
    Browser, BrowserConfig, FormResult, WaitCallback, chrome_executable, default_config,
    needs_no_sandbox,
};
pub use captcha::{CaptchaInfo, detect_captcha_from_html};
pub use confirm::{
    ConfirmationHandler, ConfirmationResult, extract_confirmation_status, is_success_response,
};
pub use filler::{FieldMapping, FillResult, FormField, FormFiller, escape_selector};

pub(crate) const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[cfg(test)]
mod tests;
