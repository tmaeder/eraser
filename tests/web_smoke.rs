//! Helper for the Go-vs-Rust side-by-side smoke check (run manually):
//! HOME=$tmp SMOKE_PORT=18081 SMOKE_CONFIG=... SMOKE_BROKERS=... cargo test --test web_smoke -- --ignored

#[tokio::test]
#[ignore]
async fn serve_for_smoke() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    eraser::web::serve(eraser::web::ServeOptions {
        host: "127.0.0.1".into(),
        port: env("SMOKE_PORT").parse().unwrap(),
        public_origin: None,
        config_path: env("SMOKE_CONFIG").into(),
        broker_path: env("SMOKE_BROKERS").into(),
    })
    .await
    .unwrap();
}
