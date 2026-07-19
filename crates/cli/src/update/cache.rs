use std::{
    fs,
    io::Write as _,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::github::Release;

const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct Cache {
    pub checked_at: u64,
    pub release: Option<Release>,
    pub notified_version: Option<String>,
    pub notified_at: Option<u64>,
}

impl Cache {
    pub(super) fn load() -> Self {
        path()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub(super) fn fresh(&self) -> bool {
        now().saturating_sub(self.checked_at) < INTERVAL.as_secs()
    }

    pub(super) fn should_notify(&self, version: &str) -> bool {
        self.notified_version.as_deref() != Some(version)
            || now().saturating_sub(self.notified_at.unwrap_or_default()) >= INTERVAL.as_secs()
    }

    pub(super) fn store(&self) {
        let Some(path) = path() else { return };
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let Ok(mut file) = tempfile::NamedTempFile::new_in(parent) else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = file
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600));
        }
        if serde_json::to_writer(&mut file, self).is_ok()
            && file.flush().is_ok()
            && file.persist(&path).is_ok()
        {}
    }
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("IMAGEGEN_BRIDGE_UPDATE_CACHE") {
        return Some(PathBuf::from(path));
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(root).join("imagegen-bridge/update.json"));
    }
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(root).join("imagegen-bridge/update.json"));
    }
    std::env::var_os("HOME")
        .map(|root| PathBuf::from(root).join(".cache/imagegen-bridge/update.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_is_rate_limited_per_version() {
        let cache = Cache {
            notified_version: Some("1.2.3".into()),
            notified_at: Some(now()),
            ..Cache::default()
        };
        assert!(!cache.should_notify("1.2.3"));
        assert!(cache.should_notify("1.2.4"));
    }
}
