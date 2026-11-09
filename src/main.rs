//! Eraser CLI.

mod cli;

/// Go's default `log` prefix: "2006/01/02 15:04:05 ".
struct GoLogTime;

impl tracing_subscriber::fmt::time::FormatTime for GoLogTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y/%m/%d %H:%M:%S"))
    }
}

#[tokio::main]
async fn main() {
    // Library log lines (Go: log.Printf) go to stderr; RUST_LOG overrides the level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_timer(GoLogTime)
        .with_level(false)
        .with_target(false)
        .with_ansi(false)
        .init();

    std::process::exit(cli::run(std::env::args_os().collect()).await);
}
