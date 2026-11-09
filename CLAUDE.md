# CLAUDE.md

## Project Overview

Eraser sends GDPR / CCPA / Swiss FADP deletion requests to 1,350+ data brokers from the user's own mailbox (iCloud by default), tracks replies over IMAP, and can scan the user's inbox for companies to send requests to or unsubscribe from. Free alternative to Incogni / DeleteMe.

## Tech Stack

- **Language**: Rust (edition 2024)
- **CLI**: clap (derive); help/output mirrors the original Go/cobra version
- **Web UI**: axum + minijinja (templates and static files embedded in the binary), HTMX
- **Email**: lettre (SMTP; port 587 = STARTTLS, otherwise implicit TLS)
- **Inbox**: async-imap + mail-parser
- **Browser automation**: chromiumoxide (Chrome/Chromium; `CHROME_PATH` overrides)
- **Database**: SQLite via rusqlite (bundled) at `~/.eraser/history.db`
- **Config**: YAML (`serde_yaml_ng`) at `~/.eraser/config.yaml`

## Project Structure

```
src/
├── main.rs, cli/        # CLI commands
├── config.rs            # Config structs, load/save (0600), provider defaults
├── broker.rs            # Broker DB loading/filtering
├── template.rs          # Request templates; "auto" picks fadp (ch) / gdpr (eu) / generic
├── email.rs             # Sender trait + SMTP sender
├── history/             # SQLite store (requests, responses, tasks, scanner tables)
├── inbox/               # IMAP monitor, response classifier, URL parser
├── scanner/             # Inbox company scanner, privacy-contact discovery, unsubscribe
├── browser/             # Opt-out form filler, captcha detection (detection only), confirm links
└── web/                 # Router, CSRF, sessions, send jobs, companies UI
templates/email/          # gdpr, ccpa, fadp, generic request texts
templates/web/            # Web UI templates
data/brokers.yaml         # Broker database
docs/manual-optouts.md    # Brokers that only accept web forms
deploy/                   # Cloudflare Containers (Worker with basic auth, R2 state sync, Dockerfile)
tests/                    # Integration tests; tests/golden holds fixtures generated from the original Go version
```

## Common Commands

```bash
cargo build --release && cp target/release/eraser .
cargo test
cargo clippy --all-targets -- -D warnings
./eraser serve                       # web UI on localhost:8080
./eraser send --dry-run
./eraser scan --days 365             # inbox company scanner
```

Docker-gated integration tests: `ERASER_IMAP_IT=1 cargo test` (GreenMail IMAP), `ERASER_BROWSER_IT=1 cargo test` (headless Chrome).

## Adding Brokers

Required fields in `data/brokers.yaml`: `id`, `name`, `email`, `region` (`us`, `eu`, `ch`, `global`, …), `category`. Only use privacy emails seen on the company's own privacy page; never construct addresses or use named employees' addresses. Brokers without an email go to `docs/manual-optouts.md`. Broker tests compare against `tests/golden/brokers_go.json`; regenerate it with `tests/golden/gen/README.txt` (needs the Go version at tag `go-final`) whenever the broker list changes.

## Deployment

`deploy/` runs the binary in a Cloudflare Container behind a Worker at https://erase.kampa.app: HTTP basic auth (`ERASER_PASSWORD` secret), `~/.eraser` restored from and synced to the R2 bucket `eraser-state` every 60 s and on SIGTERM. Deploy with `cd deploy && npx wrangler deploy` (Docker must be running).

## Security Notes

- Never commit user configs (personal data, app passwords); the config file is 0600
- Use app-specific passwords, not account passwords
- Nothing is sent or unsubscribed without an explicit user action
