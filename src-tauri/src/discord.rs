use crate::config::{ConfigStore, DiscordRpc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RpcState {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    pub active: bool,
    pub engine_ready: bool,
    pub rpc: RpcState,
    pub rpc_error: Option<String>,
}

impl Default for BridgeStatus {
    fn default() -> Self {
        Self {
            active: false,
            engine_ready: false,
            rpc: RpcState::Disconnected,
            rpc_error: None,
        }
    }
}

pub enum DiscordCommand {
    Connect,
    PlaySound(String, mpsc::SyncSender<Result<(), String>>),
    SetMute(bool),
    RefreshActive,
    Shutdown,
}

pub fn start_worker(
    store: Arc<ConfigStore>,
    status: Arc<Mutex<BridgeStatus>>,
) -> mpsc::Sender<DiscordCommand> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("discord-rpc".into())
        .spawn(move || {
            let mut client: Option<RpcClient> = None;
            let mut prior: Option<VoiceState> = None;
            let mut owns_mute = false;
            let mut pending_deafen: Option<Instant> = None;
            let mut last_sound_attempt: Option<Instant> = None;
            loop {
                let command = match pending_deafen {
                    Some(deadline) => {
                        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                let command = match command {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        pending_deafen = None;
                        if owns_mute && store.get().deafen_while_active {
                            if let Some(rpc) = client.as_mut() {
                                if let Err(error) = set_active_voice(rpc, prior, true) {
                                    client = None;
                                    prior = None;
                                    owns_mute = false;
                                    set_rpc_status(&status, RpcState::Disconnected, Some(error));
                                }
                            }
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                match command {
                    DiscordCommand::PlaySound(sound_id, reply) => {
                        let result = (|| {
                            if client.is_none() {
                                client = Some(connect_authenticated(&store)?);
                            }
                            client
                                .as_mut()
                                .expect("connected")
                                .play_soundboard_sound(&sound_id)
                        })();
                        // Playback never changes mute/deafen state or retries a sound: a
                        // lost reply could otherwise cause duplicate playback.
                        // Reconnect on the next press after an error, unless this
                        // connection still owns a dictation mute that must be restored.
                        if result.is_err() && !owns_mute {
                            client = None;
                        }
                        set_rpc_status(
                            &status,
                            if client.is_some() {
                                RpcState::Connected
                            } else {
                                RpcState::Disconnected
                            },
                            result.as_ref().err().cloned(),
                        );
                        let _ = reply.try_send(result);
                    }
                    DiscordCommand::Connect => {
                        set_rpc_status(&status, RpcState::Connecting, None);
                        match connect_authenticated(&store) {
                            Ok(next) => {
                                client = Some(next);
                                prior = None;
                                owns_mute = false;
                                pending_deafen = None;
                                set_rpc_status(&status, RpcState::Connected, None);
                            }
                            Err(error) => {
                                client = None;
                                pending_deafen = None;
                                set_rpc_status(&status, RpcState::Disconnected, Some(error));
                            }
                        }
                    }
                    DiscordCommand::SetMute(on) => {
                        let Some(rpc) = client.as_mut() else { continue };
                        let result = if on {
                            let first_activation = !owns_mute;
                            if first_activation {
                                prior = rpc.get_voice_settings().ok();
                                owns_mute = true;
                            }
                            let config = store.get();
                            let can_play = first_activation
                                && prior.is_some_and(|state| !state.deaf)
                                && !config.soundboard_sound_id.is_empty()
                                && last_sound_attempt
                                    .is_none_or(|last| last.elapsed() >= Duration::from_secs(5));
                            if can_play {
                                // Never leave the real microphone open while announcing.
                                let muted =
                                    rpc.set_voice_settings(true, prior.map(|state| state.deaf));
                                muted.and_then(|_| {
                                    last_sound_attempt = Some(Instant::now());
                                    match rpc.play_soundboard_sound(&config.soundboard_sound_id) {
                                        Ok(()) => {
                                            set_rpc_status(&status, RpcState::Connected, None);
                                            if config.deafen_while_active {
                                                // Discord clips last at most five seconds. Don't deafen
                                                // until the clip finishes; release cancels this timer.
                                                pending_deafen =
                                                    Some(Instant::now() + Duration::from_secs(5));
                                                Ok(())
                                            } else {
                                                Ok(())
                                            }
                                        }
                                        Err(error) => {
                                            // An unsupported soundboard command must not prevent muting.
                                            set_rpc_status(
                                                &status,
                                                RpcState::Connected,
                                                Some(format!("Soundboard: {error}")),
                                            );
                                            set_active_voice(rpc, prior, config.deafen_while_active)
                                        }
                                    }
                                })
                            } else {
                                set_active_voice(
                                    rpc,
                                    prior,
                                    config.deafen_while_active && pending_deafen.is_none(),
                                )
                            }
                        } else {
                            if !owns_mute {
                                continue;
                            }
                            owns_mute = false;
                            pending_deafen = None;
                            match prior.take() {
                                Some(state) => rpc.set_voice_settings(state.mute, Some(state.deaf)),
                                None => Ok(()), // fail closed rather than accidentally unmuting
                            }
                        };
                        if let Err(error) = result {
                            client = None;
                            prior = None;
                            owns_mute = false;
                            pending_deafen = None;
                            set_rpc_status(&status, RpcState::Disconnected, Some(error));
                        }
                    }
                    DiscordCommand::RefreshActive => {
                        if !owns_mute {
                            continue;
                        }
                        let Some(rpc) = client.as_mut() else { continue };
                        let config = store.get();
                        if !config.deafen_while_active || config.soundboard_sound_id.is_empty() {
                            pending_deafen = None;
                        }
                        let deafen = config.deafen_while_active && pending_deafen.is_none();
                        if let Err(error) = set_active_voice(rpc, prior, deafen) {
                            client = None;
                            prior = None;
                            owns_mute = false;
                            pending_deafen = None;
                            set_rpc_status(&status, RpcState::Disconnected, Some(error));
                        }
                    }
                    DiscordCommand::Shutdown => {
                        if owns_mute {
                            if let (Some(rpc), Some(state)) = (client.as_mut(), prior.take()) {
                                let _ = rpc.set_voice_settings(state.mute, Some(state.deaf));
                            }
                        }
                        break;
                    }
                }
            }
        })
        .expect("failed to start Discord worker");
    tx
}

fn set_active_voice(
    rpc: &mut RpcClient,
    prior: Option<VoiceState>,
    deafen_while_active: bool,
) -> Result<(), String> {
    let deaf = if deafen_while_active {
        Some(true)
    } else {
        prior.map(|state| state.deaf)
    };
    rpc.set_voice_settings(true, deaf)
}

fn set_rpc_status(status: &Mutex<BridgeStatus>, rpc: RpcState, error: Option<String>) {
    if let Ok(mut status) = status.lock() {
        status.rpc = rpc;
        status.rpc_error = error;
    }
}

#[derive(Clone, Copy)]
struct VoiceState {
    mute: bool,
    deaf: bool,
}

struct RpcClient {
    pipe: File,
    nonce: u64,
}

impl RpcClient {
    fn connect(client_id: &str) -> Result<Self, String> {
        let mut pipe = None;
        for index in 0..10 {
            let path = format!(r"\\?\pipe\discord-ipc-{index}");
            if let Ok(file) = OpenOptions::new().read(true).write(true).open(path) {
                pipe = Some(file);
                break;
            }
        }
        let mut client = Self {
            pipe: pipe.ok_or("Discord desktop RPC pipe was not found")?,
            nonce: 0,
        };
        client.write_frame(0, &json!({ "v": 1, "client_id": client_id }))?;
        loop {
            let (opcode, payload) = client.read_frame()?;
            if opcode == 1 && payload.get("evt").and_then(Value::as_str) == Some("READY") {
                break;
            }
            if opcode == 2 {
                return Err("Discord closed the RPC handshake".into());
            }
        }
        Ok(client)
    }

    fn request(&mut self, command: &str, args: Value) -> Result<Value, String> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        self.write_frame(1, &json!({ "cmd": command, "args": args, "nonce": nonce }))?;
        loop {
            let (opcode, payload) = self.read_frame()?;
            if opcode == 3 {
                self.write_frame(4, &payload)?;
                continue;
            }
            if opcode == 2 {
                return Err("Discord closed the RPC connection".into());
            }
            if payload.get("nonce").and_then(Value::as_str) != Some(&nonce) {
                continue;
            }
            if payload.get("evt").and_then(Value::as_str) == Some("ERROR") {
                return Err(rpc_error_message(&payload));
            }
            return Ok(payload.get("data").cloned().unwrap_or(Value::Null));
        }
    }

    fn authenticate(&mut self, token: &str) -> Result<(), String> {
        self.request("AUTHENTICATE", json!({ "access_token": token }))
            .map(|_| ())
    }

    fn get_voice_settings(&mut self) -> Result<VoiceState, String> {
        let data = self.request("GET_VOICE_SETTINGS", json!({}))?;
        Ok(VoiceState {
            mute: data.get("mute").and_then(Value::as_bool).unwrap_or(false),
            deaf: data.get("deaf").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    fn play_soundboard_sound(&mut self, sound_id: &str) -> Result<(), String> {
        let sounds = self.request("GET_SOUNDBOARD_SOUNDS", json!({}))?;
        let args = soundboard_args(&sounds, sound_id)?;
        self.request("PLAY_SOUNDBOARD_SOUND", args).map(|_| ())
    }

    fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
        let mut args = json!({ "mute": mute });
        if let Some(deaf) = deaf {
            args["deaf"] = Value::Bool(deaf);
        }
        self.request("SET_VOICE_SETTINGS", args).map(|_| ())
    }

    fn write_frame(&mut self, opcode: u32, payload: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(payload).map_err(|error| error.to_string())?;
        self.pipe
            .write_all(&opcode.to_le_bytes())
            .map_err(|error| error.to_string())?;
        self.pipe
            .write_all(&(body.len() as u32).to_le_bytes())
            .map_err(|error| error.to_string())?;
        self.pipe
            .write_all(&body)
            .map_err(|error| error.to_string())?;
        self.pipe.flush().map_err(|error| error.to_string())
    }

    fn read_frame(&mut self) -> Result<(u32, Value), String> {
        let mut header = [0_u8; 8];
        self.pipe
            .read_exact(&mut header)
            .map_err(|error| error.to_string())?;
        let opcode = u32::from_le_bytes(header[0..4].try_into().expect("opcode"));
        let length = u32::from_le_bytes(header[4..8].try_into().expect("length")) as usize;
        if length > 16 * 1024 * 1024 {
            return Err("Discord RPC frame was too large".into());
        }
        let mut body = vec![0_u8; length];
        self.pipe
            .read_exact(&mut body)
            .map_err(|error| error.to_string())?;
        let payload = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        Ok((opcode, payload))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

fn connect_authenticated(store: &ConfigStore) -> Result<RpcClient, String> {
    let config = store.get();
    let credentials = config.discord_rpc;
    if credentials.client_id.is_empty() || credentials.client_secret.is_empty() {
        return Err("Discord Client ID and Client Secret are required".into());
    }
    let mut rpc = RpcClient::connect(&credentials.client_id)?;
    let now = now_ms();
    let token = if credentials.access_token.is_some()
        && credentials.token_expires_at.unwrap_or(0) > now + 60_000
    {
        credentials.access_token.clone().expect("checked")
    } else if let Some(refresh) = credentials.refresh_token.as_deref() {
        match exchange_token(&credentials, None, Some(refresh)) {
            Ok(tokens) => save_tokens(store, &tokens)?,
            Err(_) => authorize_new(&mut rpc, store, &credentials)?,
        }
    } else {
        authorize_new(&mut rpc, store, &credentials)?
    };
    rpc.authenticate(&token)?;
    Ok(rpc)
}

fn authorize_new(
    rpc: &mut RpcClient,
    store: &ConfigStore,
    credentials: &DiscordRpc,
) -> Result<String, String> {
    let data = rpc.request(
        "AUTHORIZE",
        json!({
            "scopes": ["rpc", "rpc.voice.write"],
            "client_id": credentials.client_id,
        }),
    )?;
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .ok_or("Discord did not return an authorization code")?;
    let tokens = exchange_token(credentials, Some(code), None)?;
    save_tokens(store, &tokens)
}

fn save_tokens(store: &ConfigStore, tokens: &TokenResponse) -> Result<String, String> {
    let expires_at = now_ms() + tokens.expires_in * 1000;
    store.update_tokens(
        tokens.access_token.clone(),
        tokens.refresh_token.clone(),
        expires_at,
    )?;
    Ok(tokens.access_token.clone())
}

fn exchange_token(
    credentials: &DiscordRpc,
    code: Option<&str>,
    refresh: Option<&str>,
) -> Result<TokenResponse, String> {
    let mut form = vec![
        ("client_id", credentials.client_id.as_str()),
        ("client_secret", credentials.client_secret.as_str()),
    ];
    if let Some(code) = code {
        form.extend([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", "http://localhost"),
        ]);
    } else if let Some(refresh) = refresh {
        form.extend([("grant_type", "refresh_token"), ("refresh_token", refresh)]);
    }
    reqwest::blocking::Client::new()
        .post("https://discord.com/api/oauth2/token")
        .form(&form)
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json::<TokenResponse>()
        .map_err(|error| error.to_string())
}

fn soundboard_args(sounds: &Value, sound_id: &str) -> Result<Value, String> {
    let sound = sounds
        .as_array()
        .or_else(|| sounds.get("sounds").and_then(Value::as_array))
        .and_then(|sounds| {
            sounds
                .iter()
                .find(|sound| sound.get("sound_id").and_then(Value::as_str) == Some(sound_id))
        })
        .ok_or_else(|| format!("Sound {sound_id} is not in your available Discord sounds"))?;
    let mut args = json!({ "sound_id": sound_id });
    if let Some(guild_id) = sound.get("guild_id").and_then(Value::as_str) {
        args["guild_id"] = Value::String(guild_id.into());
    }
    Ok(args)
}

fn rpc_error_message(payload: &Value) -> String {
    let message = payload
        .pointer("/data/message")
        .and_then(Value::as_str)
        .unwrap_or("Discord RPC error");
    let code = payload.pointer("/data/code").and_then(Value::as_i64);
    if code == Some(5000) && message.contains("invalid_scope") {
        return format!(
            "{message} Add the Discord account currently signed in to this application's App Testers list in the Developer Portal, then try again."
        );
    }
    message.into()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_scope_error_explains_app_testers_requirement() {
        let payload = json!({
            "data": {
                "code": 5000,
                "message": "OAuth2 Error: invalid_scope: The requested scope is invalid"
            }
        });
        let message = rpc_error_message(&payload);
        assert!(message.contains("App Testers"));
        assert!(message.contains("invalid_scope"));
    }

    #[test]
    fn soundboard_command_includes_the_source_guild() {
        let sounds = json!([{
            "sound_id": "1328911757753712702",
            "guild_id": "1322604750080053409",
            "available": true
        }]);
        assert_eq!(
            soundboard_args(&sounds, "1328911757753712702").unwrap(),
            json!({ "sound_id": "1328911757753712702", "guild_id": "1322604750080053409" })
        );
        assert!(soundboard_args(&sounds, "invalid").is_err());
        assert_eq!(
            soundboard_args(&json!([{ "sound_id": "123", "guild_id": null }]), "123").unwrap(),
            json!({ "sound_id": "123" })
        );
    }

    #[test]
    fn other_rpc_errors_are_unchanged() {
        let payload = json!({ "data": { "code": 4006, "message": "Not authenticated" } });
        assert_eq!(rpc_error_message(&payload), "Not authenticated");
    }
}
