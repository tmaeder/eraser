//! IMAP inbox monitoring (port of `internal/inbox/monitor.go`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_imap::Session;
use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::{MailboxDatum, Response};
use chrono::{DateTime, Local, TimeDelta, Utc};
use futures::TryStreamExt;
use mail_parser::{MessageParser, PartKind};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tracing::{info, warn};

use super::go_lower;
use crate::broker::Broker;
use crate::config::InboxConfig;

type ImapSession = Session<TlsStream<TcpStream>>;

const NOT_CONNECTED: &str = "not connected to IMAP server";
/// TCP + TLS + greeting + login.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// ponytail: one bound per operation (all its commands), not per socket read; a per-read
/// timeout would need a stream wrapper that also knows about IDLE.
const OP_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_RETRY: crate::retry::Policy = crate::retry::Policy::new(2, Duration::from_secs(1));

/// An IMAP operation exceeded its time limit; the session is unusable afterwards.
#[derive(Debug)]
struct ImapTimeout;

impl std::fmt::Display for ImapTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IMAP server did not respond in time")
    }
}

impl std::error::Error for ImapTimeout {}

/// Dropped connection or timeout (worth one reconnect); not login, NO/BAD or parse errors.
fn is_connection_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<ImapTimeout>()
            // InvalidData = TLS certificate / protocol error: permanent
            || c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() != std::io::ErrorKind::InvalidData)
            || matches!(
                c.downcast_ref::<async_imap::error::Error>(),
                Some(async_imap::error::Error::Io(_) | async_imap::error::Error::ConnectionLost)
            )
    })
}

async fn timed<T>(limit: Duration, fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or_else(|_| Err(ImapTimeout.into()))
}

/// Runs `self.<method>(args)` under the operation timeout; after a dropped connection or a
/// timeout it reconnects and tries once more.
macro_rules! reconnect_once {
    ($self:ident . $method:ident ( $($arg:expr),* )) => {{
        let first = timed($self.op_timeout, $self.$method($($arg),*)).await;
        match first {
            Err(e) if is_connection_error(&e) => {
                warn!("IMAP connection problem ({e:#}), reconnecting once");
                $self.session = None;
                $self.connect().await?;
                timed($self.op_timeout, $self.$method($($arg),*)).await
            }
            r => r,
        }
    }};
}
const FOLDER_BATCH_SIZE: usize = 50;

const BOUNCE_SENDERS: &[&str] = &[
    "mailer-daemon",
    "postmaster",
    "mail delivery",
    "mail delivery system",
    "mail delivery subsystem",
    "mailerdaemon",
    "mailsystem",
];

const BOUNCE_SUBJECTS: &[&str] = &[
    "undeliverable",
    "delivery failed",
    "delivery status notification",
    "returned mail",
    "mail delivery failed",
    "delivery failure",
    "message not delivered",
    "could not be delivered",
];

/// An email fetched from the inbox.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Email {
    /// IMAP UID for operations like move/delete
    pub uid: u32,
    pub message_id: String,
    pub from: String,
    /// Sender display name (e.g. "Mail Delivery System")
    pub from_name: String,
    pub from_domain: String,
    pub subject: String,
    pub body: String,
    pub html_body: String,
    pub received_at: DateTime<Utc>,
    /// Matched broker ID (if found)
    pub broker_id: String,
    /// Matched broker name (if found)
    pub broker_name: String,
    /// Read receipt (MDN): the Message-IDs it may refer to (empty when it reports no display,
    /// e.g. "deleted"). None for every other mail.
    pub read_receipt_for: Option<Vec<String>>,
}

/// Handles IMAP connection and email monitoring.
pub struct Monitor {
    config: InboxConfig,
    session: Option<ImapSession>,
    /// Map of email domain to broker
    brokers: HashMap<String, Broker>,
    /// Limit per operation ([`OP_TIMEOUT`]; tests shorten it).
    pub(crate) op_timeout: Duration,
    /// Test-only: accept any server certificate (self-signed test servers).
    #[cfg(test)]
    pub(crate) insecure_tls: bool,
}

/// Header fields fetched by the company scanner (never bodies).
pub const SCAN_HEADER_FIELDS: &str = "FROM REPLY-TO LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST LIST-ID PRECEDENCE AUTO-SUBMITTED DATE MESSAGE-ID";

/// One window of header-only fetches from a folder.
#[derive(Debug, Default)]
pub struct HeaderBatch {
    pub uid_validity: u32,
    /// Messages in the date range (all windows together).
    pub total: usize,
    /// (uid, raw header block)
    pub headers: Vec<(u32, Vec<u8>)>,
}

impl Monitor {
    /// Creates a new inbox monitor.
    pub fn new(cfg: InboxConfig, broker_list: &[Broker]) -> Self {
        Monitor {
            config: cfg,
            session: None,
            brokers: build_broker_map(broker_list),
            op_timeout: OP_TIMEOUT,
            #[cfg(test)]
            insecure_tls: false,
        }
    }

    /// Establishes the IMAP connection (implicit TLS) and logs in, within [`CONNECT_TIMEOUT`];
    /// a dropped connection or timeout is retried once, a failed login never.
    pub async fn connect(&mut self) -> Result<()> {
        let session = crate::retry::run(
            CONNECT_RETRY,
            || timed(CONNECT_TIMEOUT, self.open_session()),
            |e| {
                if is_connection_error(e) {
                    crate::retry::Verdict::Retry
                } else {
                    crate::retry::Verdict::Stop
                }
            },
        )
        .await?;
        self.session = Some(session);
        info!("Login successful");
        Ok(())
    }

    async fn open_session(&self) -> Result<ImapSession> {
        let addr = format!("{}:{}", self.config.server, self.config.port);
        info!("Connecting to IMAP server {addr}...");

        let stream = self
            .dial_tls(&addr)
            .await
            .context("failed to connect to IMAP server")?;
        let mut client = async_imap::Client::new(stream);
        client
            .read_response()
            .await
            .context("failed to connect to IMAP server")?
            .ok_or_else(|| anyhow!("connection closed before greeting"))
            .context("failed to connect to IMAP server")?;

        info!("Connected, logging in as {}...", self.config.email);
        let session = client
            .login(&self.config.email, &self.config.password)
            .await
            .map_err(|(e, _)| anyhow::Error::from(e).context("failed to login"))?;
        Ok(session)
    }

    async fn dial_tls(&self, addr: &str) -> Result<TlsStream<TcpStream>> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?;
        #[cfg(test)]
        let builder = if self.insecure_tls {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(tests::AcceptAnyCert(provider)))
        } else {
            builder.with_root_certificates(webpki_root_store())
        };
        #[cfg(not(test))]
        let builder = builder.with_root_certificates(webpki_root_store());
        let tls_config = builder.with_no_client_auth();

        let server_name = rustls::pki_types::ServerName::try_from(self.config.server.clone())?;
        let tcp = TcpStream::connect(addr).await?;
        Ok(TlsConnector::from(Arc::new(tls_config))
            .connect(server_name, tcp)
            .await?)
    }

    /// Closes the IMAP connection.
    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            session.logout().await?;
        }
        Ok(())
    }

    fn session(&mut self) -> Result<&mut ImapSession> {
        self.session.as_mut().ok_or_else(|| anyhow!(NOT_CONNECTED))
    }

    /// Fetches emails from the last `days` days in the configured folder.
    /// Uses `BODY[]`, so fetched messages get marked `\Seen`.
    pub async fn fetch_recent_emails(&mut self, days: i64) -> Result<Vec<Email>> {
        reconnect_once!(self.fetch_recent_emails_once(days))
    }

    async fn fetch_recent_emails_once(&mut self, days: i64) -> Result<Vec<Email>> {
        let folder = self.config.folder.clone();
        let session = self.session()?;

        let mbox = session
            .select(&folder)
            .await
            .with_context(|| format!("failed to select mailbox {folder}"))?;
        info!("Mailbox {folder} has {} messages", mbox.exists);
        if mbox.exists == 0 {
            return Ok(Vec::new());
        }

        let since = since_date(days);
        let uids = sorted(
            session
                .uid_search(format!("SINCE {since}"))
                .await
                .context("failed to search emails")?,
        );
        info!("Found {} emails since {since}", uids.len());
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let fetches: Vec<_> = session
            .uid_fetch(uid_set(&uids), "(ENVELOPE FLAGS UID BODY[])")
            .await
            .context("failed to fetch messages")?
            .try_collect()
            .await
            .context("failed to fetch messages")?;

        Ok(fetches
            .iter()
            .filter(|f| f.envelope().is_some())
            .map(|f| self.parse_message(f.uid.unwrap_or(0), f.body().unwrap_or_default()))
            .collect())
    }

    fn parse_message(&self, uid: u32, raw: &[u8]) -> Email {
        let mut email = parse_raw_email(uid, raw);
        if let Some(b) = self.brokers.get(&email.from_domain) {
            email.broker_id = b.id.clone();
            email.broker_name = b.name.clone();
        }
        email
    }

    /// Fetches only emails from known broker domains.
    pub async fn fetch_broker_emails(&mut self, days: i64) -> Result<Vec<Email>> {
        let all = self.fetch_recent_emails(days).await?;
        let total = all.len();
        let broker_emails: Vec<Email> = all.into_iter().filter(is_broker_or_receipt).collect();
        info!(
            "Found {} emails from known brokers (out of {total} total)",
            broker_emails.len()
        );
        Ok(broker_emails)
    }

    /// Fetches broker emails from a specific folder, in batches of 50, with
    /// `BODY.PEEK[]` (does not set `\Seen`). Batch errors are logged and skipped.
    pub async fn fetch_broker_emails_from_folder(
        &mut self,
        folder: &str,
        days: i64,
    ) -> Result<Vec<Email>> {
        reconnect_once!(self.fetch_broker_emails_from_folder_once(folder, days))
    }

    async fn fetch_broker_emails_from_folder_once(
        &mut self,
        folder: &str,
        days: i64,
    ) -> Result<Vec<Email>> {
        let session = self.session()?;
        let mbox = session
            .select(folder)
            .await
            .with_context(|| format!("failed to select folder {folder}"))?;
        info!("Folder {folder} has {} messages", mbox.exists);
        if mbox.exists == 0 {
            return Ok(Vec::new());
        }

        let since = since_date(days);
        let uids = sorted(
            session
                .uid_search(format!("SINCE {since}"))
                .await
                .with_context(|| format!("failed to search emails in {folder}"))?,
        );
        info!("Found {} emails since {since} in {folder}", uids.len());
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let mut fetched = Vec::new();
        for batch in uids.chunks(FOLDER_BATCH_SIZE) {
            let result: Result<Vec<_>> = async {
                Ok(session
                    .uid_fetch(uid_set(batch), "(ENVELOPE UID BODY.PEEK[])")
                    .await?
                    .try_collect()
                    .await?)
            }
            .await;
            match result {
                Ok(f) => fetched.extend(f),
                // The connection is gone: fail so the caller reconnects instead of skipping
                // every remaining batch.
                Err(e) if is_connection_error(&e) => return Err(e),
                Err(e) => warn!("Warning: error fetching batch: {e:#}"),
            }
        }

        let total = fetched.iter().filter(|f| f.envelope().is_some()).count();
        let broker_emails: Vec<Email> = fetched
            .iter()
            .filter(|f| f.envelope().is_some())
            .map(|f| self.parse_message(f.uid.unwrap_or(0), f.body().unwrap_or_default()))
            .filter(is_broker_or_receipt)
            .collect();
        info!(
            "Found {} emails from known brokers in {folder} (out of {total} total)",
            broker_emails.len()
        );
        Ok(broker_emails)
    }

    /// Fetches emails that look like bounce/undeliverable notifications.
    pub async fn fetch_bounce_emails(&mut self, days: i64) -> Result<Vec<Email>> {
        let all = self.fetch_recent_emails(days).await?;
        let total = all.len();
        let bounces: Vec<Email> = all.into_iter().filter(is_bounce).collect();
        info!(
            "Found {} bounce emails (out of {total} total)",
            bounces.len()
        );
        Ok(bounces)
    }

    /// Monitors the configured folder via IDLE. On each new-mail notification
    /// (EXISTS/RECENT) it re-fetches the last day and passes every broker email
    /// to `callback`. Returns `Ok(())` once `stop` resolves.
    pub async fn watch_for_new_emails<F, S>(&mut self, mut callback: F, stop: S) -> Result<()>
    where
        F: FnMut(Email),
        S: Future<Output = ()>,
    {
        let folder = self.config.folder.clone();
        self.session()?
            .select(&folder)
            .await
            .context("failed to select mailbox")?;
        info!("Watching for new emails (press Ctrl+C to stop)...");
        tokio::pin!(stop);

        loop {
            let mut idle = self
                .session
                .take()
                .ok_or_else(|| anyhow!(NOT_CONNECTED))?
                .idle();
            idle.init().await.context("IDLE error")?;
            let response = {
                let (wait, _interrupt) = idle.wait();
                tokio::select! {
                    r = wait => Some(r),
                    _ = &mut stop => None,
                }
            };
            self.session = Some(idle.done().await.context("IDLE error")?);

            let Some(response) = response else {
                return Ok(());
            };
            let IdleResponse::NewData(data) = response.context("IDLE error")? else {
                continue; // timeout: re-issue IDLE
            };
            if let Response::MailboxData(MailboxDatum::Exists(n) | MailboxDatum::Recent(n)) =
                data.parsed()
            {
                info!("New mail detected: {n} messages");
                match self.fetch_recent_emails(1).await {
                    Err(e) => warn!("Error fetching new email: {e:#}"),
                    Ok(emails) => emails
                        .into_iter()
                        .filter(|e| !e.broker_id.is_empty())
                        .for_each(&mut callback),
                }
            }
        }
    }

    /// Test helper: APPENDs a raw message to `folder` (creating the folder if needed).
    #[cfg(test)]
    pub(crate) async fn append_raw(&mut self, folder: &str, raw: &[u8]) -> Result<()> {
        if folder != "INBOX" {
            self.ensure_folder_exists(folder).await?;
        }
        self.session()?.append(folder, None, None, raw).await?;
        Ok(())
    }

    /// Lists all mailbox names.
    pub async fn list_folders(&mut self) -> Result<Vec<String>> {
        reconnect_once!(self.list_folders_once())
    }

    async fn list_folders_once(&mut self) -> Result<Vec<String>> {
        let names: Vec<_> = self
            .session()?
            .list(Some(""), Some("*"))
            .await
            .context("failed to list folders")?
            .try_collect()
            .await
            .context("failed to list folders")?;
        Ok(names.iter().map(|n| n.name().to_string()).collect())
    }

    /// Header-only fetch for the company scanner: EXAMINEs `folder` (read-only), searches the
    /// last `days` and fetches `BODY.PEEK[HEADER.FIELDS (...)]` for UIDs `[offset, offset+limit)`
    /// in batches of 50. Never sets `\Seen`, never downloads bodies.
    pub async fn fetch_scan_headers(
        &mut self,
        folder: &str,
        days: i64,
        offset: usize,
        limit: usize,
    ) -> Result<HeaderBatch> {
        reconnect_once!(self.fetch_scan_headers_once(folder, days, offset, limit))
    }

    async fn fetch_scan_headers_once(
        &mut self,
        folder: &str,
        days: i64,
        offset: usize,
        limit: usize,
    ) -> Result<HeaderBatch> {
        let session = self.session()?;
        let mbox = session
            .examine(folder)
            .await
            .with_context(|| format!("failed to examine folder {folder}"))?;
        let mut batch = HeaderBatch {
            uid_validity: mbox.uid_validity.unwrap_or(0),
            ..Default::default()
        };
        if mbox.exists == 0 {
            return Ok(batch);
        }
        let uids = sorted(
            session
                .uid_search(format!("SINCE {}", since_date(days)))
                .await
                .with_context(|| format!("failed to search emails in {folder}"))?,
        );
        batch.total = uids.len();
        let window = uids
            .iter()
            .copied()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let query = format!("(UID BODY.PEEK[HEADER.FIELDS ({SCAN_HEADER_FIELDS})])");
        for chunk in window.chunks(FOLDER_BATCH_SIZE) {
            let fetched: Vec<_> = session
                .uid_fetch(uid_set(chunk), &query)
                .await
                .with_context(|| format!("failed to fetch headers in {folder}"))?
                .try_collect()
                .await
                .with_context(|| format!("failed to fetch headers in {folder}"))?;
            batch.headers.extend(
                fetched
                    .iter()
                    .filter_map(|f| Some((f.uid?, f.header()?.to_vec()))),
            );
        }
        Ok(batch)
    }

    /// Creates a folder/label if it doesn't already exist (case-insensitive check).
    pub async fn ensure_folder_exists(&mut self, name: &str) -> Result<()> {
        let session = self.session()?;
        let names: Vec<_> = session
            .list(Some(""), Some("*"))
            .await
            .context("failed to list folders")?
            .try_collect()
            .await
            .context("failed to list folders")?;

        let wanted = go_lower(name);
        if names.iter().any(|n| go_lower(n.name()) == wanted) {
            info!("Folder '{name}' already exists");
            return Ok(());
        }

        session
            .create(name)
            .await
            .with_context(|| format!("failed to create folder '{name}'"))?;
        info!("Created folder '{name}'");
        Ok(())
    }

    /// Moves a single email (by UID, in the currently selected mailbox) to `folder`.
    pub async fn move_to_folder(&mut self, uid: u32, folder: &str) -> Result<()> {
        let session = self.session()?;
        move_uids(session, &uid.to_string(), folder, "email").await
    }

    /// Re-selects the configured folder and moves `uids` to the archive `folder`.
    pub async fn archive_emails(&mut self, uids: &[u32], folder: &str) -> Result<()> {
        let inbox = self.config.folder.clone();
        let session = self.session()?;
        if uids.is_empty() {
            return Ok(());
        }
        session
            .select(&inbox)
            .await
            .context("failed to select mailbox")?;
        move_uids(
            session,
            &uid_set(&sorted(uids.iter().copied())),
            folder,
            "emails",
        )
        .await?;
        info!("Archived {} emails to '{folder}'", uids.len());
        Ok(())
    }
}

/// UID MOVE (RFC 6851), falling back to UID COPY + `\Deleted` + EXPUNGE.
/// Note: the fallback EXPUNGE removes *all* `\Deleted` messages in the mailbox.
async fn move_uids(session: &mut ImapSession, set: &str, folder: &str, noun: &str) -> Result<()> {
    let Err(move_err) = session.uid_mv(set, folder).await else {
        return Ok(());
    };
    if noun == "emails" {
        info!("MOVE not supported, falling back to COPY+DELETE: {move_err}");
    }
    session
        .uid_copy(set, folder)
        .await
        .with_context(|| format!("failed to copy {noun} to '{folder}'"))?;
    session
        .uid_store(set, "+FLAGS.SILENT (\\Deleted)")
        .await
        .with_context(|| format!("failed to mark {noun} as deleted"))?
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("failed to mark {noun} as deleted"))?;
    let expunge_ctx = if noun == "email" {
        "failed to expunge deleted email"
    } else {
        "failed to expunge deleted emails"
    };
    session
        .expunge()
        .await
        .context(expunge_ctx)?
        .try_collect::<Vec<_>>()
        .await
        .context(expunge_ctx)?;
    Ok(())
}

fn webpki_root_store() -> rustls::RootCertStore {
    rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// Maps broker email domains and website domains to brokers. Later brokers win
/// on collisions. Lookup is exact (no subdomain matching), like Go.
fn build_broker_map(broker_list: &[Broker]) -> HashMap<String, Broker> {
    let mut map = HashMap::new();
    for b in broker_list {
        if !b.email.is_empty() {
            let parts: Vec<&str> = b.email.split('@').collect();
            if parts.len() == 2 {
                map.insert(go_lower(parts[1]), b.clone());
            }
        }
        if !b.website.is_empty() {
            let domain = extract_domain(&b.website);
            if !domain.is_empty() {
                map.insert(domain, b.clone());
            }
        }
    }
    map
}

/// Extracts the lowercased host from a URL (strips one `https://`, `http://`, `www.`).
fn extract_domain(url: &str) -> String {
    let url = url.strip_prefix("https://").unwrap_or(url);
    let url = url.strip_prefix("http://").unwrap_or(url);
    let url = url.strip_prefix("www.").unwrap_or(url);
    go_lower(url.split('/').next().unwrap_or_default())
}

/// IMAP SINCE date (`2-Jan-2006`) for local "today minus `days`".
fn since_date(days: i64) -> String {
    let date = Local::now().date_naive() - TimeDelta::days(days);
    date.format("%-d-%b-%Y").to_string()
}

fn sorted(uids: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut v: Vec<u32> = uids.into_iter().collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Compresses sorted UIDs into an IMAP sequence set, e.g. `1:3,7`.
fn uid_set(sorted_uids: &[u32]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted_uids.len() {
        let start = sorted_uids[i];
        let mut end = start;
        while i + 1 < sorted_uids.len() && sorted_uids[i + 1] == end + 1 {
            i += 1;
            end = sorted_uids[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        i += 1;
    }
    parts.join(",")
}

/// Broker replies plus read receipts (a receipt may come from any address; the history store
/// links it to the request instead of storing it as a reply).
fn is_broker_or_receipt(email: &Email) -> bool {
    !email.broker_id.is_empty() || email.read_receipt_for.is_some()
}

/// Subject prefixes of read receipts sent as plain mails (en, de, fr).
const RECEIPT_SUBJECTS: &[&str] = &["read:", "gelesen:", "lu :", "lu:", "lu\u{a0}:"];

/// Message-IDs a read receipt refers to: RFC 8098 reports (multipart/report;
/// report-type=disposition-notification; Original-Message-ID first, then In-Reply-To and
/// References; emptied unless the disposition is "displayed"), and "Read: <subject>" mails that
/// reference a message. None for everything else.
fn read_receipt_refs(message: &mail_parser::Message, subject: &str) -> Option<Vec<String>> {
    let bracket = |id: &str| format!("<{}>", id.trim().trim_matches(['<', '>']));
    let mut refs: Vec<String> = message
        .in_reply_to()
        .into_iter()
        .chain(message.references())
        .flat_map(|list| list.iter().map(bracket).collect::<Vec<_>>())
        .collect();
    let is_report = message.content_type().is_some_and(|ct| {
        ct.ctype().eq_ignore_ascii_case("multipart")
            && ct
                .subtype()
                .is_some_and(|s| s.eq_ignore_ascii_case("report"))
            && ct
                .attribute("report-type")
                .is_some_and(|t| t.eq_ignore_ascii_case("disposition-notification"))
    });
    let report = message
        .root()
        .parts()
        .find(|p| p.is_content_type("message", "disposition-notification"));
    if is_report || report.is_some() {
        let mut displayed = false;
        if let Some(part) = report {
            let fields = String::from_utf8_lossy(&part.decoded()).into_owned();
            for line in fields.lines() {
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                match name.trim().to_ascii_lowercase().as_str() {
                    "original-message-id" => refs.insert(0, bracket(value)),
                    "disposition" => displayed = value.to_ascii_lowercase().contains("displayed"),
                    _ => {}
                }
            }
        }
        if !displayed {
            refs.clear();
        }
        return Some(refs);
    }
    let subject = go_lower(subject);
    (RECEIPT_SUBJECTS.iter().any(|p| subject.starts_with(p)) && !refs.is_empty()).then_some(refs)
}

fn is_bounce(email: &Email) -> bool {
    let from = go_lower(&email.from);
    let from_name = go_lower(&email.from_name);
    let subject = go_lower(&email.subject);
    BOUNCE_SENDERS
        .iter()
        .any(|s| from.contains(s) || from_name.contains(s))
        || BOUNCE_SUBJECTS.iter().any(|p| subject.contains(p))
}

/// Converts a raw RFC 822 message into an [`Email`] (broker fields left empty).
/// Body: first non-empty inline `text/plain` part and first non-empty inline
/// `text/html` part, transfer-encoding and charset decoded. Nested
/// `message/rfc822` parts and attachments are ignored.
pub fn parse_raw_email(uid: u32, raw: &[u8]) -> Email {
    let mut email = Email {
        uid,
        ..Default::default()
    };
    let Some(message) = MessageParser::default().parse(raw) else {
        return email;
    };

    email.subject = message.subject().unwrap_or_default().to_string();
    if let Some(id) = message.message_id() {
        email.message_id = format!("<{id}>"); // IMAP ENVELOPE keeps the brackets
    }
    email.read_receipt_for = read_receipt_refs(&message, &email.subject);
    if let Some(date) = message.date().filter(|d| d.is_valid()) {
        email.received_at =
            DateTime::<Utc>::from_timestamp(date.to_timestamp(), 0).unwrap_or_default();
    }
    if let Some(from) = message.from().and_then(|f| f.first()) {
        email.from = from.address().unwrap_or_default().to_string();
        email.from_name = from.name().unwrap_or_default().to_string();
        if let Some((_, host)) = email.from.rsplit_once('@') {
            email.from_domain = go_lower(host);
        }
    }

    for part in message.root().parts() {
        if matches!(part.kind(), PartKind::Multipart | PartKind::Message(_)) {
            continue;
        }
        let (ctype, subtype) = part
            .content_type()
            .map_or(("text".into(), "plain".into()), |ct| {
                (
                    ct.ctype().to_ascii_lowercase(),
                    ct.subtype().unwrap_or_default().to_ascii_lowercase(),
                )
            });
        let disposition = part
            .content_disposition()
            .map(|d| d.ctype().to_ascii_lowercase())
            .unwrap_or_default();
        let inline = disposition == "inline" || (disposition != "attachment" && ctype == "text");
        if !inline || ctype != "text" {
            continue;
        }
        let target = match subtype.as_str() {
            "plain" if email.body.is_empty() => &mut email.body,
            "html" if email.html_body.is_empty() => &mut email.html_body,
            _ => continue,
        };
        *target = part.text().unwrap_or_default().into_owned();
    }
    email
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rustls::DigitallySignedStruct;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    /// Test-only verifier for self-signed IMAP test servers.
    #[derive(Debug)]
    pub(crate) struct AcceptAnyCert(pub Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    fn broker(id: &str, email: &str, website: &str) -> Broker {
        Broker {
            id: id.into(),
            name: format!("{id} Inc"),
            email: email.into(),
            website: website.into(),
            ..Default::default()
        }
    }

    pub(crate) fn crlf(s: &str) -> Vec<u8> {
        s.replace('\n', "\r\n").into_bytes()
    }

    #[test]
    fn extract_domain_matches_go() {
        assert_eq!(
            extract_domain("https://www.Spokeo.com/optout"),
            "spokeo.com"
        );
        assert_eq!(extract_domain("http://example.com"), "example.com");
        assert_eq!(extract_domain("radaris.com/x"), "radaris.com");
        assert_eq!(
            extract_domain("https://sub.example.com:8080/a"),
            "sub.example.com:8080"
        );
        // Go only strips lowercase scheme prefixes
        assert_eq!(extract_domain("HTTPS://X.com"), "https:");
        assert_eq!(extract_domain(""), "");
    }

    #[test]
    fn broker_map_email_and_website() {
        let brokers = [
            broker(
                "acme",
                "privacy@ACME-Data.com",
                "https://www.acme.io/privacy",
            ),
            broker("bad", "not-an-email", ""),
            broker("two", "a@b@c.com", ""),
            broker("late", "", "https://acme.io"),
        ];
        let map = build_broker_map(&brokers);
        assert_eq!(map["acme-data.com"].id, "acme");
        assert_eq!(map["acme.io"].id, "late", "later broker overrides");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn domain_lookup_is_exact() {
        let m = Monitor::new(
            InboxConfig::default(),
            &[broker("acme", "privacy@acme.com", "")],
        );
        let raw = crlf("From: Acme <noreply@mail.acme.com>\nSubject: hi\n\nbody\n");
        let e = m.parse_message(5, &raw);
        assert_eq!(e.from_domain, "mail.acme.com");
        assert_eq!(e.broker_id, "", "subdomains do not match (same as Go)");

        let raw = crlf("From: Acme <NoReply@ACME.com>\nSubject: hi\n\nbody\n");
        let e = m.parse_message(5, &raw);
        assert_eq!(
            (e.broker_id.as_str(), e.broker_name.as_str()),
            ("acme", "acme Inc")
        );
        assert_eq!(e.from, "NoReply@ACME.com");
        assert_eq!(e.from_domain, "acme.com");
    }

    #[test]
    fn multipart_alternative_quoted_printable_utf8() {
        let raw = crlf(concat!(
            "From: =?UTF-8?Q?Datenschutz_M=C3=BCller?= <privacy@mueller-daten.de>\n",
            "To: me@example.com\n",
            "Subject: =?UTF-8?Q?L=C3=B6schung_Ihrer_Daten?=\n",
            "Message-ID: <abc123@mueller-daten.de>\n",
            "Date: Mon, 2 Nov 2026 10:30:00 +0100\n",
            "MIME-Version: 1.0\n",
            "Content-Type: multipart/alternative; boundary=\"XYZ\"\n",
            "\n",
            "--XYZ\n",
            "Content-Type: text/plain; charset=utf-8\n",
            "Content-Transfer-Encoding: quoted-printable\n",
            "\n",
            "Gr=C3=BC=C3=9Fe, Ihre Daten wurden gel=C3=B6scht. =\n",
            "Fertig.\n",
            "--XYZ\n",
            "Content-Type: text/html; charset=utf-8\n",
            "Content-Transfer-Encoding: quoted-printable\n",
            "\n",
            "<p>Gr=C3=BC=C3=9Fe</p>\n",
            "--XYZ--\n",
        ));
        let e = parse_raw_email(42, &raw);
        assert_eq!(e.uid, 42);
        assert_eq!(e.subject, "Löschung Ihrer Daten");
        assert_eq!(e.from, "privacy@mueller-daten.de");
        assert_eq!(e.from_name, "Datenschutz Müller");
        assert_eq!(e.from_domain, "mueller-daten.de");
        assert_eq!(e.message_id, "<abc123@mueller-daten.de>");
        assert_eq!(e.received_at.to_rfc3339(), "2026-11-02T09:30:00+00:00");
        assert_eq!(
            e.body.trim_end(),
            "Grüße, Ihre Daten wurden gelöscht. Fertig."
        );
        assert_eq!(e.html_body.trim_end(), "<p>Grüße</p>");
    }

    #[test]
    fn base64_iso_8859_1() {
        // "Sehr geehrte Frau Müller, Ihre Anfrage wurde bearbeitet." in Latin-1
        let latin1: Vec<u8> = "Sehr geehrte Frau Müller, Ihre Anfrage wurde bearbeitet."
            .chars()
            .map(|c| c as u8)
            .collect();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(latin1);
        let raw = crlf(&format!(
            "From: info@daten.ch\nSubject: =?ISO-8859-1?Q?Best=E4tigung?=\n\
             Content-Type: text/plain; charset=ISO-8859-1\nContent-Transfer-Encoding: base64\n\n{b64}\n"
        ));
        let e = parse_raw_email(1, &raw);
        assert_eq!(e.subject, "Bestätigung");
        assert_eq!(e.from_name, "");
        assert_eq!(
            e.body.trim_end(),
            "Sehr geehrte Frau Müller, Ihre Anfrage wurde bearbeitet."
        );
        assert_eq!(e.html_body, "");
    }

    #[test]
    fn html_only_body_is_not_converted_to_text() {
        let raw = crlf(concat!(
            "From: noreply@peoplefinder.com\nSubject: Confirm\n",
            "Content-Type: text/html; charset=\"utf-8\"\n\n",
            "<a href=\"https://peoplefinder.com/confirm?t=1\">Confirm</a>\n",
        ));
        let e = parse_raw_email(1, &raw);
        assert_eq!(e.body, "", "Go leaves Body empty for HTML-only mail");
        assert!(e.html_body.contains("https://peoplefinder.com/confirm?t=1"));
    }

    #[test]
    fn nested_mixed_skips_attachments_and_rfc822() {
        let raw = crlf(concat!(
            "From: a@b.com\nSubject: s\n",
            "Content-Type: multipart/mixed; boundary=OUT\n\n",
            "--OUT\n",
            "Content-Type: text/plain; charset=utf-8\n",
            "Content-Disposition: attachment; filename=a.txt\n\n",
            "attached text\n",
            "--OUT\n",
            "Content-Type: multipart/alternative; boundary=IN\n\n",
            "--IN\n",
            "Content-Type: text/plain\n\n",
            "\n",
            "--IN\n",
            "Content-Type: text/plain\n\n",
            "real body\n",
            "--IN--\n",
            "--OUT\n",
            "Content-Type: message/rfc822\n\n",
            "From: x@y.com\nSubject: inner\nContent-Type: text/html\n\n<b>inner</b>\n",
            "--OUT--\n",
        ));
        let e = parse_raw_email(1, &raw);
        assert_eq!(
            e.body.trim_end(),
            "real body",
            "empty first part skipped like Go"
        );
        assert_eq!(e.html_body, "", "nested message/rfc822 ignored");
    }

    #[test]
    fn missing_headers_and_garbage() {
        let e = parse_raw_email(9, b"");
        assert_eq!(
            e,
            Email {
                uid: 9,
                ..Default::default()
            }
        );
        let e = parse_raw_email(9, &crlf("Subject: x\n\nplain default\n"));
        assert_eq!(e.body.trim_end(), "plain default");
        assert_eq!(e.received_at, DateTime::<Utc>::default());
    }

    /// RFC 8098 MDN as Apple Mail / Thunderbird send it.
    const MDN_REPORT: &str = "From: Privacy Team <privacy@acme.com>
To: me@example.com
Subject: Read: Personal Data Removal Request
Message-ID: <mdn-1@acme.com>
Date: Tue, 10 Nov 2026 10:00:00 +0100
MIME-Version: 1.0
Content-Type: multipart/report; report-type=disposition-notification;
 boundary=\"b1\"

--b1
Content-Type: text/plain; charset=utf-8

The message sent on 10.11.2026 to privacy@acme.com was displayed.

--b1
Content-Type: message/disposition-notification

Reporting-UA: mail.acme.com; Apple Mail
Final-Recipient: rfc822;privacy@acme.com
Original-Message-ID: <AAAAAAAAAAAAAAAAAAAAAA@example.com>
Disposition: manual-action/MDN-sent-manually; displayed

--b1--
";

    #[test]
    fn rfc8098_report_is_a_read_receipt() {
        let e = parse_raw_email(1, &crlf(MDN_REPORT));
        assert_eq!(
            e.read_receipt_for,
            Some(vec!["<AAAAAAAAAAAAAAAAAAAAAA@example.com>".to_string()])
        );
        assert!(is_broker_or_receipt(&e));
        let c = crate::inbox::classifier::classify_response(&e);
        assert_eq!(c.reason, "Read receipt (not a reply)");
        let resp = crate::inbox::response_record(&e, &c);
        assert_eq!(resp.read_receipt_for, e.read_receipt_for);

        let deleted = MDN_REPORT.replace("; displayed", "; deleted");
        let e = parse_raw_email(1, &crlf(&deleted));
        assert_eq!(e.read_receipt_for, Some(vec![]), "receipt, but not read");
    }

    #[test]
    fn outlook_style_read_mail_is_a_read_receipt() {
        let raw = "From: privacy@acme.com
To: me@example.com
Subject: Gelesen: Personal Data Removal Request
In-Reply-To: <AAAAAAAAAAAAAAAAAAAAAA@example.com>
References: <older@example.com> <AAAAAAAAAAAAAAAAAAAAAA@example.com>
Content-Type: text/plain

Ihre Nachricht wurde am 10.11.2026 gelesen.
";
        let e = parse_raw_email(1, &crlf(raw));
        assert_eq!(
            e.read_receipt_for,
            Some(vec![
                "<AAAAAAAAAAAAAAAAAAAAAA@example.com>".to_string(),
                "<older@example.com>".to_string(),
                "<AAAAAAAAAAAAAAAAAAAAAA@example.com>".to_string(),
            ])
        );
        for subject in ["Read: x", "Lu : x"] {
            let r = raw.replace("Gelesen: Personal Data Removal Request", subject);
            assert!(
                parse_raw_email(1, &crlf(&r)).read_receipt_for.is_some(),
                "{subject}"
            );
        }
        // A reply quoting our subject, or a "Read:" mail without references, is not a receipt.
        let reply = raw.replace("Gelesen:", "Re:");
        assert_eq!(parse_raw_email(1, &crlf(&reply)).read_receipt_for, None);
        let unlinked = raw
            .replace("In-Reply-To: <AAAAAAAAAAAAAAAAAAAAAA@example.com>\n", "")
            .replace(
                "References: <older@example.com> <AAAAAAAAAAAAAAAAAAAAAA@example.com>\n",
                "",
            );
        assert_eq!(parse_raw_email(1, &crlf(&unlinked)).read_receipt_for, None);
    }

    #[test]
    fn bounce_detection() {
        let mk = |from: &str, name: &str, subject: &str| Email {
            from: from.into(),
            from_name: name.into(),
            subject: subject.into(),
            ..Default::default()
        };
        assert!(is_bounce(&mk("MAILER-DAEMON@mx.example.com", "", "x")));
        assert!(is_bounce(&mk("x@y.com", "Mail Delivery System", "x")));
        assert!(is_bounce(&mk(
            "x@y.com",
            "",
            "Undeliverable: Removal request"
        )));
        assert!(!is_bounce(&mk("privacy@acme.com", "Acme", "Your request")));
    }

    #[test]
    fn uid_set_and_since() {
        assert_eq!(uid_set(&[1, 2, 3, 7, 9, 10]), "1:3,7,9:10");
        assert_eq!(uid_set(&[5]), "5");
        let s = since_date(7);
        let parsed = chrono::NaiveDate::parse_from_str(&s, "%d-%b-%Y").unwrap();
        assert_eq!(Local::now().date_naive() - parsed, TimeDelta::days(7));
    }

    #[tokio::test]
    async fn not_connected_errors() {
        let mut m = Monitor::new(InboxConfig::default(), &[]);
        for err in [
            m.fetch_recent_emails(1).await.unwrap_err(),
            m.fetch_broker_emails_from_folder("X", 1).await.unwrap_err(),
            m.ensure_folder_exists("X").await.unwrap_err(),
            m.move_to_folder(1, "X").await.unwrap_err(),
            m.archive_emails(&[1], "X").await.unwrap_err(),
        ] {
            assert_eq!(err.to_string(), NOT_CONNECTED);
        }
        assert!(m.disconnect().await.is_ok());
    }

    // ---- fake implicit-TLS IMAP server (self-signed, local only) ----

    const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIBgDCCASWgAwIBAgIUGcBMSfQCmTA/QFcVVN3CPtwnva0wCgYIKoZIzj0EAwIw\nFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTExMDAzNTEwNFoYDzIxMjYxMDE3\nMDM1MTA0WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\nPQMBBwNCAATUvu4s4iPN406m5ErPBC4AQrlMCmc6c+/ihcw3Qjd5wh2PQouA503W\n2fR9tXoBqjFEWmdToQN/zkmdU7ZYrBZbo1MwUTAdBgNVHQ4EFgQUV6nXsAt42gv9\nn/0Y64s18X5+xQMwHwYDVR0jBBgwFoAUV6nXsAt42gv9n/0Y64s18X5+xQMwDwYD\nVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNJADBGAiEA1ugNI4LCyjTj7QuuAJEP\nKfPaLYVPI/1AVCirEhRNohICIQDNIu/Gm0SVWKGmXyVWeIU3PrfqaIWcr0HPW8bp\nzlHJcQ==\n-----END CERTIFICATE-----";
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgXHRI9aW/ZnqrBNBk\nDES5R+EWdFBlkQvsgq2lF/378OehRANCAATUvu4s4iPN406m5ErPBC4AQrlMCmc6\nc+/ihcw3Qjd5wh2PQouA503W2fR9tXoBqjFEWmdToQN/zkmdU7ZYrBZb\n-----END PRIVATE KEY-----";

    #[derive(Clone, Copy)]
    enum Fake {
        Ok,
        /// Closes the connection on LIST.
        DropOnList,
        /// Never answers LIST.
        HangOnList,
        /// Rejects LOGIN.
        LoginNo,
    }

    /// Connection `i` behaves like `script[i]` (the last entry repeats). Returns (port, accepted
    /// connections).
    async fn fake_imap(script: Vec<Fake>) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use rustls::pki_types::PrivateKeyDer;
        use rustls::pki_types::pem::PemObject;
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).unwrap(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = conns.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let i = counter.fetch_add(1, Ordering::SeqCst);
                let mode = script[i.min(script.len() - 1)];
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let (r, mut w) = tokio::io::split(tls);
                    let mut lines = BufReader::new(r).lines();
                    let _ = w.write_all(b"* OK fake IMAP ready\r\n").await;
                    while let Ok(Some(line)) = lines.next_line().await {
                        let (tag, cmd) = line.split_once(' ').unwrap_or((&line, ""));
                        let cmd = cmd.to_ascii_uppercase();
                        let reply = match mode {
                            Fake::LoginNo if cmd.starts_with("LOGIN") => {
                                format!("{tag} NO [AUTHENTICATIONFAILED] invalid\r\n")
                            }
                            Fake::DropOnList if cmd.starts_with("LIST") => return,
                            Fake::HangOnList if cmd.starts_with("LIST") => {
                                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                return;
                            }
                            _ if cmd.starts_with("LIST") => format!(
                                "* LIST (\\HasNoChildren) \"/\" INBOX\r\n{tag} OK LIST done\r\n"
                            ),
                            _ if cmd.starts_with("LOGOUT") => {
                                format!("* BYE\r\n{tag} OK LOGOUT done\r\n")
                            }
                            _ => format!("{tag} OK done\r\n"),
                        };
                        if w.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, conns)
    }

    fn fake_monitor(port: u16) -> Monitor {
        let cfg = InboxConfig {
            server: "127.0.0.1".into(),
            port: port.into(),
            email: "u@example.com".into(),
            password: "pw".into(),
            ..Default::default()
        };
        let mut m = Monitor::new(cfg, &[]);
        m.insecure_tls = true;
        m
    }

    fn count(c: &std::sync::atomic::AtomicUsize) -> usize {
        c.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn reconnects_once_after_dropped_connection() {
        let (port, conns) = fake_imap(vec![Fake::DropOnList, Fake::Ok]).await;
        let mut m = fake_monitor(port);
        m.connect().await.unwrap();
        assert_eq!(m.list_folders().await.unwrap(), vec!["INBOX".to_string()]);
        assert_eq!(count(&conns), 2);
        m.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn reconnects_once_after_timeout_then_gives_up() {
        let (port, conns) = fake_imap(vec![Fake::HangOnList, Fake::Ok]).await;
        let mut m = fake_monitor(port);
        m.op_timeout = std::time::Duration::from_millis(300);
        m.connect().await.unwrap();
        assert_eq!(m.list_folders().await.unwrap(), vec!["INBOX".to_string()]);
        assert_eq!(count(&conns), 2);

        let (port, conns) = fake_imap(vec![Fake::DropOnList]).await;
        let mut m = fake_monitor(port);
        m.connect().await.unwrap();
        let err = m.list_folders().await.unwrap_err();
        assert!(is_connection_error(&err), "{err:#}");
        assert_eq!(count(&conns), 2, "retried only once");
    }

    #[tokio::test]
    async fn login_failure_is_not_retried() {
        let (port, conns) = fake_imap(vec![Fake::LoginNo]).await;
        let mut m = fake_monitor(port);
        let err = m.connect().await.unwrap_err();
        assert!(format!("{err:#}").contains("failed to login"), "{err:#}");
        assert_eq!(count(&conns), 1);
    }

    #[tokio::test]
    async fn connect_retries_refused_connection_once() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = fake_monitor(port).connect().await.unwrap_err();
        assert!(is_connection_error(&err), "{err:#}");
    }

    // ---- integration test against a throwaway GreenMail container ----
    //
    // Run: ERASER_IMAP_IT=1 cargo test --lib inbox::monitor::tests::imap_integration -- --nocapture
    // Requires docker; starts greenmail/standalone:2.1.3 on a random local port.

    pub(crate) struct Container(String);
    impl Drop for Container {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }

    pub(crate) fn start_greenmail() -> (Container, u16) {
        let out = std::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "-p",
                "127.0.0.1::3993",
                "-e",
                "GREENMAIL_OPTS=-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 \
                       -Dgreenmail.users=test:secret@localhost -Dgreenmail.users.login=email",
                "greenmail/standalone:2.1.3",
            ])
            .output()
            .expect("docker run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let container = Container(String::from_utf8(out.stdout).unwrap().trim().to_string());
        let out = std::process::Command::new("docker")
            .args(["port", &container.0, "3993/tcp"])
            .output()
            .unwrap();
        let port = String::from_utf8(out.stdout).unwrap();
        let port = port
            .lines()
            .next()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        (container, port)
    }

    pub(crate) fn it_monitor(port: u16, brokers: &[Broker]) -> Monitor {
        let cfg = InboxConfig {
            enabled: true,
            provider: "imap".into(),
            server: "localhost".into(),
            port: port.into(),
            email: "test@localhost".into(),
            password: "secret".into(),
            folder: "INBOX".into(),
            auto_archive: true,
            archive_folder: "Eraser".into(),
            ..Default::default()
        };
        let mut m = Monitor::new(cfg, brokers);
        m.insecure_tls = true;
        m
    }

    pub(crate) async fn connect_with_retry(m: &mut Monitor) {
        for _ in 0..60 {
            if m.connect().await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        m.connect().await.expect("greenmail never became ready");
    }

    pub(crate) async fn seen_count(m: &mut Monitor, folder: &str) -> usize {
        let s = m.session.as_mut().unwrap();
        s.select(folder).await.unwrap();
        s.uid_search("SEEN").await.unwrap().len()
    }

    fn msg(from: &str, subject: &str, body: &str) -> Vec<u8> {
        crlf(&format!(
            "From: {from}\nTo: test@localhost\nSubject: {subject}\nDate: {}\n\
             Message-ID: <{}@test>\nContent-Type: text/plain; charset=utf-8\n\n{body}\n",
            Utc::now().to_rfc2822(),
            subject.replace(' ', "")
        ))
    }

    #[tokio::test]
    async fn imap_integration() {
        if std::env::var("ERASER_IMAP_IT").as_deref() != Ok("1") {
            eprintln!("skipping imap_integration (set ERASER_IMAP_IT=1)");
            return;
        }
        let (_container, port) = start_greenmail();
        let brokers = [
            broker("acme", "privacy@acme-data.com", ""),
            broker("pf", "", "https://www.peoplefinder.com"),
        ];
        let mut m = it_monitor(port, &brokers);
        connect_with_retry(&mut m).await;

        let html = crlf(concat!(
            "From: People Finder <noreply@peoplefinder.com>\nSubject: Confirm removal\n",
            "Content-Type: multipart/alternative; boundary=B\n\n",
            "--B\nContent-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: quoted-printable\n\n",
            "Bitte best=C3=A4tigen\n--B\nContent-Type: text/html\n\n<a href=\"https://peoplefinder.com/c\">x</a>\n--B--\n",
        ));
        let s = m.session.as_mut().unwrap();
        for raw in [
            msg(
                "Acme <privacy@acme-data.com>",
                "Request received",
                "We got it.",
            ),
            html,
            msg("Friend <friend@example.org>", "Lunch", "hi"),
            msg(
                "Mail Delivery System <MAILER-DAEMON@mx.example.org>",
                "Undeliverable",
                "bounce",
            ),
        ] {
            s.append("INBOX", None, None, raw).await.unwrap();
        }

        // PEEK fetch must not set \Seen
        let peeked = m.fetch_broker_emails_from_folder("INBOX", 7).await.unwrap();
        assert_eq!(peeked.len(), 2);
        assert_eq!(seen_count(&mut m, "INBOX").await, 0);

        let all = m.fetch_recent_emails(7).await.unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(seen_count(&mut m, "INBOX").await, 4, "BODY[] sets \\Seen");

        let broker_emails = m.fetch_broker_emails(7).await.unwrap();
        assert_eq!(broker_emails.len(), 2);
        let pf = broker_emails.iter().find(|e| e.broker_id == "pf").unwrap();
        assert_eq!(pf.body.trim_end(), "Bitte bestätigen");
        assert!(pf.html_body.contains("https://peoplefinder.com/c"));
        assert_eq!(pf.from_name, "People Finder");
        assert!(pf.uid > 0);

        let bounces = m.fetch_bounce_emails(7).await.unwrap();
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].subject, "Undeliverable");

        m.ensure_folder_exists("Eraser").await.unwrap();
        m.ensure_folder_exists("eraser").await.unwrap(); // case-insensitive: no second create
        let uids: Vec<u32> = broker_emails.iter().map(|e| e.uid).collect();
        m.archive_emails(&uids, "Eraser").await.unwrap();
        assert_eq!(m.fetch_recent_emails(7).await.unwrap().len(), 2);
        let archived = m
            .fetch_broker_emails_from_folder("Eraser", 7)
            .await
            .unwrap();
        assert_eq!(archived.len(), 2);

        // move_to_folder acts on the currently selected mailbox
        let friend = m
            .fetch_recent_emails(7)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.from_domain == "example.org")
            .unwrap();
        m.move_to_folder(friend.uid, "Eraser").await.unwrap();
        assert_eq!(m.fetch_recent_emails(7).await.unwrap().len(), 1);
        assert!(m.archive_emails(&[], "Eraser").await.is_ok());
        assert!(m.archive_emails(&[1], "NoSuchFolder").await.is_err());

        // IDLE: a second connection delivers a broker mail; the callback must fire.
        let mut sender = it_monitor(port, &brokers);
        connect_with_retry(&mut sender).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let watch = m.watch_for_new_emails(move |e| tx.send(e).unwrap(), async {
            let _ = stop_rx.await;
        });
        let driver = async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let s = sender.session.as_mut().unwrap();
            s.append(
                "INBOX",
                None,
                None,
                msg("privacy@acme-data.com", "Done", "deleted"),
            )
            .await
            .unwrap();
            let got = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
                .await
                .expect("IDLE callback timed out")
                .unwrap();
            stop_tx.send(()).unwrap();
            got
        };
        let (watch_result, got) = tokio::join!(watch, driver);
        watch_result.unwrap();
        assert_eq!(got.broker_id, "acme");
        assert_eq!(got.subject, "Done");

        // session restored after IDLE
        assert_eq!(m.fetch_broker_emails(7).await.unwrap().len(), 1);
        sender.disconnect().await.unwrap();
        m.disconnect().await.unwrap();
        assert!(m.fetch_recent_emails(1).await.is_err());
    }
}
