use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

pub(crate) const DEFAULT_PREVIEW_LIMIT_MB: u32 = 10;
pub(crate) const BYTES_PER_MB: u64 = 1024 * 1024;

pub(crate) const MAX_TRANSFER_PARALLELISM: usize = 16;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct TrashRetention {
    days: Option<u32>,
}

impl TrashRetention {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("trash-retention.json"))
    }

    pub(crate) fn new(days: Option<u32>) -> Result<Self> {
        anyhow::ensure!(
            days.is_none_or(|days| days <= 3650),
            "保留天数须为 0 到 3650 天。"
        );
        Ok(Self {
            days: days.filter(|&days| days > 0),
        })
    }

    pub(crate) fn days(&self) -> Option<u32> {
        self.days
    }

    pub(crate) fn load() -> Result<Self> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => Self::new(serde_json::from_slice::<Self>(&bytes)?.days),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn save(self) -> Result<()> {
        write_settings(&Self::path()?, &self)
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TransferSettings {
    #[serde(default)]
    parallelism: Option<usize>,
    #[serde(default = "default_transfer_parallelism", skip_serializing)]
    upload: usize,
    #[serde(default = "default_transfer_parallelism", skip_serializing)]
    download: usize,
}

fn default_transfer_parallelism() -> usize {
    4
}

impl Default for TransferSettings {
    fn default() -> Self {
        Self {
            parallelism: None,
            upload: default_transfer_parallelism(),
            download: default_transfer_parallelism(),
        }
    }
}

impl TransferSettings {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("transfer.json"))
    }

    fn validate(limit: usize) -> Result<usize> {
        anyhow::ensure!(
            (1..=MAX_TRANSFER_PARALLELISM).contains(&limit),
            "请输入 1 到 {MAX_TRANSFER_PARALLELISM} 之间的整数。"
        );
        Ok(limit)
    }

    pub(crate) fn limit(&self) -> usize {
        self.parallelism.unwrap_or(self.upload.max(self.download))
    }

    pub(crate) fn load() -> Result<Self> {
        Self::load_from(&Self::path()?).context("无法读取传输设置")
    }

    fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let settings: Self = serde_json::from_slice(&bytes)?;
                Self::validate(settings.upload)?;
                Self::validate(settings.download)?;
                Self::validate(settings.limit())?;
                Ok(settings)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let mut settings = Self::default();
                match std::fs::read(path.with_file_name("upload.json")) {
                    Ok(bytes) => settings.upload = Self::validate(serde_json::from_slice(&bytes)?)?,
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                Ok(settings)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn save(limit: usize) -> Result<()> {
        let settings = Self {
            parallelism: Some(Self::validate(limit)?),
            ..Self::default()
        };
        write_settings(&Self::path()?, &settings).context("无法保存传输设置")
    }
}

pub(crate) struct DotEntrySettings;

impl DotEntrySettings {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("hide-dot-entries.json"))
    }

    pub(crate) fn load() -> Result<bool> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn save(hidden: bool) -> Result<()> {
        write_settings(&Self::path()?, &hidden)
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct DisplayTimeZone(Option<chrono_tz::Tz>);

impl DisplayTimeZone {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("time-zone.json"))
    }

    pub(crate) fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(Self::default());
        }
        let zone = input.parse().map_err(|_| {
            anyhow::anyhow!("无法识别时区，请输入 IANA 时区名称，例如 Asia/Shanghai。")
        })?;
        Ok(Self(Some(zone)))
    }

    pub(crate) fn load() -> Result<Self> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => Self::parse(&serde_json::from_slice::<String>(&bytes)?),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn save(self) -> Result<()> {
        write_settings(&Self::path()?, &self.name())
    }

    pub(crate) fn name(self) -> &'static str {
        self.0.map_or("", |zone| zone.name())
    }

    pub(crate) fn format(self, time: chrono::DateTime<chrono::Utc>) -> String {
        let pattern = "%Y-%m-%d %H:%M:%S";
        match self.0 {
            Some(zone) => time.with_timezone(&zone).format(pattern).to_string(),
            None => time
                .with_timezone(&chrono::Local)
                .format(pattern)
                .to_string(),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PreviewSettings {
    limit_mb: u32,
}

impl PreviewSettings {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("preview.json"))
    }

    pub(crate) fn load() -> Result<u32> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => {
                let settings: Self = serde_json::from_slice(&bytes).context("预览设置格式无效")?;
                anyhow::ensure!(settings.limit_mb > 0, "预览上限必须大于 0 MB。");
                Ok(settings.limit_mb)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(DEFAULT_PREVIEW_LIMIT_MB),
            Err(error) => Err(error).context("无法读取预览设置"),
        }
    }

    pub(crate) fn save(limit_mb: u32) -> Result<()> {
        write_settings(&Self::path()?, &Self { limit_mb }).context("无法保存预览设置")
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct BucketQuotas(
    std::collections::HashMap<uuid::Uuid, std::collections::HashMap<String, i64>>,
);

impl BucketQuotas {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("quotas.json"))
    }

    pub(crate) fn load() -> Result<Self> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("桶配额配置格式无效"),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("无法读取桶配额配置"),
        }
    }

    pub(crate) fn get(&self, profile: uuid::Uuid, bucket: &str) -> Option<i64> {
        self.0.get(&profile)?.get(bucket).copied()
    }

    pub(crate) fn save(
        &mut self,
        profile: uuid::Uuid,
        bucket: &str,
        bytes: Option<i64>,
    ) -> Result<()> {
        Self::load()?;
        let mut updated = self.clone();
        let buckets = updated.0.entry(profile).or_default();
        if let Some(bytes) = bytes {
            buckets.insert(bucket.to_owned(), bytes);
        } else {
            buckets.remove(bucket);
        }
        write_settings(&Self::path()?, &updated)?;
        *self = updated;
        Ok(())
    }
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ConnectionSettings {
    bucket: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hidden_buckets: Vec<String>,
    region: String,
    endpoint: String,
    profile: String,
    protected_keys: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct ConnectionProfiles {
    pub(crate) active: Option<uuid::Uuid>,
    pub(crate) profiles: Vec<ConnectionProfile>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ConnectionProfile {
    pub(crate) id: uuid::Uuid,
    pub(crate) name: String,
    pub(crate) settings: ConnectionSettings,
}

impl ConnectionProfiles {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).context("无法读取连接配置"),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).context("连接配置格式无效")?;
        if value.get("profiles").is_some() || value.get("active").is_some() {
            return serde_json::from_value(value).context("凭证列表格式无效");
        }
        let settings = serde_json::from_value(value).context("连接配置格式无效")?;
        let id = uuid::Uuid::new_v4();
        Ok(Self {
            active: Some(id),
            profiles: vec![ConnectionProfile {
                id,
                name: "默认连接".into(),
                settings,
            }],
        })
    }

    pub(crate) fn get(&self, id: uuid::Uuid) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }
}

impl ConnectionSettings {
    pub(crate) fn path() -> Result<PathBuf> {
        let root = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
        };
        Ok(root
            .context("找不到用户配置目录")?
            .join("s3fs")
            .join("connection.json"))
    }

    #[cfg(test)]
    pub(crate) fn load(path: &Path) -> Result<Option<Self>> {
        let profiles = ConnectionProfiles::load(path)?;
        Ok(profiles
            .active
            .and_then(|id| profiles.get(id))
            .map(|profile| profile.settings.clone()))
    }

    pub(crate) fn new(bucket: String, region: String, endpoint: String, profile: String) -> Self {
        Self {
            bucket,
            region,
            endpoint,
            profile,
            ..Self::default()
        }
    }

    pub(crate) fn bucket(&self) -> &str {
        &self.bucket
    }

    pub(crate) fn bucket_names(&self) -> impl Iterator<Item = &str> {
        self.bucket()
            .split([',', '，'])
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    pub(crate) fn set_bucket_names<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        self.bucket = names.collect::<Vec<_>>().join(",");
    }

    pub(crate) fn hidden_buckets(&self) -> &[String] {
        &self.hidden_buckets
    }

    pub(crate) fn set_hidden_buckets(&mut self, names: Vec<String>) {
        self.hidden_buckets = names;
    }
    pub(crate) fn region(&self) -> &str {
        &self.region
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(crate) fn profile(&self) -> &str {
        &self.profile
    }

    pub(crate) fn keys(&self) -> Result<(String, String)> {
        if self.protected_keys.is_empty() {
            return Ok(Default::default());
        }
        let bytes = transform_keys(self.protected_keys.as_bytes(), false)?;
        serde_json::from_slice(&bytes).context("无法解析保存的密钥")
    }

    pub(crate) fn with_keys(mut self, access_key: &str, secret_key: &str) -> Result<Self> {
        if !access_key.is_empty() || !secret_key.is_empty() {
            let keys = serde_json::to_vec(&(access_key, secret_key))?;
            self.protected_keys = String::from_utf8(transform_keys(&keys, true)?)?;
        }
        Ok(self)
    }

    #[cfg(test)]
    fn save(self, path: &Path, access_key: &str, secret_key: &str) -> Result<()> {
        write_settings(path, &self.with_keys(access_key, secret_key)?)
    }
}

#[cfg(test)]
#[test]
fn connection_bucket_filters_round_trip() {
    let mut settings: ConnectionSettings =
        serde_json::from_str(r#"{"bucket":"first, second， , first"}"#).unwrap();
    assert!(settings.hidden_buckets().is_empty());
    settings.set_hidden_buckets(vec!["second".into()]);

    let json = serde_json::to_string(&settings).unwrap();
    let restored: ConnectionSettings = serde_json::from_str(&json).unwrap();
    assert_eq!(
        restored.bucket_names().collect::<Vec<_>>(),
        ["first", "second", "first"]
    );
    assert_eq!(restored.hidden_buckets(), ["second"]);
}

pub(crate) fn write_settings(path: &Path, settings: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("配置文件缺少父目录")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, settings)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(windows)]
fn transform_keys(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };

    let decoded;
    let input_bytes = if encrypt {
        bytes
    } else {
        decoded = STANDARD.decode(bytes).context("保存的密钥编码无效")?;
        &decoded
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_bytes.len().try_into()?,
        pbData: input_bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    // SAFETY: The input blob borrows the bytes for this call. DPAPI only reads
    // those bytes and writes its result to the initialized output blob.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context(if encrypt {
            "Windows 无法加密密钥"
        } else {
            "Windows 无法解密保存的密钥"
        });
    }
    struct ProtectedData(CRYPT_INTEGER_BLOB);
    impl ProtectedData {
        fn as_bytes(&self) -> Result<&[u8]> {
            let len = self.0.cbData as usize;
            if len == 0 {
                return Ok(&[]);
            }
            anyhow::ensure!(
                !self.0.pbData.is_null() && len <= isize::MAX as usize,
                "Windows 返回了无效的密钥缓冲区"
            );
            // SAFETY: DPAPI returned this many initialized bytes in a LocalAlloc
            // buffer, which stays alive while the guard is borrowed.
            Ok(unsafe { std::slice::from_raw_parts(self.0.pbData, len) })
        }
    }
    impl Drop for ProtectedData {
        fn drop(&mut self) {
            // SAFETY: A successful DPAPI call transfers ownership of its
            // LocalAlloc buffer to this guard. LocalFree also accepts NULL.
            unsafe { LocalFree(self.0.pbData.cast()) };
        }
    }

    let output = ProtectedData(output);
    let data = output.as_bytes()?;
    let result = if encrypt {
        STANDARD.encode(data).into_bytes()
    } else {
        data.to_vec()
    };
    Ok(result)
}

#[cfg(not(windows))]
fn transform_keys(_: &[u8], _: bool) -> Result<Vec<u8>> {
    anyhow::bail!("此平台未配置密钥加密存储")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn zero_trash_retention_disables_automatic_cleanup() -> Result<()> {
        for days in [None, Some(0)] {
            assert_eq!(TrashRetention::new(days)?.days(), None);
        }
        for days in [1, 3650] {
            assert_eq!(TrashRetention::new(Some(days))?.days(), Some(days));
        }
        assert!(TrashRetention::new(Some(3651)).is_err());
        Ok(())
    }

    #[test]
    fn transfer_settings_preserve_legacy_upload_limit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("transfer.json");
        assert_eq!(TransferSettings::load_from(&path)?.limit(), 4);
        std::fs::write(path.with_file_name("upload.json"), b"7")?;
        assert_eq!(TransferSettings::load_from(&path)?.limit(), 7);
        std::fs::write(&path, br#"{"upload":3,"download":12}"#)?;
        assert_eq!(TransferSettings::load_from(&path)?.limit(), 12);
        std::fs::write(&path, br#"{"upload":3,"download":17}"#)?;
        assert!(TransferSettings::load_from(&path).is_err());
        Ok(())
    }

    #[test]
    fn legacy_connection_and_multiple_profiles_keep_independent_keys() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("connection.json");
        ConnectionSettings::default().save(&path, "first-access", "first-secret")?;
        let mut profiles = ConnectionProfiles::load(&path)?;
        let first = profiles.active.unwrap();
        assert_eq!(profiles.get(first).unwrap().name, "默认连接");

        let second = uuid::Uuid::new_v4();
        profiles.profiles.push(ConnectionProfile {
            id: second,
            name: "测试环境".into(),
            settings: ConnectionSettings::default().with_keys("second-access", "second-secret")?,
        });
        profiles.active = Some(second);
        write_settings(&path, &profiles)?;

        let contents = std::fs::read_to_string(&path)?;
        for key in [
            "first-access",
            "first-secret",
            "second-access",
            "second-secret",
        ] {
            assert!(!contents.contains(key));
        }
        let profiles = ConnectionProfiles::load(&path)?;
        assert_eq!(profiles.active, Some(second));
        assert_eq!(
            profiles.get(first).unwrap().settings.keys()?.1,
            "first-secret"
        );
        assert_eq!(
            profiles.get(second).unwrap().settings.keys()?.1,
            "second-secret"
        );
        Ok(())
    }

    #[test]
    fn connection_settings_round_trip_and_clear_keys() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("profile").join("connection.json");
        assert!(ConnectionSettings::load(&path)?.is_none());
        let settings = || {
            ConnectionSettings::new(
                String::new(),
                "us-east-1".into(),
                "https://storage.example.test".into(),
                String::new(),
            )
        };
        settings().save(&path, "fixture-access", "fixture-secret")?;
        let contents = std::fs::read_to_string(&path)?;
        assert!(!contents.contains("fixture-access"));
        assert!(!contents.contains("fixture-secret"));
        let saved = ConnectionSettings::load(&path)?.context("未找到连接配置")?;
        assert_eq!(saved.endpoint(), "https://storage.example.test");
        assert_eq!(
            saved.keys()?,
            ("fixture-access".into(), "fixture-secret".into())
        );
        settings().save(&path, "", "")?;
        assert_eq!(
            ConnectionSettings::load(&path)?.unwrap().keys()?,
            Default::default()
        );
        Ok(())
    }
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct GridLayout(bool);

impl GridLayout {
    fn path() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("file-layout.json"))
    }

    pub(crate) fn load() -> Result<bool> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => Ok(serde_json::from_slice::<Self>(&bytes)?.0),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn save(grid: bool) -> Result<()> {
        write_settings(&Self::path()?, &Self(grid))
    }
}
