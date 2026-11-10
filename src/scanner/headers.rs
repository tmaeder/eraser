//! Header-block parsing for the company scanner (RFC 5322 folding, RFC 2369 List-Unsubscribe,
//! RFC 8058 one-click).

use super::domain::registrable_domain;
use chrono::{DateTime, Utc};
use mail_parser::MessageParser;

/// What the scanner keeps from one message's headers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeaderInfo {
    /// Message-ID, or `folder/uidvalidity/uid` when missing (dedupes rescans).
    pub key: String,
    pub from_addr: String,
    pub from_name: String,
    /// Registrable domain of the From address.
    pub domain: String,
    pub date: Option<DateTime<Utc>>,
    pub is_list: bool,
    /// First mailto: URI of List-Unsubscribe (empty if none).
    pub unsub_mailto: String,
    /// First https: URI of List-Unsubscribe (empty if none).
    pub unsub_https: String,
    /// RFC 8058 `List-Unsubscribe-Post: List-Unsubscribe=One-Click`.
    pub one_click: bool,
}

/// Unfolded values of every `name:` header (case-insensitive) in a raw header block.
pub fn header_values(raw: &str, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in raw.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            if let Some(v) = current.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some(v) = current.take() {
            out.push(v);
        }
        if let Some((n, v)) = line.split_once(':')
            && n.trim().eq_ignore_ascii_case(name)
        {
            current = Some(v.trim().to_string());
        }
    }
    out.extend(current);
    out
}

/// RFC 2369: `<uri>` items separated by commas; whitespace inside brackets is ignored.
/// Returns (mailto URIs, https URIs). Other schemes (http:, ftp: …) are dropped.
pub fn parse_list_unsubscribe(value: &str) -> (Vec<String>, Vec<String>) {
    let (mut mailto, mut https) = (Vec::new(), Vec::new());
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        let uri: String = rest[start + 1..start + len]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        rest = &rest[start + len + 1..];
        let lower = uri.to_ascii_lowercase();
        if lower.starts_with("mailto:") {
            mailto.push(uri);
        } else if lower.starts_with("https://") {
            https.push(uri);
        }
    }
    (mailto, https)
}

/// RFC 8058: the POST header must be exactly the one-click key/value (case-insensitive, trimmed).
pub fn is_one_click(post_value: &str) -> bool {
    post_value
        .trim()
        .eq_ignore_ascii_case("List-Unsubscribe=One-Click")
}

/// Parses a header block. `fallback_key` is used when there is no Message-ID.
/// Returns None when there is no usable From address.
pub fn parse_header_block(raw: &[u8], fallback_key: &str) -> Option<HeaderInfo> {
    let text = String::from_utf8_lossy(raw);
    let msg = MessageParser::default().parse_headers(raw)?;
    let from = msg.from()?.first()?;
    let from_addr = from.address()?.trim().to_ascii_lowercase();
    let (_, host) = from_addr.rsplit_once('@')?;
    let domain = registrable_domain(host);
    if domain.is_empty() {
        return None;
    }
    let first = |n: &str| {
        header_values(&text, n)
            .into_iter()
            .next()
            .unwrap_or_default()
    };
    let (mailto, https) = header_values(&text, "List-Unsubscribe")
        .iter()
        .map(|v| parse_list_unsubscribe(v))
        .fold((Vec::new(), Vec::new()), |mut acc, (m, h)| {
            acc.0.extend(m);
            acc.1.extend(h);
            acc
        });
    let precedence = first("Precedence").to_ascii_lowercase();
    let is_list = !first("List-Id").is_empty()
        || !mailto.is_empty()
        || !https.is_empty()
        || !first("List-Unsubscribe").is_empty()
        || matches!(precedence.as_str(), "bulk" | "list");
    let one_click = !https.is_empty()
        && header_values(&text, "List-Unsubscribe-Post")
            .iter()
            .any(|v| is_one_click(v));
    Some(HeaderInfo {
        key: msg
            .message_id()
            .map(|id| format!("<{id}>"))
            .unwrap_or_else(|| fallback_key.to_string()),
        from_name: from.name().unwrap_or_default().trim().to_string(),
        from_addr,
        domain,
        date: msg
            .date()
            .filter(|d| d.is_valid())
            .and_then(|d| DateTime::<Utc>::from_timestamp(d.to_timestamp(), 0)),
        is_list,
        unsub_mailto: mailto.into_iter().next().unwrap_or_default(),
        unsub_https: https.into_iter().next().unwrap_or_default(),
        one_click,
    })
}

/// A mailto: URI split into recipient and the subject/body hfields (RFC 6068).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mailto {
    pub to: String,
    pub subject: String,
    pub body: String,
}

pub fn parse_mailto(uri: &str) -> Option<Mailto> {
    let rest = uri.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:"))?;
    let rest = &uri[rest.len()..];
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let decode = |s: &str| {
        url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    let mut m = Mailto {
        to: decode(to),
        ..Default::default()
    };
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k.to_ascii_lowercase().as_str() {
            "subject" => m.subject = decode(v),
            "body" => m.body = decode(v),
            _ => {}
        }
    }
    (m.to.contains('@') && !m.to.contains(',')).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEWSLETTER: &str = concat!(
        "From: =?UTF-8?Q?Caf=C3=A9_M=C3=BCller?= <News@Mail.Cafe-Mueller.co.uk>\r\n",
        "Reply-To: service@cafe-mueller.co.uk\r\n",
        "List-Unsubscribe: <mailto:unsub@cafe-mueller.co.uk?subject=Unsubscribe%20me>,\r\n",
        " <https://cafe-mueller.co.uk/unsub?u=1&\r\n",
        "\tt=2>, <http://insecure.example/x>\r\n",
        "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
        "List-ID: Cafe News <news.cafe-mueller.co.uk>\r\n",
        "Date: Mon, 2 Nov 2026 10:30:00 +0100\r\n",
        "Message-ID: <abc@cafe-mueller.co.uk>\r\n\r\n",
    );

    #[test]
    fn folded_multi_uri_list_unsubscribe() {
        let h = parse_header_block(NEWSLETTER.as_bytes(), "INBOX/1/1").unwrap();
        assert_eq!(h.from_addr, "news@mail.cafe-mueller.co.uk");
        assert_eq!(h.from_name, "Café Müller", "RFC 2047 encoded display name");
        assert_eq!(h.domain, "cafe-mueller.co.uk");
        assert_eq!(h.key, "<abc@cafe-mueller.co.uk>");
        assert!(h.is_list && h.one_click);
        assert_eq!(
            h.unsub_mailto,
            "mailto:unsub@cafe-mueller.co.uk?subject=Unsubscribe%20me"
        );
        assert_eq!(
            h.unsub_https, "https://cafe-mueller.co.uk/unsub?u=1&t=2",
            "folding whitespace inside <> is removed"
        );
        assert_eq!(h.date.unwrap().to_rfc3339(), "2026-11-02T09:30:00+00:00");
    }

    #[test]
    fn list_unsubscribe_variants() {
        let (m, h) = parse_list_unsubscribe("<https://a.example/u>,<mailto:x@a.example>");
        assert_eq!((m.len(), h.len()), (1, 1));
        let (m, h) =
            parse_list_unsubscribe("garbage <ftp://x> <HTTPS://B.example/U> <unterminated");
        assert!(m.is_empty(), "{m:?}");
        assert_eq!(h, ["HTTPS://B.example/U"]);
        assert!(is_one_click(" list-unsubscribe=one-click "));
        assert!(!is_one_click("List-Unsubscribe=One-Click; other=1"));
    }

    #[test]
    fn personal_mail_and_precedence() {
        let raw = "From: Friend <friend@gmail.com>\r\nDate: Tue, 3 Nov 2026 08:00:00 +0000\r\n\r\n";
        let h = parse_header_block(raw.as_bytes(), "INBOX/7/42").unwrap();
        assert_eq!(h.key, "INBOX/7/42");
        assert!(!h.is_list && !h.one_click);
        assert_eq!(h.from_name, "Friend");

        let raw = "From: alerts@bank.ch\r\nPrecedence: bulk\r\n\r\n";
        assert!(parse_header_block(raw.as_bytes(), "k").unwrap().is_list);

        // One-click requires an https URI
        let raw = "From: a@shop.de\r\nList-Unsubscribe: <mailto:u@shop.de>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\r\n";
        let h = parse_header_block(raw.as_bytes(), "k").unwrap();
        assert!(h.is_list && !h.one_click);

        assert!(parse_header_block(b"Subject: no from\r\n\r\n", "k").is_none());
    }

    #[test]
    fn header_values_unfolds_and_matches_case_insensitively() {
        let raw = "list-id: a\r\n b\r\nX: y\r\nLIST-ID: c\r\n";
        assert_eq!(header_values(raw, "List-Id"), ["a b", "c"]);
    }

    #[test]
    fn mailto_params() {
        let m =
            parse_mailto("mailto:unsub@x.com?subject=Bye%20now&body=remove+me%0Aplease").unwrap();
        assert_eq!(m.to, "unsub@x.com");
        assert_eq!(m.subject, "Bye now");
        assert_eq!(m.body, "remove+me\nplease", "'+' is literal in mailto URIs");
        assert_eq!(parse_mailto("MAILTO:a%40b.com").unwrap().to, "a@b.com");
        assert!(parse_mailto("mailto:a@b.com,c@d.com").is_none());
        assert!(parse_mailto("https://x").is_none());
    }
}
