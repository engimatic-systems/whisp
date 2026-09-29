use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub backend: Backend,
    pub auto_copy: bool,
    pub auto_paste: bool,
    pub paste_shortcut: String,
    pub hotkeys: crate::hotkeys::Bindings,
    pub recording_sounds: bool,
    pub model: String,
    pub prompt: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    #[default]
    Codex,
    ApiKey,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend: Backend::Codex,
            auto_copy: true,
            auto_paste: false,
            paste_shortcut: "auto".into(),
            hotkeys: crate::hotkeys::Bindings::default(),
            recording_sounds: true,
            model: "gpt-transcribe".into(),
            prompt: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME").context("HOME is not set")?;
        let home = PathBuf::from(home);
        let config = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("whisp");
        let state = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("whisp");
        for dir in [&config, &state] {
            fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { config, state })
    }

    pub fn recording(&self) -> PathBuf {
        self.state.join("recording.wav")
    }
    pub fn draft(&self) -> PathBuf {
        self.state.join("draft.txt")
    }
    pub fn key_path(&self) -> PathBuf {
        self.config.join("api-key")
    }

    pub fn settings(&self) -> Result<Settings> {
        match fs::read(self.config.join("settings.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Cannot read settings.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        atomic_write(
            &self.config.join("settings.json"),
            &serde_json::to_vec_pretty(settings)?,
        )
    }

    pub fn api_key(&self) -> Result<String> {
        let key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| fs::read_to_string(self.key_path()).ok())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if key.is_empty() {
            bail!("Add your OpenAI API key in Settings before recording.");
        }
        if key.contains(['\r', '\n']) {
            bail!("The API key must be a single line.");
        }
        Ok(key)
    }
}

// Same-directory rename keeps drafts and credentials intact if a write fails.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Missing parent directory")?)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_save_replaces_content_and_keeps_it_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("draft.txt");
        atomic_write(&path, b"first draft").unwrap();
        atomic_write(&path, "QEMU — བོད་".as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "QEMU — བོད་");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
