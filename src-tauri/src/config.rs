use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    },
    core::PCWSTR,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Mod {
    Ctrl,
    Alt,
    Cmd,
    Shift,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Shortcut {
    pub mods: Vec<Mod>,
    #[serde(default)]
    pub key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DictationApp {
    #[default]
    Willow,
    Wispr,
}

fn default_hands_free_shortcut() -> Shortcut {
    Shortcut {
        mods: vec![Mod::Ctrl, Mod::Cmd],
        key: "VK_20".into(), // Space
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Hold,
    Toggle,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordRpc {
    pub client_id: String,
    pub client_secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_expires_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub dictation_app: DictationApp,
    #[serde(default = "default_hands_free_shortcut")]
    pub hands_free_shortcut: Shortcut,
    pub shortcut: Shortcut,
    pub discord_rpc: DiscordRpc,
    pub mode: Mode,
    #[serde(default)]
    pub deafen_while_active: bool,
    #[serde(default)]
    pub soundboard_sound_id: String,
    pub unmute_delay_ms: u64,
    pub launch_at_login: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dictation_app: DictationApp::Willow,
            hands_free_shortcut: default_hands_free_shortcut(),
            shortcut: Shortcut {
                mods: vec![Mod::Ctrl, Mod::Cmd],
                key: String::new(),
            },
            discord_rpc: DiscordRpc::default(),
            mode: Mode::Auto,
            deafen_while_active: false,
            soundboard_sound_id: String::new(),
            unmute_delay_ms: 0,
            launch_at_login: true,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.shortcut.mods.is_empty() && self.shortcut.key.is_empty() {
            return Err("Shortcut must contain a key or modifier".into());
        }
        if self.dictation_app == DictationApp::Wispr {
            if self.hands_free_shortcut.mods.is_empty() && self.hands_free_shortcut.key.is_empty() {
                return Err("Hands-free shortcut must contain a key or modifier".into());
            }
            if self.shortcut.key == self.hands_free_shortcut.key
                && self.shortcut.mods.len() == self.hands_free_shortcut.mods.len()
                && self
                    .shortcut
                    .mods
                    .iter()
                    .all(|value| self.hands_free_shortcut.mods.contains(value))
            {
                return Err("Dictation and hands-free shortcuts must be different".into());
            }
        }
        if self.unmute_delay_ms > 5_000 {
            return Err("Unmute delay must be between 0 and 5000 milliseconds".into());
        }
        if !self.soundboard_sound_id.is_empty()
            && (self.soundboard_sound_id.len() > 20
                || !self
                    .soundboard_sound_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit()))
        {
            return Err("Soundboard sound ID must contain only digits (up to 20)".into());
        }
        Ok(())
    }

    pub fn public(&self) -> Self {
        let mut value = self.clone();
        value.discord_rpc.access_token = None;
        value.discord_rpc.refresh_token = None;
        value.discord_rpc.token_expires_at = None;
        value
    }
}

pub struct ConfigStore {
    path: PathBuf,
    config: Mutex<Config>,
}

impl ConfigStore {
    pub fn load(path: PathBuf) -> Self {
        let config = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Config>(&raw).ok())
            .map(decrypt_config)
            .filter(|config| config.validate().is_ok())
            .unwrap_or_default();
        Self {
            path,
            config: Mutex::new(config),
        }
    }

    pub fn get(&self) -> Config {
        self.config.lock().expect("config mutex poisoned").clone()
    }

    pub fn replace_public(&self, mut next: Config) -> Result<Config, String> {
        next.validate()?;
        let mut current = self.config.lock().map_err(|_| "Config lock failed")?;
        let same_credentials = current.discord_rpc.client_id == next.discord_rpc.client_id
            && current.discord_rpc.client_secret == next.discord_rpc.client_secret;
        if same_credentials {
            next.discord_rpc.access_token = current.discord_rpc.access_token.clone();
            next.discord_rpc.refresh_token = current.discord_rpc.refresh_token.clone();
            next.discord_rpc.token_expires_at = current.discord_rpc.token_expires_at;
        }
        *current = next.clone();
        self.write_locked(&current)?;
        Ok(next.public())
    }

    pub fn update_tokens(
        &self,
        access: String,
        refresh: String,
        expires_at: u64,
    ) -> Result<(), String> {
        let mut current = self.config.lock().map_err(|_| "Config lock failed")?;
        current.discord_rpc.access_token = Some(access);
        current.discord_rpc.refresh_token = Some(refresh);
        current.discord_rpc.token_expires_at = Some(expires_at);
        self.write_locked(&current)
    }

    fn write_locked(&self, config: &Config) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let encrypted = encrypt_config(config.clone());
        let json = serde_json::to_string_pretty(&encrypted).map_err(|error| error.to_string())?;
        fs::write(&self.path, json).map_err(|error| error.to_string())
    }
}

fn encrypt_config(mut config: Config) -> Config {
    config.discord_rpc.client_secret =
        protect(&config.discord_rpc.client_secret).unwrap_or_default();
    config.discord_rpc.access_token = config.discord_rpc.access_token.as_deref().and_then(protect);
    config.discord_rpc.refresh_token = config
        .discord_rpc
        .refresh_token
        .as_deref()
        .and_then(protect);
    config
}

fn decrypt_config(mut config: Config) -> Config {
    config.discord_rpc.client_secret =
        unprotect(&config.discord_rpc.client_secret).unwrap_or_default();
    config.discord_rpc.access_token = config
        .discord_rpc
        .access_token
        .as_deref()
        .and_then(unprotect);
    config.discord_rpc.refresh_token = config
        .discord_rpc
        .refresh_token
        .as_deref()
        .and_then(unprotect);
    config
}

fn protect(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }
    let mut bytes = value.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .ok()?;
        let protected = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let encoded = protected
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Some(format!("dpapi:{encoded}"))
    }
}

fn unprotect(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }
    if !value.starts_with("dpapi:") {
        return Some(value.to_owned());
    }
    let hex = &value[6..];
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .ok()?;
        let plain = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let result = String::from_utf8(plain.to_vec()).ok();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_willow_shortcut() {
        let config = Config::default();
        assert_eq!(config.shortcut.mods, vec![Mod::Ctrl, Mod::Cmd]);
        assert!(matches!(config.mode, Mode::Auto));
    }

    #[test]
    fn dpapi_round_trip() {
        let encrypted = protect("secret-value").expect("encrypt");
        assert!(encrypted.starts_with("dpapi:"));
        assert_eq!(unprotect(&encrypted).as_deref(), Some("secret-value"));
    }

    #[test]
    fn old_configs_default_deafen_to_off() {
        let raw = r#"{
            "shortcut": { "mods": ["ctrl"], "key": "" },
            "discordRpc": { "clientId": "", "clientSecret": "" },
            "mode": "hold",
            "unmuteDelayMs": 0,
            "launchAtLogin": false
        }"#;
        let config: Config = serde_json::from_str(raw).expect("legacy config");
        assert!(!config.deafen_while_active);
        assert!(config.soundboard_sound_id.is_empty());
        assert_eq!(config.dictation_app, DictationApp::Willow);
        assert_eq!(config.hands_free_shortcut.key, "VK_20");
    }

    #[test]
    fn wispr_shortcuts_must_be_distinct() {
        let mut config = Config::default();
        config.dictation_app = DictationApp::Wispr;
        assert!(config.validate().is_ok());
        config.hands_free_shortcut = config.shortcut.clone();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_soundboard_id() {
        let mut config = Config::default();
        config.soundboard_sound_id = "1328911757753712702".into();
        assert!(config.validate().is_ok());
        config.soundboard_sound_id = "not-a-snowflake".into();
        assert!(config.validate().is_err());
    }
}
