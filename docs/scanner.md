## Inbox company scanner

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
