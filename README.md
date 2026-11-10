# Eraser

Take back your privacy. Eraser sends data removal requests to 1,350+ data brokers on your behalf—for free.

You know those sites like Spokeo, BeenVerified, and Whitepages that have your home address, phone number, and family members' names? They're called data brokers, and there are hundreds of them. Services like Incogni and DeleteMe charge $100+/year to send opt-out requests to these companies. Eraser does the same thing, but it's open source and completely free.

### What to Expect

**The good:** Eraser automatically sends removal request emails to 1,350+ data brokers. Many brokers process these requests automatically—you send the email, they remove your data, done.

**The reality:** Some brokers require additional steps. They might send you a confirmation link to click, ask you to fill out a form on their website, or request identity verification. Eraser tracks these responses and shows you exactly what needs manual attention.

**The bottom line:** You're not paying $100+/year, and you're taking real action to protect your privacy. Even with some manual steps, Eraser handles the heavy lifting and gives you a fighting chance against the data broker industry.

---

## The Easy Way (Web Interface)

If you're not comfortable with command-line tools, Eraser has a visual interface that runs in your web browser.

### What You'll Need

1. **Rust** installed on your computer ([download here](https://rustup.rs/))
2. An **iCloud Mail account** to send emails from (with an app-specific password—setup instructions below)

### Getting Started

**Step 1: Download Eraser**

Open your terminal (on Mac, search for "Terminal"; on Windows, use PowerShell) and run:

```bash
git clone https://github.com/tmaeder/eraser.git
cd eraser
cargo build --release && cp target/release/eraser .
```

**Step 2: Start the Web Interface**

```bash
./eraser serve
```

Open your browser and go to `http://localhost:8080`

**Step 3: Complete the Setup Wizard**

The wizard walks you through entering your personal information (the data brokers need this to find your records) and setting up email. Just follow the prompts.

**Step 4: Send Removal Requests**

From the dashboard, you can:
- Browse the list of 1,350+ data brokers
- Send requests one at a time or in bulk
- Track which requests have been sent and their status

That's it. The whole process takes about 10 minutes to set up, and then Eraser handles the rest.

### Setting Up iCloud Mail

Eraser sends removal requests from your iCloud Mail account. Apple requires an **app-specific password** for third-party apps.

**One-time setup (takes 2 minutes):**

1. Make sure two-factor authentication is on for your Apple Account (it is for most accounts)
2. Sign in at [account.apple.com](https://account.apple.com)
3. Go to **Sign-In and Security → App-Specific Passwords**
4. Click **+**, name it "Eraser", and copy the password (looks like `xxxx-xxxx-xxxx-xxxx`)

Use your `@icloud.com` / `@me.com` address plus that password in the setup wizard. You enter the account **once**: the same address and app-specific password are used for sending requests and for reading broker replies. Eraser connects to `smtp.mail.me.com:587` (STARTTLS) and, for inbox monitoring, `imap.mail.me.com:993`.

**Note:** iCloud only sends mail whose *From* address is one of your iCloud addresses, so Eraser uses your iCloud login as the sender.

**Daily sending limits:** iCloud allows ~1,000 emails per day. Eraser chunks large sends across multiple days (250/day) so you stay well under the limit.

**Still want Gmail or Outlook?** Pick it as provider in the setup wizard (or set `account.provider: gmail` in `~/.eraser/config.yaml`) and use an app password. Older configs with `email.smtp` / `inbox` credentials are migrated to `account` automatically on the next save. Settings → Account has a **Test connection** button (SMTP and IMAP login, nothing is sent).

### Autopilot

Eraser can run on its own. Turn it on under **Settings → Autopilot** (off until setup is complete). Every 6 hours on the hosted version (or `eraser autopilot` locally) one round:

1. checks your inbox for broker replies,
2. opens confirmation links (only on the broker's own domain, once per link),
3. sends the next batch of broker requests within the daily limit, plus one reminder to brokers that haven't answered after 30 days,
4. scans your inbox for companies and unsubscribes from one-click mailing lists,
5. sends deletion requests to inbox companies that have been inactive for 12+ months or only send newsletters, never to anything on your keep list (banks, insurers, health insurers, government, your employer, plus your own entries).

Anything it can't do (web forms, ID requests, captchas, plain unsubscribe links) lands on the **Today** page as a to-do. Use **Dry run now** to see what a round would do without contacting anyone, and **Pause** to stop it.

### Status and tracking

The **Today** page shows funnels (sent → opened → replied → deleted), a 30-day deadline bar per request, sending progress, a breakdown by region and the recent autopilot rounds. Each broker has a detail page with a timeline.

With `email.tracking: true` (Settings → Account, off by default because iCloud rejected tracked mail), every mail asks for a read receipt and, on the hosted version, contains a tracking pixel; opens via Apple's or Google's image proxies are shown separately because they don't prove the mail was read. Legally, the 30-day deadline runs from receipt, not from opening.

### Inbox company scanner

Eraser can find the companies that email you and help you get your data deleted or unsubscribe —
beyond the data brokers in `data/brokers.yaml`.

```bash
eraser scan                      # last 365 days of INBOX, Archive, archive folder, Newsletter/Promotions
eraser scan --days 90 --folders INBOX,Archive
eraser companies --lists         # mailing lists only (--manual: no published privacy address, --all)
eraser request shop.example --dry-run
eraser unsubscribe shop.example news.example
```

In the web UI the same lives under **Companies** (scan with range selector, filters, bulk
"Send deletion request" / "Unsubscribe" with a confirmation step listing every address/URL).

How it works:

- **Headers only.** The scan `EXAMINE`s each folder and fetches
  `BODY.PEEK[HEADER.FIELDS (FROM REPLY-TO LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST LIST-ID PRECEDENCE AUTO-SUBMITTED DATE MESSAGE-ID)]`
  in batches. Bodies are never downloaded and nothing is marked as read. Rescans only add new
  messages (deduplicated by Message-ID).
- **Grouping.** Senders are grouped by registrable domain (`news.shop.co.uk` → `shop.co.uk`).
  Your own addresses/domains and personal mail providers (gmail.com, icloud.com, gmx.*, …) are
  excluded unless the mail carries mailing-list headers; they stay visible under "Excluded".
  Folders are configurable with `inbox.scan_folders` in config.yaml.
- **Privacy contacts are never guessed.** Eraser uses the broker database entry for the domain,
  otherwise an address published on the company's own website (privacy policy, Datenschutz,
  confidentialité, … pages; Impressum as last resort) on the company's domain, with the page URL
  recorded as source. Otherwise the company is marked "manual". The crawler identifies itself,
  honours robots.txt, makes at most 6 requests per company (10 s timeout) and checks 4 companies
  at a time.
- **Deletion requests** use your configured template (`auto` picks FADP for .ch/.li, GDPR for
  EU/EEA ccTLDs and .eu, generic otherwise), the normal rate/daily limits, and appear in History
  as `company:<domain>`. Replies are picked up by `monitor` / the Pipeline inbox scan.
- **Unsubscribe** uses RFC 8058 one-click (HTTPS POST, no cookies, ≤ 3 redirects) or the
  `mailto:` address (subject/body from the URI). Plain unsubscribe links are never opened
  automatically — the UI shows them as a button and you mark them done.
- Nothing is contacted without your explicit confirmation; every action is logged per company.

### Brokers without an email address

Some brokers only offer a web opt-out form. Eraser can't email those, so they are listed with their opt-out links in [docs/manual-optouts.md](docs/manual-optouts.md) as a checklist to work through by hand.

### European and Swiss Brokers

The database includes 500+ European and Swiss address brokers, credit agencies (SCHUFA, CRIF, Experian, Equifax, Creditreform, infoscore…) and ad-tech data companies, imported from the CC0 [Datenanfragen.de company database](https://github.com/datenanfragen/data). They use `region: eu` or `region: ch`. The 51 Swiss entries also cover collection agencies, loyalty programs (Migros Cumulus, Coop Supercard, Manor), media groups and market-research panels; every Swiss address was taken from the company's own privacy policy.

**Swiss tip:** also sign up for the free SDV Robinson list (no unsolicited advertising mail) at [sdv-konsumenteninfo.ch/robinsonliste](https://sdv-konsumenteninfo.ch/robinsonliste/). It does not accept requests by email, so Eraser can't do it for you. To target only those, set:

### Manual opt-outs (web form only)

These companies hold data on Europeans but only accept requests through their own web form, so Eraser can't email them. Submit these yourself:

| Company | Form |
|---|---|
| InfobelPRO | https://dpo.infobel.com/ |
| BoldData | https://www.boldata.com/email/info |
| INSEE SIRENE opt-out (official online process) | https://www.insee.fr/fr/information/7456564 |
| Experian Spain (Badexcug) | https://portaldelconsumidor.experian.es |
| Banco de Portugal (CRC) | https://www.bportugal.pt/area-cidadao/formulario/227 |
| BG Informatik GmbH | http://www.bg-informatik.ch/kontakt/ |
| getstone informatik GmbH | https://www.getstone.ch/kontakt/ |
| Royal Mail (home-mover / Redirection data) | https://privacyportal-de.onetrust.com/webform/e1491f29-851b-4350-bb98-8faa269b4189/efa508f1-1c18-46e2-920f-93d44be4554f |

**Official registers** (INSEE/SIRENE, Bolagsverket, Brønnøysund, CVR, KRS, justice.cz, Banque de France, SISBON, Kreditni biro, CORPME, IRN…) are tagged `category: registry`. They are usually legally required to keep this data, so expect refusals. To skip them, list their ids under `options.excluded_brokers`.

```yaml
options:
  template: gdpr
  regions: [eu, ch]
```

---

## For Developers: CLI Usage

If you prefer the command line, Eraser has a full CLI.

### Installation

```bash
git clone https://github.com/tmaeder/eraser.git
cd eraser
cargo build --release && cp target/release/eraser .
```

### Quick Start

```bash
# Interactive setup
./eraser init

# Preview what would be sent (no emails go out)
./eraser send --dry-run

# Send removal requests to all brokers
./eraser send

# Check your history
./eraser status
```

### Commands

| Command | What it does |
|---------|--------------|
| `eraser init` | Interactive config setup |
| `eraser send` | Send removal requests |
| `eraser send --dry-run` | Preview without sending |
| `eraser list-brokers` | Show all 1,350+ brokers |
| `eraser status` | View history and stats |
| `eraser status --limit 50` | Show more history |
| `eraser add-broker` | Add a custom broker |
| `eraser serve` | Start web interface |
| `eraser serve -p 3000` | Web interface on custom port |

### Configuration File

Your config lives at `~/.eraser/config.yaml`. Here's the full schema:

```yaml
profile:
  first_name: Jane
  last_name: Doe
  email: jane@example.com
  # Optional but helps brokers find your records
  address: "123 Main Street"
  city: "San Francisco"
  state: "CA"
  zip_code: "94102"
  country: "USA"
  phone: "+1-555-123-4567"
  date_of_birth: "1990-01-15"

# One account sends the requests (SMTP) and reads the replies (IMAP)
account:
  provider: icloud  # icloud | gmail | outlook | custom
  email: jane@icloud.com  # also the From address
  password: xxxx-xxxx-xxxx-xxxx  # app-specific password (encrypted at rest with ERASER_CONFIG_KEY)
  # custom only: smtp_host, smtp_port (587 = STARTTLS), imap_host, imap_port

email:
  tracking: false  # open tracking (own Message-ID, read receipt, pixel); may cause iCloud rejections

inbox:  # optional, options only (credentials come from account)
  enabled: true
  folder: INBOX
  auto_archive: false
  archive_folder: Eraser

options:
  template: generic  # or "gdpr", "ccpa", "fadp" (Swiss)
  rate_limit_ms: 2000  # delay between emails

  # Optional: only target specific regions
  # regions:
  #   - us
  #   - global

  # Optional: skip specific brokers
  # excluded_brokers:
  #   - spokeo
  #   - whitepages
```

### Email Templates

Eraser includes three templates:

- **GDPR** — Invokes Article 17 "Right to Erasure" under EU law
- **CCPA** — Invokes California Consumer Privacy Act rights
- **Generic** — References multiple privacy laws, works anywhere

The generic template is a good default if you're not sure.

### Adding Brokers

The broker database is at `data/brokers.yaml`. To add one:

```yaml
- id: example-broker
  name: Example Broker
  email: privacy@example.com
  website: https://example.com
  opt_out_url: https://example.com/optout
  region: us  # us, eu, or global
  category: people-search  # people-search, marketing, or background-check
```

Or use the interactive command:

```bash
./eraser add-broker
```

---

## Run It Hosted (Cloudflare)

Instead of keeping your Mac on, you can run Eraser in a Cloudflare Container. The autopilot then runs every 6 hours on its own. The `deploy/` folder has everything:

- a Worker in front of the app with a login page (signed cookie, rate-limited), plus basic auth for scripts
- `/internal/*` blocked from the internet; a cron trigger calls the autopilot every 6 hours
- your `~/.eraser` (config + history) saved to a private R2 bucket every 60 seconds and on shutdown
- the app password in the config encrypted with a key that only lives in a Cloudflare secret

**Setup (one time):**

1. Install [Docker](https://www.docker.com/) and Node.js, and log in with `npx wrangler login`
2. Create the R2 bucket: `npx wrangler r2 bucket create eraser-state`
3. Point `routes` in `deploy/wrangler.jsonc` at your own domain
4. Set three secrets (`cd deploy`, then `npx wrangler secret put <NAME>`; generate each value with `openssl rand -base64 32` and keep a copy in your password manager):
   - `ERASER_PASSWORD`: your login password for the web UI
   - `ERASER_AUTOPILOT_TOKEN`: lets the cron trigger start autopilot rounds
   - `ERASER_CONFIG_KEY`: encrypts the app password in the config (if you lose it, you only re-enter the app password)
5. Deploy: `cd deploy && npm install && npx wrangler deploy`

Open your domain, sign in, finish the setup wizard, then turn on the autopilot under **Settings → Autopilot** (try **Dry run now** on the Today page first).

Requires a Cloudflare Workers paid plan (Containers). The container sleeps when idle, so it only runs a few minutes per day.

---

## Security Notes

- **Your config file contains personal data.** Don't commit it to git. The file is created with restricted permissions (readable only by you).
- **Use app passwords, not your real password.** For iCloud and Gmail, this is required. For other providers, it's still a good idea.
- **Consider using a dedicated email.** This keeps your removal request activity separate from your main inbox.

---

## Does This Actually Work?

Yes, with caveats:

- **Most brokers comply.** They're legally required to under GDPR (EU) and CCPA (California). Even brokers not covered by these laws often honor requests to avoid liability.
- **Some require manual steps.** About 20-30% of brokers will respond asking you to:
  - Click a confirmation link (Eraser detects these and shows them to you)
  - Fill out an opt-out form on their website (Eraser tracks these as "pending tasks")
  - Verify your identity via email reply
- **It's not instant.** Brokers have up to 30-45 days to process requests (varies by law). Some are faster.
- **You'll need to repeat this.** Data brokers buy and sell data continuously. Running Eraser monthly keeps you off their lists.

**The Pipeline view** in Eraser's web UI shows you exactly which brokers need manual attention. It's not fully automated, but it's free—and it does the tedious work of sending 750+ emails and tracking responses for you.

---

## How It Compares

| Service | Price | Brokers | Open Source |
|---------|-------|---------|-------------|
| **Eraser** | Free | 750+ | Yes |
| Incogni | $77/year | 180+ | No |
| DeleteMe | $129/year | 750+ | No |
| Privacy Duck | $500+/year | 500+ | No |

---

## Contributing

Contributions are welcome. The most helpful things:

- **Adding brokers** — The database at `data/brokers.yaml` can always use more entries
- **Template improvements** — Better wording for removal requests
- **Bug fixes** — Found something broken? PRs welcome
- **Documentation** — Typos, clarifications, better examples

---

## Project Structure

```
eraser/
├── src/
│   ├── main.rs, cli/        # CLI (clap)
│   ├── config.rs            # Configuration (~/.eraser/config.yaml)
│   ├── broker.rs            # Broker loading and filtering
│   ├── template.rs          # Request templates (GDPR, CCPA, FADP, generic, auto)
│   ├── email.rs             # SMTP sender (iCloud STARTTLS, implicit TLS)
│   ├── history/             # SQLite request, response and scanner tracking
│   ├── inbox/               # IMAP monitor, response classifier, URL parser
│   ├── scanner/             # Inbox company scanner, contact discovery, unsubscribe
│   ├── browser/             # Opt-out form filling (Chrome), confirmation links
│   └── web/                 # Web UI (axum, minijinja, CSRF, send jobs)
├── templates/               # Email and web templates (embedded in the binary)
├── static/                  # CSS/JS (embedded)
├── data/brokers.yaml        # 1,350+ broker database (US, EU, CH)
├── deploy/                  # Cloudflare Containers deployment (Worker + Dockerfile)
└── config.example.yaml      # Example configuration
```

Run the tests with `cargo test`.

---

## License

MIT — do whatever you want with it.

---

## Disclaimer

This tool sends legitimate data removal requests based on privacy laws. It's not legal advice. Not all brokers are required to comply with all requests, and response times vary. But it works, and it's free.
