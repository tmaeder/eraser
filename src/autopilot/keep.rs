//! Keep list: companies the autopilot never contacts.

use crate::config::Config;
use crate::scanner::Identity;
use crate::scanner::domain::registrable_domain;

/// Built-in Swiss and EU banks, insurers and health insurers (registrable domains).
pub const BUILTIN_DOMAINS: &[&str] = &[
    // Swiss banks
    "ubs.com",
    "credit-suisse.com",
    "postfinance.ch",
    "raiffeisen.ch",
    "zkb.ch",
    "bcv.ch",
    "bcge.ch",
    "lukb.ch",
    "bekb.ch",
    "valiant.ch",
    "migrosbank.ch",
    "cler.ch",
    "yuh.com",
    "neon-free.ch",
    "swissquote.ch",
    "juliusbaer.com",
    "vontobel.com",
    "twint.ch",
    // Swiss insurers and health insurers
    "css.ch",
    "helsana.ch",
    "swica.ch",
    "sanitas.com",
    "visana.ch",
    "concordia.ch",
    "groupemutuel.ch",
    "kpt.ch",
    "assura.ch",
    "sympany.ch",
    "oekk.ch",
    "atupri.ch",
    "egk.ch",
    "axa.ch",
    "mobiliar.ch",
    "zurich.ch",
    "allianz.ch",
    "generali.ch",
    "helvetia.com",
    "baloise.ch",
    "vaudoise.ch",
    "swisslife.ch",
    "suva.ch",
    // EU banks, insurers and health insurers
    "axa.com",
    "zurich.com",
    "allianz.de",
    "allianz.com",
    "ergo.de",
    "huk.de",
    "debeka.de",
    "generali.com",
    "deutsche-bank.de",
    "commerzbank.de",
    "sparkasse.de",
    "ing.de",
    "ing.com",
    "dkb.de",
    "comdirect.de",
    "n26.com",
    "revolut.com",
    "bawag.at",
    "erstebank.at",
    "raiffeisen.at",
    "bnpparibas.com",
    "societegenerale.fr",
    "credit-agricole.fr",
    "abnamro.nl",
    "rabobank.nl",
    "santander.com",
    "bbva.com",
    "unicredit.it",
    "intesasanpaolo.com",
    "tk.de",
    "aok.de",
    "barmer.de",
    "dak.de",
];

/// Government domains and suffixes (".admin.ch", ".gov", ".gv.at", …).
pub const GOVERNMENT_SUFFIXES: &[&str] = &[
    "admin.ch",
    "ch.ch",
    "gov",
    "mil",
    "gv.at",
    "gov.uk",
    "gouv.fr",
    "bund.de",
    "europa.eu",
    "gob.es",
    "gov.it",
    "overheid.nl",
    "fgov.be",
    "belgium.be",
    "llv.li",
];

/// Cantonal government domains are `<canton>.ch` (e.g. `steuern.zh.ch` → `zh.ch`).
pub const CANTONS: &[&str] = &[
    "ag", "ai", "ar", "be", "bl", "bs", "fr", "ge", "gl", "gr", "ju", "lu", "ne", "nw", "ow", "sg",
    "sh", "so", "sz", "tg", "ti", "ur", "vd", "vs", "zg", "zh",
];

/// Substrings of a domain or sender name that mark finance, insurance, health or tax mail.
pub const KEYWORDS: &[&str] = &[
    "bank",
    "insurance",
    "versicherung",
    "krankenkasse",
    "clinic",
    "klinik",
    "arzt",
    "steuer",
];

fn matches(domain: &str, entry: &str) -> bool {
    domain == entry || domain.ends_with(&format!(".{entry}"))
}

/// Why the company at `domain` (sender display `name`) must never be contacted, or None.
pub fn keep_reason(domain: &str, name: &str, cfg: &Config) -> Option<&'static str> {
    let domain = registrable_domain(domain);
    let d = domain.as_str();
    let user_kept = cfg
        .autopilot
        .keep_domains
        .iter()
        .map(|k| registrable_domain(k.trim()))
        .any(|k| !k.is_empty() && matches(d, &k));
    if user_kept {
        return Some("keep list");
    }
    if Identity::from_config(cfg).domains.contains(d) {
        return Some("own domain");
    }
    if BUILTIN_DOMAINS.iter().any(|b| matches(d, b)) {
        return Some("bank or insurer");
    }
    if GOVERNMENT_SUFFIXES.iter().any(|g| matches(d, g))
        || CANTONS.iter().any(|c| d == format!("{c}.ch"))
    {
        return Some("government");
    }
    let name = name.to_lowercase();
    if KEYWORDS.iter().any(|k| d.contains(k) || name.contains(k)) {
        return Some("finance, health or tax");
    }
    None
}

/// The always-on rules, for the settings page.
pub fn summary() -> String {
    format!(
        "Always kept, in addition to your list: {} built-in Swiss and EU banks, insurers and health insurers; government domains (.admin.ch, cantonal .ch, .gov, .gv.at, …); domains or sender names containing {}; your own email domain.",
        BUILTIN_DOMAINS.len(),
        KEYWORDS.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_rules() {
        let mut cfg = Config::default();
        cfg.profile.email = "me@employer.example".into();
        cfg.inbox.email = "me@icloud.com".into();
        cfg.autopilot.keep_domains = vec!["Club.Example".into(), " ".into()];
        let k = |d: &str, n: &str| keep_reason(d, n, &cfg);
        assert_eq!(k("ubs.com", "UBS"), Some("bank or insurer"));
        assert_eq!(k("news.helsana.ch", ""), Some("bank or insurer"));
        assert_eq!(k("estv.admin.ch", ""), Some("government"));
        assert_eq!(k("steuern.zh.ch", ""), Some("government"));
        assert_eq!(k("irs.gov", ""), Some("government"));
        assert_eq!(k("bmf.gv.at", ""), Some("government"));
        assert_eq!(k("meinebank.de", ""), Some("finance, health or tax"));
        assert_eq!(
            k("praxis.de", "Dr. med. Arzt Praxis"),
            Some("finance, health or tax")
        );
        assert_eq!(k("Mail.Employer.Example", ""), Some("own domain"));
        assert_eq!(
            k("icloud.com", "Apple"),
            None,
            "freemail is not an own domain"
        );
        assert_eq!(k("news.club.example", ""), Some("keep list"));
        assert_eq!(k("zalando.de", "Zalando"), None);
        assert_eq!(
            k("booking.com", "Booking.com"),
            None,
            "suffix match needs a dot"
        );
        assert!(summary().contains("versicherung"));
    }
}
