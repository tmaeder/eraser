//! Port of internal/web: the browser UI (`eraser serve`).

mod csrf;
mod handlers;
mod job;
mod session;
mod views;

use crate::broker::{self, BrokerDatabase};
use crate::config::{self, Config, EmailConfig};
use crate::email::{self, Sender};
use crate::history::{self, Store};
use anyhow::{Context, Result};
use axum::Router;
use axum::http::{HeaderValue, header};
use axum::routing::{delete, get, post};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub use job::{JobStatus, PersistentJobState};

const DEFAULT_RATE_LIMIT: usize = 30;
const DEFAULT_RATE_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub public_origin: Option<String>,
    pub config_path: PathBuf,
    pub broker_path: PathBuf,
}

/// Builds an email sender from a config (swappable so tests never send real mail).
pub type SenderFactory = Arc<dyn Fn(&EmailConfig) -> Result<Box<dyn Sender>> + Send + Sync>;

/// Everything needed to build the router; `serve` fills it from the real environment.
pub struct AppOptions {
    pub config: Option<Config>,
    pub config_path: PathBuf,
    pub brokers: BrokerDatabase,
    pub history: Store,
    /// Holds pending_job.json (Go: ~/.eraser).
    pub data_dir: PathBuf,
    pub port: u16,
    pub public_origin: Option<String>,
    pub sender_factory: SenderFactory,
}

struct RateLimiter {
    requests: Mutex<HashMap<String, Vec<Instant>>>,
    limit: usize,
    window: Duration,
}

impl RateLimiter {
    fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.requests.lock().unwrap();
        let recent = map.entry(key.to_string()).or_default();
        recent.retain(|t| now.duration_since(*t) < self.window);
        if recent.len() >= self.limit {
            return false;
        }
        recent.push(now);
        true
    }

    fn cleanup(&self) {
        let now = Instant::now();
        let mut map = self.requests.lock().unwrap();
        map.values_mut()
            .for_each(|v| v.retain(|t| now.duration_since(*t) < self.window));
        map.retain(|_, v| !v.is_empty());
    }
}

pub struct AppState {
    config: RwLock<Option<Config>>,
    config_path: PathBuf,
    brokers: BrokerDatabase,
    history: Store,
    engine: crate::template::Engine,
    views: minijinja::Environment<'static>,
    sessions: session::SessionStore,
    rate_limiter: RateLimiter,
    jobs: job::JobManager,
    persistence: job::JobPersistence,
    secure_cookies: bool,
    sender_factory: SenderFactory,
    shutting_down: AtomicBool,
    job_tasks: AtomicUsize,
}

impl AppState {
    fn config(&self) -> Option<Config> {
        self.config.read().unwrap().clone()
    }
}

fn origin_host(origin: &str) -> Result<String> {
    let u = url::Url::parse(origin).with_context(|| format!("invalid public origin {origin:?}"))?;
    let host = u.host_str().context("public origin has no host")?;
    Ok(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    })
}

/// Builds the router (all routes + middleware) and its shared state.
pub fn app(opts: AppOptions) -> Result<(Router, Arc<AppState>)> {
    let port = opts.port;
    let mut trusted = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
    ];
    if let Some(origin) = &opts.public_origin {
        trusted.push(origin_host(origin)?);
    }
    let secure = opts.public_origin.is_some();
    let csrf = Arc::new(csrf::Csrf {
        trusted_origins: trusted,
        secure,
    });

    let state = Arc::new(AppState {
        config: RwLock::new(opts.config),
        config_path: opts.config_path,
        brokers: opts.brokers,
        history: opts.history,
        engine: crate::template::Engine::new()?,
        views: views::environment(),
        sessions: session::SessionStore::new(DEFAULT_SESSION_TTL),
        rate_limiter: RateLimiter {
            requests: Mutex::new(HashMap::new()),
            limit: DEFAULT_RATE_LIMIT,
            window: DEFAULT_RATE_WINDOW,
        },
        jobs: job::JobManager::default(),
        persistence: job::JobPersistence::new(opts.data_dir),
        secure_cookies: secure,
        sender_factory: opts.sender_factory,
        shutting_down: AtomicBool::new(false),
        job_tasks: AtomicUsize::new(0),
    });

    use handlers as h;
    let api = Router::new()
        .route("/stats", get(h::api_stats))
        .route("/brokers", get(h::api_brokers))
        .route("/history", get(h::api_history))
        .route("/history/failed", delete(h::api_delete_failed))
        .route("/send/{broker_id}", post(h::api_send_one))
        .route("/send-all", post(h::api_send_all))
        .route("/job/active", get(h::api_job_active))
        .route("/job/{job_id}/status", get(h::api_job_status))
        .route("/job/{job_id}/cancel", post(h::api_job_cancel))
        .route("/pipeline/stats", get(h::api_pipeline_stats))
        .route("/pipeline/responses", get(h::api_responses))
        .route("/pipeline/tasks", get(h::api_tasks))
        .route("/inbox/scan", post(h::api_inbox_scan))
        .route("/inbox/rescan", post(h::api_inbox_rescan))
        .route("/inbox/reclassify", post(h::api_reclassify));

    let router = Router::new()
        .route("/static/{*path}", get(h::static_file))
        .route("/", get(h::dashboard))
        .route("/brokers", get(h::brokers))
        .route("/history", get(h::history))
        .route("/settings", get(h::settings))
        .route("/settings/inbox", post(h::settings_inbox))
        .route("/pipeline", get(h::pipeline))
        .route("/tasks", get(h::tasks))
        .route("/tasks/{task_id}", get(h::task_detail))
        .route("/tasks/{task_id}/helper", get(h::task_helper))
        .route("/tasks/{task_id}/complete", post(h::task_complete))
        .route("/tasks/{task_id}/skip", post(h::task_skip))
        .route("/forms", get(h::forms))
        .route("/forms/{broker_id}/complete", post(h::form_complete))
        .route("/forms/{broker_id}/skip", post(h::form_skip))
        .route("/setup", get(h::setup_welcome))
        .route("/setup/", get(h::setup_welcome))
        .route(
            "/setup/profile",
            get(h::setup_profile).post(h::setup_profile),
        )
        .route("/setup/email", get(h::setup_email).post(h::setup_email))
        .route("/setup/test", get(h::setup_test))
        .route("/setup/test/send", post(h::setup_test_send))
        .route("/setup/complete", get(h::setup_complete))
        .nest("/api", api)
        .fallback(h::not_found)
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(csrf, csrf::protect))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    Ok((router, state))
}

const CSP: &str = "default-src 'self'; \
script-src 'self' 'unsafe-inline' 'unsafe-eval' https://cdn.tailwindcss.com https://unpkg.com; \
style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
img-src 'self' data:; \
font-src 'self' https://fonts.gstatic.com; \
connect-src 'self'; \
frame-ancestors 'none'; \
form-action 'self'; \
base-uri 'self'";

async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_static = req.uri().path().starts_with("/static/");
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let mut set = |k: header::HeaderName, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::REFERRER_POLICY, "strict-origin-when-cross-origin");
    set(header::CONTENT_SECURITY_POLICY, CSP);
    if !is_static {
        set(
            header::CACHE_CONTROL,
            "no-store, no-cache, must-revalidate, private",
        );
        set(header::PRAGMA, "no-cache");
        set(header::EXPIRES, "0");
    }
    set(
        header::HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=(), payment=()",
    );
    resp
}

fn open_browser(url: &str) {
    let (cmd, args): (&str, Vec<&str>) = match std::env::consts::OS {
        "macos" => ("open", vec![url]),
        "linux" => ("xdg-open", vec![url]),
        "windows" => ("cmd", vec!["/c", "start", url]),
        _ => return,
    };
    let _ = std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

/// Runs the web UI until SIGINT/SIGTERM. In-flight requests get a short grace period; a running
/// send job stops at its next broker (its progress is persisted after every broker, so it
/// resumes on the next start).
pub async fn serve(opts: ServeOptions) -> Result<()> {
    let cfg = if opts.config_path.exists() {
        match config::load(&opts.config_path) {
            Ok(c) => Some(c),
            Err(e) => {
                println!("⚠️  Config exists but failed to load: {e:#}");
                println!("The setup wizard will help you reconfigure.");
                None
            }
        }
    } else {
        None
    };
    let brokers = broker::load_from_file(&opts.broker_path).context("failed to load brokers")?;
    let history = Store::new(history::default_db_path()).context("failed to initialize history")?;
    let data_dir = dirs::home_dir().unwrap_or_default().join(".eraser");

    let (router, state) = app(AppOptions {
        config: cfg,
        config_path: opts.config_path,
        brokers,
        history,
        data_dir,
        port: opts.port,
        public_origin: opts.public_origin.clone(),
        sender_factory: Arc::new(|cfg: &EmailConfig| email::new_sender(cfg)),
    })?;

    let listener = tokio::net::TcpListener::bind((opts.host.as_str(), opts.port))
        .await
        .with_context(|| {
            format!(
                "server error: failed to listen on {}:{}",
                opts.host, opts.port
            )
        })?;

    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            cleanup_state.sessions.cleanup();
            cleanup_state.rate_limiter.cleanup();
        }
    });

    handlers::check_pending_job(&state);

    let url = format!("http://localhost:{}", opts.port);
    if opts.public_origin.is_none() {
        let url = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            open_browser(&url);
        });
    }
    match &opts.public_origin {
        Some(origin) => println!(
            "Starting Eraser web UI at {origin} (listening on {}:{})",
            opts.host, opts.port
        ),
        None => println!("Starting Eraser web UI at {url}"),
    }
    println!("Press Ctrl+C to stop");

    let (tx, rx) = tokio::sync::watch::channel(false);
    let sig_state = state.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        println!("\nShutting down...");
        sig_state.shutting_down.store(true, Ordering::SeqCst);
        tx.send_replace(true);
    });
    let mut graceful_rx = rx.clone();
    let mut grace_rx = rx;
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = graceful_rx.wait_for(|v| *v).await;
    });
    tokio::select! {
        r = server => r.context("server error")?,
        _ = async move {
            let _ = grace_rx.wait_for(|v| *v).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!("shutdown grace period elapsed; dropping open connections"),
    }

    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while state.job_tasks.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
