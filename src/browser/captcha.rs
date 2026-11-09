//! Port of internal/browser/captcha.go. Detection only: a detected CAPTCHA is handed to a human.

use chromiumoxide::Page;
use serde_json::Value;

use super::confirm::go_to_lower;

/// Information about a detected CAPTCHA.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptchaInfo {
    pub found: bool,
    pub r#type: String,
    pub frame_src: String,
    pub element_id: String,
    pub confidence: f64,
    pub description: String,
}

pub const CAPTCHA_TYPE_RECAPTCHA_V2: &str = "recaptcha_v2";
pub const CAPTCHA_TYPE_RECAPTCHA_V3: &str = "recaptcha_v3";
pub const CAPTCHA_TYPE_HCAPTCHA: &str = "hcaptcha";
pub const CAPTCHA_TYPE_TURNSTILE: &str = "cloudflare_turnstile";
pub const CAPTCHA_TYPE_FUNCAPTCHA: &str = "funcaptcha";
pub const CAPTCHA_TYPE_IMAGE_CAPTCHA: &str = "image_captcha";
pub const CAPTCHA_TYPE_TEXT_CAPTCHA: &str = "text_captcha";
pub const CAPTCHA_TYPE_CLOUDFLARE: &str = "cloudflare_challenge";
pub const CAPTCHA_TYPE_UNKNOWN: &str = "unknown";

const JS_RECAPTCHA_V2: &str = r#"(function() {
		var iframe = document.querySelector('iframe[src*="recaptcha"]');
		if (iframe) {
			return {found: true, src: iframe.src, type: 'iframe'};
		}
		var div = document.querySelector('.g-recaptcha, [data-sitekey]');
		if (div) {
			return {found: true, id: div.id || '', type: 'div'};
		}
		if (typeof grecaptcha !== 'undefined') {
			return {found: true, type: 'script'};
		}
		return {found: false};
	})()"#;

const JS_RECAPTCHA_V3: &str = r#"(function() {
		var scripts = document.querySelectorAll('script[src*="recaptcha"]');
		for (var i = 0; i < scripts.length; i++) {
			if (scripts[i].src.includes('render=')) {
				return {found: true, src: scripts[i].src};
			}
		}
		scripts = document.querySelectorAll('script[src*="enterprise.js"]');
		if (scripts.length > 0) {
			return {found: true, type: 'enterprise'};
		}
		return {found: false};
	})()"#;

const JS_HCAPTCHA: &str = r#"(function() {
		var iframe = document.querySelector('iframe[src*="hcaptcha"]');
		if (iframe) {
			return {found: true, src: iframe.src};
		}
		var div = document.querySelector('.h-captcha, [data-hcaptcha-sitekey]');
		if (div) {
			return {found: true, id: div.id || ''};
		}
		if (typeof hcaptcha !== 'undefined') {
			return {found: true, type: 'script'};
		}
		return {found: false};
	})()"#;

const JS_TURNSTILE: &str = r#"(function() {
		var iframe = document.querySelector('iframe[src*="challenges.cloudflare.com"]');
		if (iframe) {
			return {found: true, src: iframe.src};
		}
		var div = document.querySelector('.cf-turnstile, [data-turnstile-sitekey]');
		if (div) {
			return {found: true, id: div.id || ''};
		}
		return {found: false};
	})()"#;

const JS_FUNCAPTCHA: &str = r#"(function() {
		var iframe = document.querySelector('iframe[src*="funcaptcha"], iframe[src*="arkoselabs"]');
		if (iframe) {
			return {found: true, src: iframe.src};
		}
		var div = document.querySelector('#FunCaptcha, [data-callback]');
		if (div && div.id && div.id.toLowerCase().includes('funcaptcha')) {
			return {found: true, id: div.id};
		}
		return {found: false};
	})()"#;

const JS_CLOUDFLARE: &str = r#"(function() {
		var title = document.title.toLowerCase();
		if (title.includes('just a moment') || title.includes('checking your browser')) {
			return {found: true, type: 'title'};
		}
		var form = document.querySelector('form#challenge-form');
		if (form) {
			return {found: true, type: 'form'};
		}
		var rayId = document.querySelector('.ray-id, [data-ray]');
		var body = document.body.innerText.toLowerCase();
		if (rayId || (body.includes('ray id') && body.includes('cloudflare'))) {
			return {found: true, type: 'rayid'};
		}
		return {found: false};
	})()"#;

const JS_GENERIC: &str = r#"(function() {
		var body = document.body.innerHTML.toLowerCase();
		var text = document.body.innerText.toLowerCase();
		var keywords = ['captcha', 'verification code', 'security code', 'enter the code',
			'type the characters', 'verify you are human', 'prove you are human',
			'i am not a robot', 'human verification'];
		for (var i = 0; i < keywords.length; i++) {
			if (text.includes(keywords[i])) {
				var img = document.querySelector('img[src*="captcha"], img[alt*="captcha"], .captcha-image');
				var input = document.querySelector('input[name*="captcha"], input[id*="captcha"], input[placeholder*="captcha" i]');
				if (img || input) {
					return {found: true, keyword: keywords[i], hasImage: !!img, hasInput: !!input};
				}
			}
		}
		var captchaImg = document.querySelector('img[src*="captcha"], img[alt*="captcha"]');
		if (captchaImg) {
			return {found: true, type: 'image', src: captchaImg.src};
		}
		return {found: false};
	})()"#;

fn info(r#type: &str, confidence: f64, description: &str) -> CaptchaInfo {
    CaptchaInfo {
        found: true,
        r#type: r#type.into(),
        confidence,
        description: description.into(),
        ..Default::default()
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Runs a detector script; `None` on evaluation error or `found: false`.
async fn run(page: &Page, js: &str) -> Option<Value> {
    let v: Value = page.evaluate(js).await.ok()?.into_value().ok()?;
    (v.get("found").and_then(Value::as_bool) == Some(true)).then_some(v)
}

/// Checks the live page for CAPTCHA types, in the same order as Go.
pub async fn detect_captcha(page: &Page) -> CaptchaInfo {
    if let Some(v) = run(page, JS_RECAPTCHA_V2).await {
        let mut i = info(
            CAPTCHA_TYPE_RECAPTCHA_V2,
            0.95,
            "Google reCAPTCHA v2 detected",
        );
        i.frame_src = str_field(&v, "src").unwrap_or_default();
        i.element_id = str_field(&v, "id").unwrap_or_default();
        return i;
    }
    if run(page, JS_RECAPTCHA_V3).await.is_some() {
        return info(
            CAPTCHA_TYPE_RECAPTCHA_V3,
            0.85,
            "Google reCAPTCHA v3 (invisible) detected - may not require interaction",
        );
    }
    if let Some(v) = run(page, JS_HCAPTCHA).await {
        let mut i = info(CAPTCHA_TYPE_HCAPTCHA, 0.95, "hCaptcha detected");
        i.frame_src = str_field(&v, "src").unwrap_or_default();
        return i;
    }
    if run(page, JS_TURNSTILE).await.is_some() {
        return info(
            CAPTCHA_TYPE_TURNSTILE,
            0.95,
            "Cloudflare Turnstile detected",
        );
    }
    if run(page, JS_FUNCAPTCHA).await.is_some() {
        return info(
            CAPTCHA_TYPE_FUNCAPTCHA,
            0.90,
            "Arkose Labs FunCaptcha detected",
        );
    }
    if run(page, JS_CLOUDFLARE).await.is_some() {
        return info(
            CAPTCHA_TYPE_CLOUDFLARE,
            0.90,
            "Cloudflare challenge page detected - wait or solve challenge",
        );
    }
    if let Some(v) = run(page, JS_GENERIC).await {
        let r#type = if str_field(&v, "type").as_deref() == Some("image") {
            CAPTCHA_TYPE_IMAGE_CAPTCHA
        } else if v.get("hasInput").and_then(Value::as_bool) == Some(true) {
            CAPTCHA_TYPE_TEXT_CAPTCHA
        } else {
            CAPTCHA_TYPE_UNKNOWN
        };
        let description = match str_field(&v, "keyword") {
            Some(k) => format!("CAPTCHA detected: {k}"),
            None => "Generic CAPTCHA detected".into(),
        };
        return info(r#type, 0.75, &description);
    }
    CaptchaInfo::default()
}

impl CaptchaInfo {
    /// True if the CAPTCHA requires human intervention (reCAPTCHA v3 is invisible).
    pub fn is_captcha_blocking(&self) -> bool {
        self.found && self.r#type != CAPTCHA_TYPE_RECAPTCHA_V3
    }

    /// Human-readable description of what the human needs to do.
    pub fn get_captcha_description(&self) -> String {
        if !self.found {
            return "No CAPTCHA detected".into();
        }
        match self.r#type.as_str() {
            CAPTCHA_TYPE_RECAPTCHA_V2 => {
                "Google reCAPTCHA v2 - Click the checkbox and/or solve image puzzles"
            }
            CAPTCHA_TYPE_RECAPTCHA_V3 => "Google reCAPTCHA v3 - Usually invisible, may auto-pass",
            CAPTCHA_TYPE_HCAPTCHA => "hCaptcha - Select images matching the description",
            CAPTCHA_TYPE_TURNSTILE => {
                "Cloudflare Turnstile - Usually auto-passes after brief check"
            }
            CAPTCHA_TYPE_FUNCAPTCHA => "FunCaptcha - Complete interactive puzzles",
            CAPTCHA_TYPE_IMAGE_CAPTCHA => "Image CAPTCHA - Type the characters shown in the image",
            CAPTCHA_TYPE_TEXT_CAPTCHA => "Text CAPTCHA - Enter the verification code",
            CAPTCHA_TYPE_CLOUDFLARE => "Cloudflare Challenge - Wait or complete verification",
            CAPTCHA_TYPE_UNKNOWN => "Unknown CAPTCHA type - Manual inspection required",
            _ => return self.description.clone(),
        }
        .into()
    }
}

/// Checks HTML content for CAPTCHA indicators.
pub fn detect_captcha_from_html(html: &str) -> CaptchaInfo {
    let html = go_to_lower(html);
    if html.contains("recaptcha") || html.contains("g-recaptcha") {
        return info(
            CAPTCHA_TYPE_RECAPTCHA_V2,
            0.85,
            "reCAPTCHA detected in HTML",
        );
    }
    if html.contains("hcaptcha") || html.contains("h-captcha") {
        return info(CAPTCHA_TYPE_HCAPTCHA, 0.85, "hCaptcha detected in HTML");
    }
    if html.contains("cf-turnstile") || html.contains("challenges.cloudflare.com") {
        return info(
            CAPTCHA_TYPE_TURNSTILE,
            0.85,
            "Cloudflare Turnstile detected in HTML",
        );
    }
    for keyword in [
        "captcha",
        "verification code",
        "security code",
        "prove you are human",
    ] {
        if html.contains(keyword) {
            return info(
                CAPTCHA_TYPE_UNKNOWN,
                0.60,
                &format!("Possible CAPTCHA detected: {keyword}"),
            );
        }
    }
    CaptchaInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_detection_order() {
        assert!(!detect_captcha_from_html("<form></form>").found);
        assert_eq!(
            detect_captcha_from_html("<div class=h-captcha></div><div class=g-recaptcha>").r#type,
            CAPTCHA_TYPE_RECAPTCHA_V2
        );
        let c = detect_captcha_from_html("Enter Security Code");
        assert_eq!(
            (c.r#type.as_str(), c.confidence),
            (CAPTCHA_TYPE_UNKNOWN, 0.60)
        );
        assert_eq!(c.description, "Possible CAPTCHA detected: security code");
    }

    #[test]
    fn blocking_and_descriptions() {
        assert!(!CaptchaInfo::default().is_captcha_blocking());
        assert_eq!(
            CaptchaInfo::default().get_captcha_description(),
            "No CAPTCHA detected"
        );
        let v3 = info(CAPTCHA_TYPE_RECAPTCHA_V3, 0.85, "x");
        assert!(!v3.is_captcha_blocking());
        let other = info("weird", 0.1, "custom");
        assert!(other.is_captcha_blocking());
        assert_eq!(other.get_captcha_description(), "custom");
    }
}
