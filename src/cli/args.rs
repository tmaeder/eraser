//! clap definitions mirroring the cobra command tree. Help text is rendered by `help.rs`.
//! Bool flags accept `--flag`, `--flag=true` and `--flag=false` like pflag.

use clap::builder::BoolishValueParser;
use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

const ROOT_LONG: &str = "Eraser is an open-source tool that automates sending data removal
requests to data brokers, helping you protect your privacy.

It supports GDPR, CCPA, and generic removal request templates, and can
send via iCloud Mail SMTP.";

const SERVE_LONG: &str = "Start a local web server providing a browser-based interface for Eraser.

This opens a visual dashboard where you can:
- Set up your profile and email settings
- Browse and manage data brokers
- Send removal requests with visual progress
- View history and statistics

The server runs locally on your machine - no data is sent to external servers.";

const MONITOR_LONG: &str =
    "Connect to your email inbox via IMAP and monitor for responses from data brokers.

This command will:
- Fetch recent emails from known broker domains
- Classify responses (form required, confirmation needed, success, etc.)
- Extract form URLs and confirmation links
- Store results for the pipeline to process

Requires inbox configuration in config.yaml with IMAP settings.";

const FILL_LONG: &str = r#"Navigate to data broker opt-out forms and automatically fill them using your profile data.

This command uses headless Chrome to:
- Navigate to opt-out form URLs
- Detect and fill form fields with your personal information
- Detect CAPTCHAs (creates tasks for manual solving)
- Optionally submit the form

Examples:
  # Fill a specific form URL
  eraser fill --url "https://example.com/optout"

  # Fill form for a specific broker (using URL from pipeline)
  eraser fill --broker spokeo

  # Fill all pending forms from the pipeline
  eraser fill --pending

  # Fill with visible browser window (for debugging)
  eraser fill --url "https://example.com/optout" --headless=false

  # Fill form and wait for you to solve CAPTCHA, then auto-submit
  eraser fill --url "https://example.com/optout" --headless=false --wait --submit"#;

const CONFIRM_LONG: &str = r#"Automatically click confirmation links received from data brokers.

This command makes HTTP GET requests to confirmation URLs to complete the opt-out process.
It follows redirects and verifies success based on the response content.

Examples:
  # Confirm a specific URL
  eraser confirm --url "https://broker.com/confirm?token=abc123"

  # Confirm for a specific broker (using URL from pipeline)
  eraser confirm --broker spokeo

  # Confirm all pending confirmation links
  eraser confirm --pending

  # Preview without actually clicking (dry run)
  eraser confirm --pending --dry-run

Safety features:
  - Domain validation ensures links are from known broker domains
  - Follows redirects up to 10 hops
  - Detects success/failure from response content"#;

const CLEANUP_LONG: &str = "Scan your inbox for bounced/undeliverable emails and identify
invalid broker email addresses. Optionally remove them from the database.

By default, this command shows what would be removed without making changes.
Use --remove to actually remove the invalid brokers from the database.

Examples:
  eraser cleanup-bounces                 # Show bounced emails (dry run)
  eraser cleanup-bounces --remove        # Remove bounced brokers
  eraser cleanup-bounces --days 30       # Look back 30 days";

const SCAN_LONG: &str =
    "Scan INBOX, the archive folder and newsletter folders for companies that email you.

Only message headers are fetched (never bodies), and nothing is marked as read.
Senders are grouped by domain; your own and personal mail domains are excluded.
After the scan, privacy contacts are looked up in the broker database and on each
company's own website (only addresses published there are used).

Examples:
  eraser scan
  eraser scan --days 90 --folders INBOX,Archive
  eraser companies --lists";

const AUTOPILOT_LONG: &str = "Run one autopilot round now: check the inbox for broker replies, open
confirmation links on the broker's own domain, send one reminder after the deadline and the next
broker requests (daily limit applies), scan the inbox for companies, one-click unsubscribe and
send requests to inactive or newsletter-only companies.

Steps follow the autopilot section of config.yaml. Unlike the hosted schedule, this command runs
even when the autopilot is switched off. Anything that needs you appears under Today in the web UI.";

#[derive(Parser)]
#[command(
    name = "eraser",
    about = "Eraser - Automated data broker removal requests",
    long_about = ROOT_LONG,
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "string",
        help = "config file (default is $HOME/.eraser/config.yaml)"
    )]
    pub config: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "string",
        help = "broker database file (default is ./data/brokers.yaml)"
    )]
    pub brokers: Option<PathBuf>,
    #[arg(short, long, global = true, action = ArgAction::SetTrue, hide = true)]
    pub help: bool,
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    #[command(
        about = "Initialize configuration interactively",
        long_about = "Create a new configuration file with your personal information and email settings."
    )]
    Init,
    #[command(
        about = "Send removal requests to data brokers",
        long_about = "Send data removal requests to all configured data brokers."
    )]
    Send {
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Preview emails without sending")]
        dry_run: bool,
    },
    #[command(about = "Start the local web interface", long_about = SERVE_LONG)]
    Serve {
        #[arg(
            long,
            default_value_t = 8080,
            value_name = "int",
            help = "Port to listen on"
        )]
        port: u16,
        #[arg(
            long,
            default_value = "127.0.0.1",
            value_name = "string",
            help = "Address to listen on"
        )]
        host: String,
        #[arg(
            long,
            value_name = "string",
            help = "Public origin (scheme://host[:port]) when served behind a proxy"
        )]
        public_origin: Option<String>,
    },
    #[command(
        about = "List all data brokers in the database",
        long_about = "Show all data brokers that will receive removal requests."
    )]
    ListBrokers,
    #[command(
        about = "Show removal request history and statistics",
        long_about = "Display recent removal requests and overall statistics."
    )]
    Status {
        #[arg(
            long,
            default_value_t = 20,
            value_name = "int",
            allow_negative_numbers = true,
            help = "Number of recent requests to show"
        )]
        limit: i64,
    },
    #[command(
        about = "Add a new data broker to the database",
        long_about = "Interactively add a new data broker to the local broker database."
    )]
    AddBroker,
    #[command(about = "Monitor inbox for broker responses", long_about = MONITOR_LONG)]
    Monitor {
        #[arg(
            long,
            default_value_t = 7,
            value_name = "int",
            allow_negative_numbers = true,
            help = "Number of days to look back for emails"
        )]
        days: i64,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Check inbox once and exit (don't watch for new emails)")]
        once: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Continuously watch for new emails")]
        watch: bool,
    },
    #[command(
        about = "Show pipeline status and statistics",
        long_about = "Display the current status of the removal pipeline, including pending tasks and response classifications."
    )]
    Pipeline,
    #[command(about = "Fill opt-out forms using browser automation", long_about = FILL_LONG)]
    Fill {
        #[arg(
            long,
            default_value = "",
            value_name = "string",
            help = "Broker ID to fill form for (uses URL from pipeline)"
        )]
        broker: String,
        #[arg(
            long,
            default_value = "",
            value_name = "string",
            help = "Direct URL to the opt-out form"
        )]
        url: String,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "true", value_parser = BoolishValueParser::new(), help = "Run browser in headless mode")]
        headless: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Automatically submit the form after filling")]
        submit: bool,
        #[arg(
            long,
            default_value = "",
            value_name = "string",
            help = "Directory to save screenshots (default: ~/.eraser/screenshots)"
        )]
        screenshots: String,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Fill all pending forms from the pipeline")]
        pending: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Wait for user to solve CAPTCHA before continuing (use with --headless=false)")]
        wait: bool,
    },
    #[command(about = "Click confirmation links from broker emails", long_about = CONFIRM_LONG)]
    Confirm {
        #[arg(
            long,
            default_value = "",
            value_name = "string",
            help = "Direct confirmation URL to click"
        )]
        url: String,
        #[arg(
            long,
            default_value = "",
            value_name = "string",
            help = "Broker ID to confirm for (uses URL from pipeline)"
        )]
        broker: String,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Confirm all pending confirmation links")]
        pending: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "true", value_parser = BoolishValueParser::new(), help = "Validate URL domain against known brokers")]
        validate_domain: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Preview links without clicking them")]
        dry_run: bool,
    },
    #[command(about = "Find and remove bounced broker email addresses", long_about = CLEANUP_LONG)]
    CleanupBounces {
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Actually remove bounced brokers from database")]
        remove: bool,
        #[arg(
            long,
            default_value_t = 30,
            value_name = "int",
            allow_negative_numbers = true,
            help = "Number of days to scan for bounced emails"
        )]
        days: i64,
    },
    #[command(about = "Scan the inbox for companies that email you", long_about = SCAN_LONG)]
    Scan {
        #[arg(
            long,
            default_value_t = 365,
            value_name = "int",
            help = "Number of days to look back"
        )]
        days: i64,
        #[arg(
            long,
            value_delimiter = ',',
            value_name = "strings",
            help = "Folders to scan (default: INBOX, Archive, archive folder, Newsletter/Promotions if present)"
        )]
        folders: Vec<String>,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "true", value_parser = BoolishValueParser::new(), help = "Look up privacy contacts for new companies after the scan")]
        discover: bool,
    },
    #[command(
        about = "List companies found by 'eraser scan'",
        long_about = "List the companies found by 'eraser scan' with their privacy contact and unsubscribe options."
    )]
    Companies {
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Only mailing lists")]
        lists: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Only companies without a published privacy address")]
        manual: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Include excluded and ignored companies")]
        all: bool,
    },
    #[command(
        about = "Unsubscribe from companies' mailing lists",
        long_about = "Unsubscribe via RFC 8058 one-click or the List-Unsubscribe mailto: address. Plain web links are only shown, never opened.\n\nExample: eraser unsubscribe shop.example news.example --dry-run"
    )]
    Unsubscribe {
        #[arg(num_args = 0.., value_name = "domain")]
        domains: Vec<String>,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Show what would be done without contacting anyone")]
        dry_run: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Skip the confirmation prompt")]
        yes: bool,
    },
    #[command(
        about = "Send deletion requests to scanned companies",
        long_about = "Send a data deletion request to the discovered privacy contact of each company.\nAsks for confirmation unless --yes is given.\n\nExample: eraser request shop.example --dry-run"
    )]
    Request {
        #[arg(num_args = 0.., value_name = "domain")]
        domains: Vec<String>,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Show what would be sent without sending")]
        dry_run: bool,
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Skip the confirmation prompt")]
        yes: bool,
    },
    #[command(about = "Run one autopilot round", long_about = AUTOPILOT_LONG)]
    Autopilot {
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Show what would be done without contacting anyone")]
        dry_run: bool,
    },
    #[command(
        about = "Help about any command",
        long_about = "Help provides help for any command in the application.\nSimply type eraser help [path to command] for full details."
    )]
    Help { topic: Vec<String> },
}
