//! Broker database: loading, sanitizing, filtering.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

fn nullable<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Broker {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub email: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub website: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub opt_out_url: String,
    /// "us", "eu", "global", ...
    #[serde(deserialize_with = "nullable")]
    pub region: String,
    /// "people-search", "marketing", "background-check", ...
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub category: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub notes: String,
    /// Whether they require ID verification
    #[serde(skip_serializing_if = "is_false")]
    pub requires_id: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "nullable")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrokerDatabase {
    #[serde(deserialize_with = "nullable")]
    pub brokers: Vec<Broker>,
}

/// Empty is valid; otherwise the URL must parse with an http/https scheme.
fn is_valid_url(raw: &str) -> bool {
    if raw.is_empty() {
        return true;
    }
    match url::Url::parse(raw) {
        Ok(u) => matches!(u.scheme(), "http" | "https"),
        Err(_) => false,
    }
}

fn sanitize_broker(b: &mut Broker) {
    if !is_valid_url(&b.opt_out_url) {
        b.opt_out_url.clear();
    }
    if !is_valid_url(&b.website) {
        b.website.clear();
    }
}

fn to_set(items: &[String]) -> HashSet<String> {
    items.iter().map(|s| s.to_lowercase()).collect()
}

pub fn load_from_file(path: impl AsRef<Path>) -> Result<BrokerDatabase> {
    let data = std::fs::read_to_string(path).context("failed to read broker file")?;
    let mut db: BrokerDatabase = if data.trim().is_empty() {
        BrokerDatabase::default()
    } else {
        yaml_serde::from_str(&data).context("failed to parse broker file")?
    };
    db.brokers.iter_mut().for_each(sanitize_broker);
    Ok(db)
}

/// Optional extra facts about a broker, from `data/broker_info.yaml` (keyed by broker id).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrokerInfo {
    #[serde(deserialize_with = "nullable")]
    pub description: String,
    /// ISO 3166-1 alpha-2, upper case.
    #[serde(deserialize_with = "nullable")]
    pub country: String,
    #[serde(deserialize_with = "nullable")]
    pub hq: String,
    #[serde(deserialize_with = "nullable")]
    pub legal_name: String,
    #[serde(deserialize_with = "nullable")]
    pub data_types: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub opt_out_url: String,
    pub requires_id: Option<bool>,
    pub response_days: Option<u32>,
    #[serde(deserialize_with = "nullable")]
    pub source: String,
    #[serde(deserialize_with = "nullable")]
    pub status: String,
}

pub type BrokerInfoMap = HashMap<String, BrokerInfo>;

/// Loads the broker info file; a missing or empty file is an empty map. Keys are lower-cased,
/// invalid URLs dropped and country codes upper-cased.
pub fn load_info(path: impl AsRef<Path>) -> Result<BrokerInfoMap> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BrokerInfoMap::new()),
        Err(e) => return Err(e).context("failed to read broker info file"),
    };
    let map: Option<BrokerInfoMap> =
        yaml_serde::from_str(&data).context("failed to parse broker info file")?;
    Ok(map
        .unwrap_or_default()
        .into_iter()
        .map(|(id, mut info)| {
            for url in [&mut info.opt_out_url, &mut info.source] {
                if !is_valid_url(url) {
                    url.clear();
                }
            }
            info.country = info.country.trim().to_uppercase();
            if info.country.len() != 2 || !info.country.bytes().all(|b| b.is_ascii_uppercase()) {
                info.country.clear();
            }
            (id.to_lowercase(), info)
        })
        .collect())
}

impl BrokerDatabase {
    /// Excludes brokers whose id or name (case-insensitive) is in `excluded`. If `regions` is
    /// non-empty, keeps only brokers in those regions, plus "global" brokers; a "global" entry in
    /// `regions` disables region filtering entirely.
    pub fn filter(&self, regions: &[String], excluded: &[String]) -> Vec<Broker> {
        let region_set = to_set(regions);
        let excluded_set = to_set(excluded);

        self.brokers
            .iter()
            .filter(|b| {
                if excluded_set.contains(&b.id.to_lowercase())
                    || excluded_set.contains(&b.name.to_lowercase())
                {
                    return false;
                }
                if !region_set.is_empty() {
                    let r = b.region.to_lowercase();
                    if !region_set.contains(&r) && !region_set.contains("global") && r != "global" {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    pub fn find_by_id(&self, id: &str) -> Option<&Broker> {
        let id = id.to_lowercase();
        self.brokers.iter().find(|b| b.id.to_lowercase() == id)
    }

    pub fn find_by_email(&self, email: &str) -> Option<&Broker> {
        let email = email.to_lowercase();
        self.brokers
            .iter()
            .find(|b| b.email.to_lowercase() == email)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let data = yaml_serde::to_string(self).context("failed to serialize brokers")?;
        crate::config::write_with_mode(path.as_ref(), data.as_bytes(), 0o644)?;
        Ok(())
    }

    pub fn add(&mut self, broker: Broker) -> Result<()> {
        if self.find_by_id(&broker.id).is_some() {
            bail!("broker with ID {:?} already exists", broker.id);
        }
        self.brokers.push(broker);
        Ok(())
    }

    /// Removes the first broker with this email (case-insensitive) and returns it.
    pub fn remove_by_email(&mut self, email: &str) -> Option<Broker> {
        let email = email.to_lowercase();
        let i = self
            .brokers
            .iter()
            .position(|b| b.email.to_lowercase() == email)?;
        Some(self.brokers.remove(i))
    }

    /// Copies the existing file to `<path>.bak` (if present), then saves.
    pub fn save_with_backup(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if path.exists() {
            let data = std::fs::read(path).context("failed to read file for backup")?;
            let mut backup = path.as_os_str().to_owned();
            backup.push(".bak");
            crate::config::write_with_mode(Path::new(&backup), &data, 0o644)
                .context("failed to create backup")?;
        }
        self.save(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(id: &str, name: &str, region: &str) -> Broker {
        Broker {
            id: id.into(),
            name: name.into(),
            email: format!("privacy@{id}.com"),
            region: region.into(),
            ..Default::default()
        }
    }

    fn sample() -> BrokerDatabase {
        BrokerDatabase {
            brokers: vec![
                b("spokeo", "Spokeo", "us"),
                b("acxiom", "Acxiom", "EU"),
                b("world", "World Data", "global"),
                b("swiss", "Swiss Co", "ch"),
            ],
        }
    }

    fn ids(v: &[Broker]) -> Vec<&str> {
        v.iter().map(|b| b.id.as_str()).collect()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn filter_semantics() {
        let db = sample();
        assert_eq!(
            ids(&db.filter(&[], &[])),
            ["spokeo", "acxiom", "world", "swiss"]
        );
        assert_eq!(ids(&db.filter(&s(&["us"]), &[])), ["spokeo", "world"]);
        assert_eq!(ids(&db.filter(&s(&["eu"]), &[])), ["acxiom", "world"]);
        assert_eq!(
            ids(&db.filter(&s(&["US", "ch"]), &[])),
            ["spokeo", "world", "swiss"]
        );
        // "global" in regions disables region filtering
        assert_eq!(ids(&db.filter(&s(&["global"]), &[])).len(), 4);
        // exclusion by id or by name, case-insensitive
        assert_eq!(
            ids(&db.filter(&[], &s(&["SPOKEO", "world data"]))),
            ["acxiom", "swiss"]
        );
    }

    #[test]
    fn find_add_remove() {
        let mut db = sample();
        assert_eq!(db.find_by_id("ACXIOM").unwrap().name, "Acxiom");
        assert_eq!(db.find_by_email("PRIVACY@spokeo.com").unwrap().id, "spokeo");
        assert!(db.find_by_id("nope").is_none());
        assert_eq!(
            db.add(b("Spokeo", "x", "us")).unwrap_err().to_string(),
            "broker with ID \"Spokeo\" already exists"
        );
        assert_eq!(
            db.remove_by_email("privacy@acxiom.com").unwrap().id,
            "acxiom"
        );
        assert!(db.remove_by_email("privacy@acxiom.com").is_none());
        assert_eq!(db.brokers.len(), 3);
    }

    #[test]
    fn sanitizes_urls() {
        assert!(is_valid_url(""));
        assert!(is_valid_url("https://x.com/a"));
        assert!(is_valid_url("HTTP://x.com"));
        assert!(!is_valid_url("javascript:alert(1)"));
        assert!(!is_valid_url("ftp://x.com"));
        assert!(!is_valid_url("www.example.com"));
    }

    #[test]
    fn save_and_reload_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brokers.yaml");
        let mut db = sample();
        db.brokers[0].tags = vec!["a".into()];
        db.brokers[0].requires_id = true;
        db.save(&path).unwrap();
        db.save_with_backup(&path).unwrap();
        assert!(dir.path().join("brokers.yaml.bak").exists());
        assert_eq!(load_from_file(&path).unwrap(), db);
    }

    #[test]
    fn loads_broker_info() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker_info.yaml");
        assert!(load_info(&path).unwrap().is_empty(), "missing file is fine");
        std::fs::write(
            &path,
            "Acme:\n  description: Test data\n  country: de\n  data_types: [email, phone]\n  \
             opt_out_url: javascript:x\n  source: https://example.test/src\n  response_days: 30\n\
             other:\n  country: Germany\n  requires_id: null\n",
        )
        .unwrap();
        let m = load_info(&path).unwrap();
        let a = &m["acme"];
        assert_eq!(a.country, "DE");
        assert_eq!(a.data_types, ["email", "phone"]);
        assert_eq!(a.opt_out_url, "");
        assert_eq!(a.source, "https://example.test/src");
        assert_eq!(a.response_days, Some(30));
        assert_eq!(a.requires_id, None);
        assert_eq!(m["other"].country, "", "not an ISO-2 code");
        // The shipped file parses.
        load_info(Path::new(env!("CARGO_MANIFEST_DIR")).join("data/broker_info.yaml")).unwrap();
    }

    #[test]
    fn loads_repo_broker_data() {
        let db = load_from_file(Path::new(env!("CARGO_MANIFEST_DIR")).join("data/brokers.yaml"))
            .unwrap();
        assert!(db.brokers.len() >= 1119, "got {}", db.brokers.len());
        assert_eq!(
            db.find_by_id("spokeo").unwrap().opt_out_url,
            "https://www.spokeo.com/optout"
        );
        let us = db.filter(&s(&["us"]), &[]);
        assert!(us.iter().all(|b| b.region == "us" || b.region == "global"));

        // Compare against the Go loader's output (JSON dump of the sanitized brokers).
        let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/brokers_go.json");
        let go: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(golden).unwrap()).unwrap();
        assert_eq!(go.len(), db.brokers.len());
        for (g, r) in go.iter().zip(&db.brokers) {
            let tags: Vec<String> = serde_json::from_value(g["Tags"].clone()).unwrap_or_default();
            let want = Broker {
                id: g["ID"].as_str().unwrap().into(),
                name: g["Name"].as_str().unwrap().into(),
                email: g["Email"].as_str().unwrap().into(),
                website: g["Website"].as_str().unwrap().into(),
                opt_out_url: g["OptOutURL"].as_str().unwrap().into(),
                region: g["Region"].as_str().unwrap().into(),
                category: g["Category"].as_str().unwrap().into(),
                notes: g["Notes"].as_str().unwrap().into(),
                requires_id: g["RequiresID"].as_bool().unwrap(),
                tags,
            };
            assert_eq!(&want, r);
        }
    }
}
