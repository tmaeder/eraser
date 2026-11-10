//! Port of internal/broker/broker.go.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
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
        v.iter().map(|x| x.to_string()).collect()
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
