//! Differential tests against Go golden output, plus a real-browser integration test.

use super::*;
use crate::config::Profile;
use serde_json::Value;
use std::path::Path;

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/browser")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn differential_against_go() {
    let samples = fixture("samples.json");
    let golden = fixture("golden.json");
    let mut checked = 0;

    for (html, want) in samples["html"]
        .as_array()
        .unwrap()
        .iter()
        .zip(golden["captcha"].as_array().unwrap())
    {
        let got = detect_captcha_from_html(html.as_str().unwrap());
        assert_eq!(got.found, want["found"].as_bool().unwrap(), "{html}");
        assert_eq!(got.r#type, want["type"].as_str().unwrap(), "{html}");
        assert_eq!(
            got.confidence,
            want["confidence"].as_f64().unwrap(),
            "{html}"
        );
        assert_eq!(
            got.description,
            want["description"].as_str().unwrap(),
            "{html}"
        );
        checked += 1;
    }

    for (set, rows) in samples["domain_sets"]
        .as_array()
        .unwrap()
        .iter()
        .zip(golden["validate_domain"].as_array().unwrap())
    {
        let domains: Vec<&str> = set
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap())
            .collect();
        let h = ConfirmationHandler::new(&domains);
        for row in rows.as_array().unwrap() {
            let url = row["url"].as_str().unwrap();
            match h.validate_domain(url) {
                Err(_) => assert!(
                    row["error"].as_bool().unwrap(),
                    "unexpected error for {url:?}"
                ),
                Ok((valid, domain)) => {
                    assert!(!row["error"].as_bool().unwrap(), "Go errored for {url:?}");
                    assert_eq!(valid, row["valid"].as_bool().unwrap(), "{url:?}");
                    let allowed: Vec<&str> = row["domains"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|d| d.as_str().unwrap())
                        .collect();
                    assert!(
                        allowed.contains(&domain.as_str()),
                        "{url:?}: {domain} not in {allowed:?}"
                    );
                }
            }
            checked += 1;
        }
    }

    for row in golden["success"].as_array().unwrap() {
        let status = row["status"].as_u64().unwrap() as u16;
        let body = row["body"].as_str().unwrap();
        let success = is_success_response(status, body);
        assert_eq!(
            success,
            row["success"].as_bool().unwrap(),
            "{status} {body:?}"
        );
        let r = ConfirmationResult {
            success,
            status_code: status,
            response_body: body.into(),
            ..Default::default()
        };
        assert_eq!(
            extract_confirmation_status(&r),
            row["status_text"].as_str().unwrap()
        );
        checked += 1;
    }
    assert_eq!(checked, 32 + 37 + 24);
}

const FORM: &str = r#"<!doctype html><html><head><title>Opt Out</title></head><body>
<form action="/done" method="get">
  <label for="em">Your e-mail</label><input id="em" name="contact">
  <input name="first_name">
  <input name="last_name">
  <input name="city">
  <input name="hidden_zip" style="display:none">
  <button type="submit">Submit</button>
</form></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn navigate_and_fill_real_browser() {
    use axum::{Router, response::Html, routing::get};
    let app = Router::new()
        .route("/optout", get(|| async { Html(FORM) }))
        .route("/done", get(|| async { Html("<p>Request received</p>") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let profile = Profile {
        first_name: "Jürg".into(),
        last_name: "Müller".into(),
        email: "juerg@example.ch".into(),
        city: "Zürich".into(),
        zip_code: "8001".into(),
        ..Default::default()
    };
    let shots = tempfile::tempdir().unwrap();
    let cfg = BrowserConfig {
        timeout: std::time::Duration::from_secs(45),
        screenshot_dir: shots.path().to_string_lossy().into(),
        ..Default::default()
    };
    let mut b = Browser::new(cfg, profile).await.expect("launch Chrome");
    let url = format!("http://{addr}/optout");
    let r = b
        .navigate_and_fill(&url, "testbroker", false)
        .await
        .unwrap();

    assert!(r.success, "{r:?}");
    assert!(!r.captcha_found);
    assert_eq!(r.fields_filled, ["email", "firstName", "lastName", "city"]);
    assert_eq!(
        r.fields_missing,
        ["phone", "address", "state", "country", "dateOfBirth"]
    );
    assert!(
        r.screenshot_path.starts_with("testbroker_filled_") && r.screenshot_path.ends_with(".png")
    );
    assert!(shots.path().join(&r.screenshot_path).is_file());

    let values: Vec<String> = b
        .page()
        .evaluate("Array.from(document.querySelectorAll('input')).map(i => i.value)")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(values, ["juerg@example.ch", "Jürg", "Müller", "Zürich", ""]);
    assert_eq!(b.get_page_title().await.unwrap(), "Opt Out");

    let fields = FormFiller::new(Profile::default())
        .detect_form_fields(b.page())
        .await
        .unwrap();
    assert_eq!(fields.len(), 5);
    assert_eq!(
        (fields[0].label.as_str(), fields[4].visible),
        ("Your e-mail", false)
    );

    let r = b.navigate_and_fill(&url, "testbroker", true).await.unwrap();
    assert!(r.submit_attempted && r.success, "{r:?}");
    let landed = b.page().url().await.unwrap().unwrap_or_default();
    assert!(
        landed.contains("/done?contact=juerg%40example.ch"),
        "{landed}"
    );
    b.close().await;
}
