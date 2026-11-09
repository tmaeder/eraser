# Autopilot design (2026-11-10)

Goal: less manual work. Eraser runs on its own on erase.kampa.app; the user only handles the few items it cannot do, shown on the dashboard. No email or push digest.

## Approach

Cloudflare Cron Trigger wakes the hosted container every 6 hours and starts one autopilot round. The container sleeps in between. The same round runs locally with `eraser autopilot [--dry-run]`.

Rejected: always-on in-process timer (container runs 24/7, costs more); macOS launchd job (needs the Mac awake, hosted copy stays manual).

## One round

Each step can be switched off in Settings → Autopilot.

1. **Check inbox**: fetch and classify new replies (existing monitor + classifier).
2. **Confirm links**: open confirmation links only on the broker's own domain, at most one click per link, log the result. Captchas and ID requests are never handled; they become to-dos.
3. **Broker requests**: send the next batch up to the daily limit (250 for SMTP). A broker with no reply after 30 days gets exactly one reminder citing the deadline, then is marked "no response".
4. **Inbox company scan**: incremental header-only scan (existing scanner); contact discovery only for new companies.
5. **One-click unsubscribe**: RFC 8058 lists only; plain unsubscribe links become to-dos.
6. **Requests to inbox companies**, only if all hold:
   - inactive for 12+ months, or newsletter-only sender;
   - verified privacy contact;
   - not on the keep list.
7. **Round report**: stored and shown on the dashboard.

Safety rails: Autopilot master switch, off by default until setup is complete; a Pause button; only one round at a time (lock).

## Keep list

Pre-filled and editable:
- built-in list of CH/EU banks, insurers and health insurers (UBS, PostFinance, Raiffeisen, ZKB, CSS, Helsana, SWICA, AXA, Mobiliar, …);
- government domains (`.admin.ch`, cantonal `.ch` government domains, `.gov`, `.gv.at`, …);
- domains or sender names containing bank/insurance/versicherung/krankenkasse/clinic/klinik/arzt/steuer;
- the user's own email domain (employer).

## Dashboard

New start page "Today":
1. **Status**: autopilot on/off, last round, next round, Pause button.
2. **To do**: only items needing the user, sorted by deadline, 10 shown plus "show all". Each item has one action button (open form / show request / open link) plus Done / Skip (hide 30 days). Kinds: web-form broker, ID requested, captcha/blocked confirm link, plain unsubscribe link.
3. **Progress**: brokers contacted / confirmed deleted / rejected / waiting (bar), same for inbox companies (requested / unsubscribed).
4. **Activity**: last 5 rounds with one-line summaries.

Settings → Autopilot: per-step switches, reminder days (30), inactivity months (12), keep list editor.

## Implementation

- `src/autopilot/`: `run_round(state, opts) -> RoundReport`, reusing sender, monitor, scanner, browser confirm handler, job rate/daily limits.
- DB:
  - `autopilot_rounds` table (started_at, finished_at, dry_run, report JSON);
  - `pending_tasks` reused for to-dos, new kinds `id_required` and `unsubscribe_link`, plus `skipped_until`;
  - `removal_requests.reminder_sent_at` column.
- `templates/email/reminder.txt`: refers to the original request date and the 30-day deadline (GDPR Art. 12(3), Swiss FADP); language/law via the same "auto" selection.
- Config: `autopilot:` section (`enabled: false`, per-step booleans, `reminder_days: 30`, `inactive_months: 12`, `keep_domains: []`).
- Trigger: `POST /internal/autopilot/tick` with header `X-Autopilot-Token` = env `ERASER_AUTOPILOT_TOKEN` (constant-time compare); returns 202 and runs the round in the background; 409 if a round is running. CLI: `eraser autopilot [--dry-run]`.
- Worker:
  - `scheduled` handler with cron `0 */6 * * *` calls the container endpoint with the token;
  - `fetch` returns 404 for `/internal/*` from the internet;
  - the token is a Worker secret passed to the container as an env var and stored in the user's Keychain.

## Tests

- round with mock sender + GreenMail;
- dry run sends nothing;
- lock prevents parallel rounds;
- keep list blocks requests;
- reminder sent at most once and only after N days;
- tick without/with wrong token refused;
- Worker blocks `/internal/*`.

## Order

1. Autopilot module + tests
2. Dashboard + settings
3. Worker scheduler
4. Deploy
5. One live dry-run round
