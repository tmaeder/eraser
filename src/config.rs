//! Config structs, load/save (0600) and provider defaults.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};

const DEFAULT_RATE_LIMIT_MS: i64 = 2000;

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Treats an explicit YAML `null` (e.g. `regions:` with no value) as the default, like yaml.v3.
fn nullable<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(deserialize_with = "nullable")]
    pub profile: Profile,
    #[serde(deserialize_with = "nullable")]
    pub email: EmailConfig,
    #[serde(deserialize_with = "nullable")]
    pub options: Options,
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub inbox: InboxConfig,
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub pipeline: Pipeline,
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub autopilot: AutopilotConfig,
}

pub const DEFAULT_REMINDER_DAYS: i64 = 30;
pub const DEFAULT_INACTIVE_MONTHS: i64 = 12;

/// Unattended rounds (`eraser autopilot`, hosted cron). The scheduled rounds only run when
/// `enabled`; every step can be switched off on its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutopilotConfig {
    pub enabled: bool,
    pub check_inbox: bool,
    pub confirm_links: bool,
    /// New broker requests plus the one reminder.
    pub broker_requests: bool,
    pub scan_inbox: bool,
    /// RFC 8058 one-click unsubscribes.
    pub unsubscribe: bool,
    pub company_requests: bool,
    /// Days without any reply before the single reminder.
    pub reminder_days: i64,
    /// Months without mail before an inbox company counts as inactive.
    pub inactive_months: i64,
    /// Domains never contacted, on top of the built-in keep rules.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "nullable")]
    pub keep_domains: Vec<String>,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_inbox: true,
            confirm_links: true,
            broker_requests: true,
            scan_inbox: true,
            unsubscribe: true,
            company_requests: true,
            reminder_days: DEFAULT_REMINDER_DAYS,
            inactive_months: DEFAULT_INACTIVE_MONTHS,
            keep_domains: Vec::new(),
        }
    }
}

/// IMAP settings for monitoring broker responses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InboxConfig {
    pub enabled: bool,
    /// "icloud", "gmail", "outlook", "imap"
    #[serde(deserialize_with = "nullable")]
    pub provider: String,
    #[serde(deserialize_with = "nullable")]
    pub server: String,
    pub port: i64,
    #[serde(deserialize_with = "nullable")]
    pub email: String,
    /// App password (not main password)
    #[serde(deserialize_with = "nullable")]
    pub password: String,
    #[serde(deserialize_with = "nullable")]
    pub folder: String,
    pub auto_archive: bool,
    #[serde(deserialize_with = "nullable")]
    pub archive_folder: String,
    /// Folders for the company scanner (empty: INBOX + archive_folder + Archive/Newsletter/Promotions if present).
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "nullable")]
    pub scan_folders: Vec<String>,
}

/// Settings for the automation pipeline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pipeline {
    pub auto_confirm: bool,
    pub auto_fill_forms: bool,
    pub browser_headless: bool,
    pub browser_timeout_sec: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    #[serde(deserialize_with = "nullable")]
    pub first_name: String,
    #[serde(deserialize_with = "nullable")]
    pub last_name: String,
    #[serde(deserialize_with = "nullable")]
    pub email: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub address: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub city: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub state: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub zip_code: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub country: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub phone: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub date_of_birth: String,
}

impl Profile {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    #[serde(deserialize_with = "nullable")]
    pub provider: String,
    #[serde(deserialize_with = "nullable")]
    pub from: String,
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub smtp: SmtpConfig,
}

pub type Email = EmailConfig;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmtpConfig {
    #[serde(deserialize_with = "nullable")]
    pub host: String,
    pub port: i64,
    #[serde(deserialize_with = "nullable")]
    pub username: String,
    #[serde(deserialize_with = "nullable")]
    pub password: String,
    pub use_tls: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    #[serde(deserialize_with = "nullable")]
    pub template: String,
    pub dry_run: bool,
    pub rate_limit_ms: i64,
    #[serde(deserialize_with = "nullable")]
    pub regions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "nullable")]
    pub excluded_brokers: Vec<String>,
}

#[cfg(unix)]
fn check_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let perm = meta.permissions().mode() & 0o777;
    if perm & 0o077 != 0 {
        bail!(
            "config file {} has insecure permissions {:04o}; should be 0600",
            path.display(),
            perm
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn default_config_path() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join(".eraser").join("config.yaml"),
        None => PathBuf::from("config.yaml"),
    }
}

/// `ERASER_CONFIG_KEY` (base64, 32 bytes) turns on at-rest encryption of the config's secrets.
const KEY_ENV: &str = "ERASER_CONFIG_KEY";
const ENC_PREFIX: &str = "enc:v1:";

fn env_key() -> Result<Option<[u8; 32]>> {
    use base64::Engine;
    let Ok(raw) = std::env::var(KEY_ENV) else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .with_context(|| format!("{KEY_ENV} is not valid base64"))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{KEY_ENV} must decode to exactly 32 bytes"))?;
    Ok(Some(key))
}

/// "enc:v1:<base64(nonce || ChaCha20-Poly1305 ciphertext)>"
fn seal(key: &[u8; 32], plain: &str) -> String {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, KeyInit};
    let mut nonce = [0u8; 12];
    rand::fill(&mut nonce[..]);
    let ct = chacha20poly1305::ChaCha20Poly1305::new(key.into())
        .encrypt(&nonce.into(), plain.as_bytes())
        .expect("ChaCha20-Poly1305 encryption cannot fail for in-memory buffers");
    let mut out = nonce.to_vec();
    out.extend(ct);
    format!(
        "{ENC_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(out)
    )
}

fn open(key: Option<&[u8; 32]>, value: &str, field: &str) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, KeyInit};
    let Some(b64) = value.strip_prefix(ENC_PREFIX) else {
        return Ok(value.to_string()); // plaintext (pre-encryption config)
    };
    let Some(key) = key else {
        bail!("{field} is encrypted but {KEY_ENV} is not set");
    };
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()
        .filter(|d| d.len() > 12)
        .with_context(|| format!("{field}: malformed encrypted value"))?;
    let (nonce, ct) = data.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().expect("split at 12");
    let plain = chacha20poly1305::ChaCha20Poly1305::new(key.into())
        .decrypt(&nonce.into(), ct)
        .map_err(|_| anyhow::anyhow!("{field}: decryption failed (wrong {KEY_ENV}?)"))?;
    String::from_utf8(plain).with_context(|| format!("{field}: decrypted value is not UTF-8"))
}

pub fn load(path: impl AsRef<Path>) -> Result<Config> {
    load_with_key(path.as_ref(), env_key()?.as_ref())
}

fn load_with_key(path: &Path, key: Option<&[u8; 32]>) -> Result<Config> {
    if let Err(e) = check_file_permissions(path) {
        eprintln!("WARNING: {e:#}");
    }

    let data = std::fs::read_to_string(path).context("failed to read config file")?;
    let mut cfg: Config = if data.trim().is_empty() {
        Config::default()
    } else {
        yaml_serde::from_str(&data).context("failed to parse config file")?
    };
    cfg.email.smtp.password = open(key, &cfg.email.smtp.password, "email.smtp.password")?;
    cfg.inbox.password = open(key, &cfg.inbox.password, "inbox.password")?;

    if cfg.options.template.is_empty() {
        cfg.options.template = "generic".into();
    }
    if cfg.options.rate_limit_ms == 0 {
        cfg.options.rate_limit_ms = DEFAULT_RATE_LIMIT_MS;
    }

    apply_inbox_defaults(&mut cfg.inbox);

    if cfg.pipeline.browser_timeout_sec == 0 {
        cfg.pipeline.browser_timeout_sec = 30;
    }
    if cfg.autopilot.reminder_days <= 0 {
        cfg.autopilot.reminder_days = DEFAULT_REMINDER_DAYS;
    }
    if cfg.autopilot.inactive_months <= 0 {
        cfg.autopilot.inactive_months = DEFAULT_INACTIVE_MONTHS;
    }
    cfg.pipeline.browser_headless = true; // always headless, whatever the file says

    Ok(cfg)
}

/// Fills folder names and the provider's IMAP server/port when unset.
pub fn apply_inbox_defaults(inbox: &mut InboxConfig) {
    if inbox.folder.is_empty() {
        inbox.folder = "INBOX".into();
    }
    if inbox.archive_folder.is_empty() {
        inbox.archive_folder = "Eraser".into();
    }
    if inbox.server.is_empty() {
        let server = match inbox.provider.as_str() {
            "gmail" => Some("imap.gmail.com"),
            "icloud" => Some("imap.mail.me.com"),
            "outlook" => Some("outlook.office365.com"),
            _ => None,
        };
        if let Some(server) = server {
            inbox.server = server.into();
            inbox.port = 993;
        }
    }
}

pub fn save(path: impl AsRef<Path>, cfg: &Config) -> Result<()> {
    save_with_key(path.as_ref(), cfg, env_key()?.as_ref())
}

fn save_with_key(path: &Path, cfg: &Config, key: Option<&[u8; 32]>) -> Result<()> {
    let sealed;
    let cfg = match key {
        Some(key) => {
            let mut c = cfg.clone();
            for secret in [&mut c.email.smtp.password, &mut c.inbox.password] {
                if !secret.is_empty() {
                    *secret = seal(key, secret);
                }
            }
            sealed = c;
            &sealed
        }
        None => cfg,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir).context("failed to create config directory")?;
    }
    let data = yaml_serde::to_string(cfg).context("failed to serialize config")?;
    write_with_mode(path, data.as_bytes(), 0o600)?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// The mode only applies when the file is created.
pub(crate) fn write_with_mode(path: &Path, data: &[u8], _mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, _mode);
    opts.open(path)?.write_all(data)
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.profile.first_name.is_empty() || self.profile.last_name.is_empty() {
            bail!("profile: first_name and last_name are required");
        }
        if self.profile.email.is_empty() {
            bail!("profile: email is required");
        }
        if self.email.provider.is_empty() {
            bail!("email: provider is required");
        }
        if self.email.from.is_empty() {
            bail!("email: from address is required");
        }
        if self.email.provider != "smtp" {
            bail!(
                "email: unknown provider {:?} (only smtp is supported)",
                self.email.provider
            );
        }
        if self.email.smtp.host.is_empty() {
            bail!("email.smtp: host is required");
        }
        if self.email.smtp.port == 0 {
            bail!("email.smtp: port is required");
        }
        Ok(())
    }

    /// Validates inbox configuration (only called when inbox monitoring is used).
    pub fn validate_inbox(&self) -> Result<()> {
        let inbox = &self.inbox;
        if !inbox.enabled {
            bail!("inbox: monitoring is not enabled in config");
        }
        if inbox.email.is_empty() {
            bail!("inbox: email address is required");
        }
        if inbox.password.is_empty() {
            bail!("inbox: password (app password) is required");
        }
        if inbox.server.is_empty() {
            bail!("inbox: IMAP server is required");
        }
        if inbox.port == 0 {
            bail!("inbox: IMAP port is required");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml")
    }

    #[test]
    fn loads_example_with_defaults() {
        let cfg = load(example_path()).unwrap();
        assert_eq!(cfg.profile.full_name(), "John Doe");
        assert_eq!(cfg.profile.date_of_birth, "1990-01-15");
        assert_eq!(cfg.email.provider, "smtp");
        assert_eq!(cfg.email.smtp.host, "smtp.mail.me.com");
        assert_eq!(cfg.email.smtp.port, 587);
        assert!(cfg.email.smtp.use_tls);
        assert_eq!(cfg.options.template, "generic");
        assert_eq!(cfg.options.rate_limit_ms, 2000);
        assert_eq!(cfg.options.regions, Vec::<String>::new());
        assert_eq!(cfg.inbox.folder, "INBOX");
        assert_eq!(cfg.inbox.archive_folder, "Eraser");
        assert_eq!(cfg.pipeline.browser_timeout_sec, 30);
        assert!(cfg.pipeline.browser_headless);
        cfg.validate().unwrap();
        assert!(cfg.validate_inbox().is_err());
    }

    #[test]
    fn round_trip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.yaml");
        let cfg = load(example_path()).unwrap();
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path).unwrap(), cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    #[test]
    fn inbox_provider_defaults_and_omitempty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(
            &path,
            "profile:\n  first_name: A\n  last_name: B\n  email: a@b.c\noptions:\n  regions:\ninbox:\n  provider: icloud\n",
        )
        .unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.inbox.server, "imap.mail.me.com");
        assert_eq!(cfg.inbox.port, 993);

        let yaml = yaml_serde::to_string(&Config::default()).unwrap();
        assert!(!yaml.contains("inbox"));
        assert!(!yaml.contains("pipeline"));
        assert!(!yaml.contains("smtp"));
        assert!(!yaml.contains("address"));
        assert!(!yaml.contains("excluded_brokers"));
        assert!(yaml.contains("regions: []"));
    }

    #[test]
    fn validate_errors() {
        let mut cfg = Config::default();
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            "profile: first_name and last_name are required"
        );
        cfg.profile = Profile {
            first_name: "A".into(),
            last_name: "B".into(),
            email: "a@b.c".into(),
            ..Default::default()
        };
        cfg.email.provider = "sendgrid".into();
        cfg.email.from = "a@b.c".into();
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            "email: unknown provider \"sendgrid\" (only smtp is supported)"
        );
    }

    #[test]
    fn autopilot_defaults_and_partial_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(
            &path,
            "autopilot:\n  enabled: true\n  unsubscribe: false\n  reminder_days: 0\n  keep_domains:\n    - Example.ORG\n",
        )
        .unwrap();
        let cfg = load(&path).unwrap();
        let a = &cfg.autopilot;
        assert!(a.enabled && !a.unsubscribe);
        assert!(a.check_inbox && a.confirm_links && a.broker_requests);
        assert!(a.scan_inbox && a.company_requests);
        assert_eq!(
            (a.reminder_days, a.inactive_months),
            (30, 12),
            "0 falls back to the default"
        );
        assert_eq!(a.keep_domains, ["Example.ORG"]);

        let d = Config::default();
        assert!(!d.autopilot.enabled, "off by default");
        assert!(d.autopilot.check_inbox, "steps default on");
        let yaml = yaml_serde::to_string(&d).unwrap();
        assert!(!yaml.contains("autopilot"), "default section is omitted");
    }

    fn secret_config() -> Config {
        let mut cfg = load(example_path()).unwrap();
        cfg.email.smtp.password = "smtp-secret".into();
        cfg.inbox.password = "imap-secret".into();
        cfg
    }

    #[test]
    fn encrypted_secrets_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        let key = [7u8; 32];
        let cfg = secret_config();
        save_with_key(&path, &cfg, Some(&key)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("smtp-secret") && !raw.contains("imap-secret"));
        assert_eq!(raw.matches(ENC_PREFIX).count(), 2);
        assert_eq!(load_with_key(&path, Some(&key)).unwrap(), cfg);
    }

    #[test]
    fn plaintext_secrets_still_load_and_get_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        let key = [1u8; 32];
        let cfg = secret_config();
        save_with_key(&path, &cfg, None).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("smtp-secret")
        );
        let loaded = load_with_key(&path, Some(&key)).unwrap();
        assert_eq!(loaded, cfg);
        save_with_key(&path, &loaded, Some(&key)).unwrap();
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("smtp-secret")
        );
    }

    #[test]
    fn encrypted_secrets_need_the_right_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        save_with_key(&path, &secret_config(), Some(&[2u8; 32])).unwrap();
        let wrong = load_with_key(&path, Some(&[3u8; 32]))
            .unwrap_err()
            .to_string();
        assert!(wrong.contains("wrong ERASER_CONFIG_KEY"), "{wrong}");
        let missing = load_with_key(&path, None).unwrap_err().to_string();
        assert!(
            missing.contains("ERASER_CONFIG_KEY is not set"),
            "{missing}"
        );
    }
}
