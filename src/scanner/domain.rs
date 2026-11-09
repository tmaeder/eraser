//! Domain helpers: registrable domain (small built-in public-suffix list), personal mail
//! providers, TLD → legal region.

/// Two-label public suffixes we care about. Anything else is treated as a one-label TLD.
// ponytail: built-in suffix subset, swap in the `psl` crate if odd ccTLDs show up in real inboxes.
const MULTI_PART_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "sch.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "co.nz", "org.nz", "net.nz", "co.jp",
    "ne.jp", "or.jp", "ac.jp", "go.jp", "gv.at", "co.at", "or.at", "ac.at", "com.br", "com.mx",
    "co.za", "com.cn", "com.tr", "co.in", "co.kr", "com.sg", "com.hk", "co.il", "com.ar", "com.tw",
    "com.my", "co.id", "co.th", "com.pl", "com.es", "com.pt", "com.gr",
];

/// `mail.news.shop.co.uk` → `shop.co.uk`. Input is lowercased; ports/trailing dots are dropped.
pub fn registrable_domain(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or_default();
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    let last_two = labels[labels.len() - 2..].join(".");
    let keep = if MULTI_PART_SUFFIXES.contains(&last_two.as_str()) {
        3
    } else {
        2
    };
    labels[labels.len().saturating_sub(keep)..].join(".")
}

/// The label in front of the public suffix (`shop` for `shop.co.uk`).
pub fn brand_label(domain: &str) -> &str {
    domain.split('.').next().unwrap_or_default()
}

/// Same registrable domain, or the same brand label under another suffix
/// (`zalando.de` ↔ `zalando.ch`). Short brand labels never count as related.
pub fn is_related_domain(candidate: &str, company: &str) -> bool {
    let a = registrable_domain(candidate);
    let b = registrable_domain(company);
    if a == b {
        return true;
    }
    let (la, lb) = (brand_label(&a), brand_label(&b));
    la == lb && la.len() >= 4
}

const PERSONAL_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "web.de",
    "t-online.de",
    "bluewin.ch",
    "hispeed.ch",
    "sunrise.ch",
    "proton.me",
    "pm.me",
    "mail.com",
    "gmx.net",
    "posteo.de",
    "mailbox.org",
    "freenet.de",
    "orange.fr",
    "free.fr",
    "wanadoo.fr",
    "libero.it",
    "yandex.ru",
    "zoho.com",
    "fastmail.com",
    "tutanota.com",
    "tuta.io",
    "msn.com",
    "ymail.com",
    "rocketmail.com",
];

/// Brand labels that are personal mail providers under any suffix (hotmail.co.uk, gmx.ch …).
const PERSONAL_BRANDS: &[&str] = &["hotmail", "live", "yahoo", "gmx", "protonmail", "outlook"];

/// Freemail / ISP domains where senders are individuals, not companies.
pub fn is_personal_domain(domain: &str) -> bool {
    let d = registrable_domain(domain);
    PERSONAL_DOMAINS.contains(&d.as_str()) || PERSONAL_BRANDS.contains(&brand_label(&d))
}

const EU_EEA_TLDS: &[&str] = &[
    "eu", "at", "be", "bg", "hr", "cy", "cz", "dk", "ee", "fi", "fr", "de", "gr", "hu", "ie", "it",
    "lv", "lt", "lu", "mt", "nl", "pl", "pt", "ro", "sk", "si", "es", "se", "is", "no",
];

/// Region for template "auto": `.ch`/`.li` → ch (FADP), EU/EEA ccTLDs + `.eu` → eu (GDPR),
/// everything else → global.
pub fn tld_region(domain: &str) -> &'static str {
    let tld = domain.rsplit('.').next().unwrap_or_default();
    match tld {
        "ch" | "li" => "ch",
        t if EU_EEA_TLDS.contains(&t) => "eu",
        _ => "global",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrable_domains() {
        for (host, want) in [
            ("news.shop.example.com", "example.com"),
            ("Mail.Shop.CO.UK", "shop.co.uk"),
            ("a.b.bbc.co.uk", "bbc.co.uk"),
            ("x.ox.ac.uk", "ox.ac.uk"),
            ("mail.woolworths.com.au", "woolworths.com.au"),
            ("e.rakuten.co.jp", "rakuten.co.jp"),
            ("post.bmf.gv.at", "bmf.gv.at"),
            ("example.com", "example.com"),
            ("example.com.", "example.com"),
            ("localhost", "localhost"),
            ("co.uk", "co.uk"),
            ("shop.de:8080", "shop.de"),
        ] {
            assert_eq!(registrable_domain(host), want, "{host}");
        }
    }

    #[test]
    fn personal_and_related() {
        for d in [
            "gmail.com",
            "mail.icloud.com",
            "hotmail.co.uk",
            "hotmail.de",
            "yahoo.fr",
            "gmx.ch",
            "protonmail.com",
            "proton.me",
            "bluewin.ch",
            "outlook.com",
        ] {
            assert!(is_personal_domain(d), "{d}");
        }
        for d in ["zalando.de", "migros.ch", "news.apple.com", "liveshop.com"] {
            assert!(!is_personal_domain(d), "{d}");
        }
        assert!(is_related_domain("privacy.zalando.ch", "zalando.de"));
        assert!(is_related_domain(
            "dpo@x.shop.co.uk".split('@').nth(1).unwrap(),
            "shop.co.uk"
        ));
        assert!(!is_related_domain("onetrust.com", "zalando.de"));
        assert!(
            !is_related_domain("ab.ch", "ab.de"),
            "short brand labels are not related"
        );
    }

    #[test]
    fn regions() {
        assert_eq!(tld_region("migros.ch"), "ch");
        assert_eq!(tld_region("bank.li"), "ch");
        assert_eq!(tld_region("zalando.de"), "eu");
        assert_eq!(tld_region("europa.eu"), "eu");
        assert_eq!(tld_region("finn.no"), "eu");
        assert_eq!(tld_region("bmf.gv.at"), "eu");
        assert_eq!(tld_region("amazon.com"), "global");
        assert_eq!(tld_region("bbc.co.uk"), "global");
    }
}
