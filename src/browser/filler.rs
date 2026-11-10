//! Form field detection and auto-filling.

use anyhow::{Result, anyhow};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Profile;

/// Detects and fills form fields with profile data.
#[derive(Debug, Clone)]
pub struct FormFiller {
    profile: Profile,
}

/// Outcome of form filling.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillResult {
    pub filled_fields: Vec<String>,
    pub missing_fields: Vec<String>,
    pub errors: Vec<String>,
}

/// Maps a field type to a profile value plus the selectors/patterns used to find it.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldMapping {
    pub field_type: &'static str,
    pub profile_value: String,
    pub selectors: &'static [&'static str],
    /// Patterns to match in name/id/placeholder/label
    pub patterns: &'static [&'static str],
}

/// A detected form field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub tag: String,
    pub r#type: String,
    pub name: String,
    pub id: String,
    pub placeholder: String,
    pub label: String,
    pub required: bool,
    pub visible: bool,
}

impl FormFiller {
    pub fn new(profile: Profile) -> Self {
        Self { profile }
    }

    /// Detects and fills form fields on the page.
    pub async fn fill(&self, page: &Page) -> FillResult {
        let mut result = FillResult::default();
        for mapping in self.get_field_mappings() {
            if mapping.profile_value.is_empty() {
                result.missing_fields.push(mapping.field_type.into());
                continue;
            }
            if try_fill_field(page, &mapping).await {
                result.filled_fields.push(mapping.field_type.into());
            }
        }
        result
    }

    /// Mappings for all profile fields, in fill order.
    pub fn get_field_mappings(&self) -> Vec<FieldMapping> {
        let p = &self.profile;
        let m = |field_type, value: &str, selectors, patterns| FieldMapping {
            field_type,
            profile_value: value.to_string(),
            selectors,
            patterns,
        };
        vec![
            m(
                "email",
                &p.email,
                &[
                    "input[type='email']",
                    "input[name='email']",
                    "input[id='email']",
                    "input[name*='email']",
                    "input[id*='email']",
                    "input[placeholder*='email' i]",
                    "input[autocomplete='email']",
                ],
                &["email", "e-mail", "e_mail"],
            ),
            m(
                "firstName",
                &p.first_name,
                &[
                    "input[name='firstName']",
                    "input[name='first_name']",
                    "input[name='fname']",
                    "input[id='firstName']",
                    "input[id='first_name']",
                    "input[name*='first']",
                    "input[id*='first']",
                    "input[placeholder*='first name' i]",
                    "input[autocomplete='given-name']",
                ],
                &["first", "fname", "given"],
            ),
            m(
                "lastName",
                &p.last_name,
                &[
                    "input[name='lastName']",
                    "input[name='last_name']",
                    "input[name='lname']",
                    "input[id='lastName']",
                    "input[id='last_name']",
                    "input[name*='last']",
                    "input[id*='last']",
                    "input[placeholder*='last name' i]",
                    "input[autocomplete='family-name']",
                ],
                &["last", "lname", "family", "surname"],
            ),
            m(
                "fullName",
                &format!("{} {}", p.first_name, p.last_name),
                &[
                    "input[name='name']",
                    "input[name='fullName']",
                    "input[name='full_name']",
                    "input[id='name']",
                    "input[id='fullName']",
                    "input[placeholder*='full name' i]",
                    "input[placeholder*='your name' i]",
                    "input[autocomplete='name']",
                ],
                &["fullname", "full_name", "your_name"],
            ),
            m(
                "phone",
                &p.phone,
                &[
                    "input[type='tel']",
                    "input[name='phone']",
                    "input[name='telephone']",
                    "input[name='mobile']",
                    "input[id='phone']",
                    "input[name*='phone']",
                    "input[id*='phone']",
                    "input[placeholder*='phone' i]",
                    "input[autocomplete='tel']",
                ],
                &["phone", "tel", "mobile", "cell"],
            ),
            m(
                "address",
                &p.address,
                &[
                    "input[name='address']",
                    "input[name='street']",
                    "input[name='address1']",
                    "input[name='streetAddress']",
                    "input[id='address']",
                    "input[name*='address']",
                    "input[name*='street']",
                    "input[placeholder*='address' i]",
                    "input[placeholder*='street' i]",
                    "input[autocomplete='street-address']",
                    "input[autocomplete='address-line1']",
                ],
                &["address", "street", "addr"],
            ),
            m(
                "city",
                &p.city,
                &[
                    "input[name='city']",
                    "input[id='city']",
                    "input[name*='city']",
                    "input[placeholder*='city' i]",
                    "input[autocomplete='address-level2']",
                ],
                &["city", "town", "locality"],
            ),
            m(
                "state",
                &p.state,
                &[
                    "input[name='state']",
                    "input[id='state']",
                    "input[name*='state']",
                    "input[placeholder*='state' i]",
                    "select[name='state']",
                    "select[id='state']",
                    "input[autocomplete='address-level1']",
                ],
                &["state", "province", "region"],
            ),
            m(
                "zipCode",
                &p.zip_code,
                &[
                    "input[name='zip']",
                    "input[name='zipCode']",
                    "input[name='zip_code']",
                    "input[name='postalCode']",
                    "input[name='postal_code']",
                    "input[id='zip']",
                    "input[name*='zip']",
                    "input[name*='postal']",
                    "input[placeholder*='zip' i]",
                    "input[placeholder*='postal' i]",
                    "input[autocomplete='postal-code']",
                ],
                &["zip", "postal", "postcode"],
            ),
            m(
                "country",
                &p.country,
                &[
                    "input[name='country']",
                    "input[id='country']",
                    "select[name='country']",
                    "select[id='country']",
                    "input[autocomplete='country-name']",
                ],
                &["country", "nation"],
            ),
            m(
                "dateOfBirth",
                &p.date_of_birth,
                &[
                    "input[name='dob']",
                    "input[name='dateOfBirth']",
                    "input[name='date_of_birth']",
                    "input[name='birthdate']",
                    "input[name='birth_date']",
                    "input[type='date']",
                    "input[id='dob']",
                    "input[placeholder*='birth' i]",
                    "input[autocomplete='bday']",
                ],
                &["dob", "birth", "bday"],
            ),
        ]
    }

    /// Lists all non-hidden, non-button form fields on the page.
    pub async fn detect_form_fields(&self, page: &Page) -> Result<Vec<FormField>> {
        const JS: &str = r#"(function() {
		var fields = [];
		var inputs = document.querySelectorAll('input, select, textarea');
		for (var i = 0; i < inputs.length; i++) {
			var el = inputs[i];
			if (el.type === 'hidden' || el.type === 'submit' || el.type === 'button') continue;
			var label = '';
			if (el.id) {
				var labelEl = document.querySelector('label[for="' + el.id + '"]');
				if (labelEl) label = labelEl.textContent.trim();
			}
			fields.push({
				tag: el.tagName.toLowerCase(),
				type: el.type || '',
				name: el.name || '',
				id: el.id || '',
				placeholder: el.placeholder || '',
				label: label,
				required: el.required,
				visible: el.offsetParent !== null
			});
		}
		return fields;
	})()"#;
        Ok(page.evaluate(JS).await?.into_value()?)
    }
}

async fn eval_bool(page: &Page, js: &str) -> bool {
    match page.evaluate(js).await {
        Ok(r) => r.into_value::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}

/// Strategy 1: exact selectors; strategy 2: pattern match over all inputs.
async fn try_fill_field(page: &Page, mapping: &FieldMapping) -> bool {
    for selector in mapping.selectors {
        if fill_selector(page, selector, &mapping.profile_value).await {
            return true;
        }
    }
    fill_by_pattern(page, mapping).await
}

/// JS returning whether `selector` matches a visible element.
pub fn visibility_js(selector: &str) -> String {
    format!(
        r#"(function() {{
			var el = document.querySelector("{}");
			return el !== null && el.offsetParent !== null;
		}})()"#,
        escape_selector(selector)
    )
}

/// Fills the element matching `selector` if it exists and is visible.
async fn fill_selector(page: &Page, selector: &str, value: &str) -> bool {
    if !eval_bool(page, &visibility_js(selector)).await {
        return false;
    }
    clear_and_send_keys(page, selector, value).await.is_ok()
}

/// chromedp Clear + SendKeys: clear the value attribute (input) or text child (textarea),
/// erroring for other elements like chromedp does, then focus and type the value.
async fn clear_and_send_keys(page: &Page, selector: &str, value: &str) -> Result<()> {
    let el = page.find_element(selector).await?;
    let cleared = el
        .call_js_fn(
            r"function() {
				if (this.tagName === 'INPUT') { this.setAttribute('value', ''); return true; }
				if (this.tagName === 'TEXTAREA') {
					for (var c of this.childNodes) { if (c.nodeType === 3) { c.nodeValue = ''; return true; } }
				}
				return false;
			}",
            false,
        )
        .await?;
    if cleared.result.value != Some(Value::Bool(true)) {
        return Err(anyhow!("selector {selector:?} cannot be cleared"));
    }
    el.focus().await?;
    send_keys(page, value).await
}

/// Types text into the focused element. Unlike chromiumoxide's `type_str`, this handles any
/// character (e.g. umlauts), like chromedp's key encoder.
pub(crate) async fn send_keys(page: &Page, text: &str) -> Result<()> {
    for c in text.chars() {
        let s = c.to_string();
        for kind in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let mut b = DispatchKeyEventParams::builder()
                .r#type(kind.clone())
                .key(&s);
            if kind == DispatchKeyEventType::KeyDown {
                b = b.text(&s);
            }
            page.execute(b.build().map_err(|e| anyhow!(e))?).await?;
        }
    }
    Ok(())
}

/// JS that finds the first visible field whose name/id/placeholder/label contains a pattern
/// and returns a selector for it.
pub fn pattern_js(patterns: &[&str]) -> String {
    format!(
        r#"(function() {{
		var patterns = {};
		var inputs = document.querySelectorAll('input, select, textarea');
		for (var i = 0; i < inputs.length; i++) {{
			var el = inputs[i];
			var name = (el.name || '').toLowerCase();
			var id = (el.id || '').toLowerCase();
			var placeholder = (el.placeholder || '').toLowerCase();
			var label = '';
			if (el.id) {{
				var labelEl = document.querySelector('label[for="' + el.id + '"]');
				if (labelEl) label = labelEl.textContent.toLowerCase();
			}}
			for (var j = 0; j < patterns.length; j++) {{
				var p = patterns[j];
				if (name.includes(p) || id.includes(p) || placeholder.includes(p) || label.includes(p)) {{
					if (el.offsetParent !== null) {{
						return {{found: true, selector: el.tagName.toLowerCase() + (el.id ? '#' + el.id : (el.name ? '[name="' + el.name + '"]' : ''))}};
					}}
				}}
			}}
		}}
		return {{found: false}};
	}})()"#,
        Value::from(patterns)
    )
}

async fn fill_by_pattern(page: &Page, mapping: &FieldMapping) -> bool {
    let Ok(res) = page.evaluate(pattern_js(mapping.patterns)).await else {
        return false;
    };
    let Ok(v) = res.into_value::<Value>() else {
        return false;
    };
    if v.get("found").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    match v.get("selector").and_then(Value::as_str) {
        Some(sel) if !sel.is_empty() => fill_selector(page, sel, &mapping.profile_value).await,
        _ => false,
    }
}

/// Escapes a CSS selector for embedding in a double-quoted JS string.
pub fn escape_selector(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_backslash_then_quote() {
        assert_eq!(
            escape_selector(r#"input[name="a\b"]"#),
            r#"input[name=\"a\\b\"]"#
        );
        assert_eq!(escape_selector("input#x"), "input#x");
        assert!(visibility_js(r#"a[b="c"]"#).contains(r#"querySelector("a[b=\"c\"]")"#));
    }

    #[test]
    fn mappings_follow_profile() {
        let f = FormFiller::new(Profile {
            first_name: "Jane".into(),
            last_name: "Döe".into(),
            email: "j@x.ch".into(),
            city: "Zürich".into(),
            ..Default::default()
        });
        let m = f.get_field_mappings();
        let types: Vec<_> = m.iter().map(|m| m.field_type).collect();
        assert_eq!(
            types,
            [
                "email",
                "firstName",
                "lastName",
                "fullName",
                "phone",
                "address",
                "city",
                "state",
                "zipCode",
                "country",
                "dateOfBirth"
            ]
        );
        assert_eq!(m[3].profile_value, "Jane Döe");
        assert_eq!(m[6].profile_value, "Zürich");
        assert_eq!(m[4].profile_value, "");
        assert_eq!(m[0].selectors[0], "input[type='email']");
        assert_eq!(m[2].patterns, ["last", "lname", "family", "surname"]);
        assert!(
            pattern_js(m[0].patterns).contains(r#"var patterns = ["email","e-mail","e_mail"];"#)
        );
    }
}
