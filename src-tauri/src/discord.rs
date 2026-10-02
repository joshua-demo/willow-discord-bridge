use crate::config::{ConfigStore, DiscordRpc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
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

const SOUNDBOARD_START_GRACE: Duration = Duration::from_millis(100);

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
            let mut last_sound_attempt: Option<Instant> = None;
            let mut pending_deafen: Option<Instant> = None;
            loop {
                let received = match pending_deafen {
                    Some(deadline) => {
                        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                let command = match received {
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
                                last_sound_attempt = Some(Instant::now());
                                begin_announcement(
                                    rpc,
                                    prior,
                                    &config.soundboard_sound_id,
                                    config.deafen_while_active,
                                )
                                .map(|announcement| {
                                    pending_deafen = announcement.deafen_at;
                                    set_rpc_status(
                                        &status,
                                        RpcState::Connected,
                                        announcement
                                            .sound_error
                                            .map(|error| format!("Soundboard: {error}")),
                                    );
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
                        if let Err(error) = set_active_voice(
                            rpc,
                            prior,
                            config.deafen_while_active && pending_deafen.is_none(),
                        ) {
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

trait VoiceControl {
    fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String>;
    fn play_soundboard_sound(&mut self, sound_id: &str) -> Result<(), String>;

    fn mute_and_play(
        &mut self,
        deaf: Option<bool>,
        sound_id: &str,
    ) -> Result<Option<String>, String> {
        self.set_voice_settings(true, deaf)?;
        Ok(self.play_soundboard_sound(sound_id).err())
    }
}

struct Announcement {
    sound_error: Option<String>,
    deafen_at: Option<Instant>,
}

fn begin_announcement(
    rpc: &mut impl VoiceControl,
    prior: Option<VoiceState>,
    sound_id: &str,
    deafen_while_active: bool,
) -> Result<Announcement, String> {
    // The RPC reply accepts the request but doesn't confirm audio has started.
    // Send mute and sound back-to-back, then briefly leave receive audio on so
    // Discord can send the sound before deafening. Release stays responsive.
    let sound_error = rpc.mute_and_play(prior.map(|state| state.deaf), sound_id)?;
    let deafen_at = if deafen_while_active && sound_error.is_none() {
        Some(Instant::now() + SOUNDBOARD_START_GRACE)
    } else {
        // Failed playback shouldn't delay deafening.
        if deafen_while_active {
            rpc.set_voice_settings(true, Some(true))?;
        }
        None
    };
    Ok(Announcement {
        sound_error,
        deafen_at,
    })
}

fn set_active_voice(
    rpc: &mut impl VoiceControl,
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

struct RpcClient<T = File> {
    pipe: T,
    nonce: u64,
    soundboard_args: HashMap<String, Value>,
}

impl RpcClient<File> {
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
            soundboard_args: HashMap::new(),
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
}

impl<T: Read + Write> RpcClient<T> {
    fn send_request(&mut self, command: &str, args: Value) -> Result<String, String> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        self.write_frame(1, &json!({ "cmd": command, "args": args, "nonce": nonce }))?;
        Ok(nonce)
    }

    fn request(&mut self, command: &str, args: Value) -> Result<Value, String> {
        let nonce = self.send_request(command, args)?;
        self.wait_for_responses(&[nonce])?
            .pop()
            .expect("one response")
    }

    fn wait_for_responses(
        &mut self,
        nonces: &[String],
    ) -> Result<Vec<Result<Value, String>>, String> {
        let mut replies = vec![None; nonces.len()];
        while replies.iter().any(Option::is_none) {
            let (opcode, payload) = self.read_frame()?;
            if opcode == 3 {
                self.write_frame(4, &payload)?;
                continue;
            }
            if opcode == 2 {
                return Err("Discord closed the RPC connection".into());
            }
            let Some(index) = nonces.iter().position(|nonce| {
                payload.get("nonce").and_then(Value::as_str) == Some(nonce.as_str())
            }) else {
                continue;
            };
            if replies[index].is_none() {
                replies[index] = Some(
                    if payload.get("evt").and_then(Value::as_str) == Some("ERROR") {
                        Err(rpc_error_message(&payload))
                    } else {
                        Ok(payload.get("data").cloned().unwrap_or(Value::Null))
                    },
                );
            }
        }
        Ok(replies
            .into_iter()
            .map(|reply| reply.expect("all replies received"))
            .collect())
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

    fn prepare_soundboard_sound(&mut self, sound_id: &str) -> Result<Value, String> {
        if let Some(args) = self.soundboard_args.get(sound_id) {
            return Ok(args.clone());
        }
        let sounds = self.request("GET_SOUNDBOARD_SOUNDS", json!({}))?;
        let args = soundboard_args(&sounds, sound_id)?;
        self.soundboard_args.insert(sound_id.into(), args.clone());
        Ok(args)
    }

    fn play_soundboard_sound(&mut self, sound_id: &str) -> Result<(), String> {
        let args = self.prepare_soundboard_sound(sound_id)?;
        let result = self.request("PLAY_SOUNDBOARD_SOUND", args).map(|_| ());
        if result.is_err() {
            // Refresh metadata on the next attempt, but never retry playback:
            // a lost reply could otherwise cause a duplicate announcement.
            self.soundboard_args.remove(sound_id);
        }
        result
    }

    fn mute_and_play(
        &mut self,
        deaf: Option<bool>,
        sound_id: &str,
    ) -> Result<Option<String>, String> {
        let sound_args = match self.prepare_soundboard_sound(sound_id) {
            Ok(args) => args,
            Err(error) => {
                // Metadata failure must not prevent protecting the microphone.
                self.set_voice_settings(true, deaf)?;
                return Ok(Some(error));
            }
        };
        let mut mute_args = json!({ "mute": true });
        if let Some(deaf) = deaf {
            mute_args["deaf"] = Value::Bool(deaf);
        }
        let mute_nonce = self.send_request("SET_VOICE_SETTINGS", mute_args)?;
        let sound_nonce = self.send_request("PLAY_SOUNDBOARD_SOUND", sound_args)?;
        // Both frames are written before reading either response. Match by nonce
        // because Discord can acknowledge these commands in either order.
        let mut replies = self
            .wait_for_responses(&[mute_nonce, sound_nonce])?
            .into_iter();
        let mute_result = replies.next().expect("mute response");
        let sound_error = replies.next().expect("sound response").err();
        if sound_error.is_some() {
            self.soundboard_args.remove(sound_id);
        }
        mute_result?;
        Ok(sound_error)
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

impl<T: Read + Write> VoiceControl for RpcClient<T> {
    fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
        RpcClient::set_voice_settings(self, mute, deaf)
    }

    fn play_soundboard_sound(&mut self, sound_id: &str) -> Result<(), String> {
        RpcClient::play_soundboard_sound(self, sound_id)
    }

    fn mute_and_play(
        &mut self,
        deaf: Option<bool>,
        sound_id: &str,
    ) -> Result<Option<String>, String> {
        RpcClient::mute_and_play(self, deaf, sound_id)
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
    if !config.soundboard_sound_id.is_empty() {
        // Resolve the source guild while connecting, not while dictation starts.
        // Failure here is nonfatal; playback will report it on the next attempt.
        let _ = rpc.prepare_soundboard_sound(&config.soundboard_sound_id);
    }
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

    #[derive(Default)]
    struct FakeVoice {
        calls: Vec<Value>,
        sound_error: Option<String>,
        mute_error: bool,
        deafen_error: bool,
    }

    impl VoiceControl for FakeVoice {
        fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
            self.calls.push(json!({ "mute": mute, "deaf": deaf }));
            if (deaf == Some(true) && self.deafen_error) || (deaf != Some(true) && self.mute_error)
            {
                return Err("Voice settings failed".into());
            }
            Ok(())
        }

        fn play_soundboard_sound(&mut self, sound_id: &str) -> Result<(), String> {
            self.calls.push(json!({ "sound_id": sound_id }));
            match &self.sound_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    struct TestPipe {
        incoming: std::io::Cursor<Vec<u8>>,
        outgoing: Vec<u8>,
    }

    impl Read for TestPipe {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            // Enforce pipelining at the transport, not just in the voice mock.
            let requests = decoded_frames(&self.outgoing);
            assert!(requests.iter().filter(|(opcode, _)| *opcode == 1).count() >= 2);
            self.incoming.read(bytes)
        }
    }

    impl Write for TestPipe {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.outgoing.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn encoded_frames(frames: &[(u32, Value)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (opcode, payload) in frames {
            let body = serde_json::to_vec(payload).unwrap();
            bytes.extend(opcode.to_le_bytes());
            bytes.extend((body.len() as u32).to_le_bytes());
            bytes.extend(body);
        }
        bytes
    }

    fn decoded_frames(bytes: &[u8]) -> Vec<(u32, Value)> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let opcode = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let length =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            offset += 8;
            frames.push((
                opcode,
                serde_json::from_slice(&bytes[offset..offset + length]).unwrap(),
            ));
            offset += length;
        }
        frames
    }

    fn scripted_rpc(frames: &[(u32, Value)]) -> RpcClient<TestPipe> {
        RpcClient {
            pipe: TestPipe {
                incoming: std::io::Cursor::new(encoded_frames(frames)),
                outgoing: Vec::new(),
            },
            nonce: 0,
            soundboard_args: HashMap::from([(
                "123".into(),
                json!({ "sound_id": "123", "guild_id": "456" }),
            )]),
        }
    }

    #[test]
    fn mute_and_sound_are_sent_before_reading_and_allow_out_of_order_replies() {
        let mut rpc = scripted_rpc(&[
            (3, json!({ "ping": true })),
            (1, json!({ "nonce": "2", "data": null })),
            (1, json!({ "evt": "VOICE_SETTINGS_UPDATE", "data": {} })),
            (1, json!({ "nonce": "1", "data": { "mute": true } })),
        ]);
        assert_eq!(rpc.mute_and_play(Some(false), "123"), Ok(None));
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames[0].1["cmd"], "SET_VOICE_SETTINGS");
        assert_eq!(frames[0].1["args"], json!({ "mute": true, "deaf": false }));
        assert_eq!(frames[1].1["cmd"], "PLAY_SOUNDBOARD_SOUND");
        assert_eq!(
            frames[1].1["args"],
            json!({ "sound_id": "123", "guild_id": "456" })
        );
        assert_eq!(frames[2], (4, json!({ "ping": true })));
    }

    #[test]
    fn both_replies_are_drained_on_error_without_replaying_sound() {
        for failed_nonce in ["1", "2"] {
            let other_nonce = if failed_nonce == "1" { "2" } else { "1" };
            let mut rpc = scripted_rpc(&[
                (
                    1,
                    json!({ "nonce": failed_nonce, "evt": "ERROR", "data": { "message": "Denied" } }),
                ),
                (1, json!({ "nonce": other_nonce, "data": null })),
                (
                    1,
                    json!({ "nonce": "3", "data": { "mute": true, "deaf": false } }),
                ),
            ]);
            let result = rpc.mute_and_play(Some(false), "123");
            if failed_nonce == "1" {
                assert_eq!(result, Err("Denied".into()));
            } else {
                assert_eq!(result, Ok(Some("Denied".into())));
                assert!(!rpc.soundboard_args.contains_key("123"));
            }
            assert_eq!(rpc.nonce, 2);
            assert!(rpc.get_voice_settings().unwrap().mute);
            assert_eq!(rpc.nonce, 3);
        }
    }

    #[test]
    fn announcement_pipelines_mute_and_sound_then_schedules_a_short_deafen_grace() {
        let mut rpc = scripted_rpc(&[
            (1, json!({ "nonce": "2", "data": null })),
            (1, json!({ "nonce": "1", "data": { "mute": true } })),
            (
                1,
                json!({ "nonce": "3", "data": { "mute": true, "deaf": true } }),
            ),
        ]);
        let before = Instant::now();
        let result = begin_announcement(
            &mut rpc,
            Some(VoiceState {
                mute: false,
                deaf: false,
            }),
            "123",
            true,
        )
        .unwrap();
        let after = Instant::now();
        assert!(result.sound_error.is_none());
        let deadline = result.deafen_at.unwrap();
        assert!(deadline >= before + Duration::from_millis(100));
        assert!(deadline <= after + Duration::from_millis(100));
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1["cmd"], "SET_VOICE_SETTINGS");
        assert_eq!(frames[1].1["cmd"], "PLAY_SOUNDBOARD_SOUND");
        set_active_voice(&mut rpc, None, true).unwrap();
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames[2].1["args"], json!({ "mute": true, "deaf": true }));
    }

    #[test]
    fn failed_sound_still_deafens_immediately_and_returns_the_sound_error() {
        let mut rpc = FakeVoice {
            sound_error: Some("Invalid Sound".into()),
            ..FakeVoice::default()
        };
        let result = begin_announcement(&mut rpc, None, "123", true).unwrap();
        assert_eq!(result.sound_error, Some("Invalid Sound".into()));
        assert!(result.deafen_at.is_none());
        assert_eq!(
            rpc.calls.last(),
            Some(&json!({ "mute": true, "deaf": true }))
        );
    }

    #[test]
    fn announcement_does_not_deafen_when_disabled() {
        let mut rpc = FakeVoice::default();
        let result = begin_announcement(&mut rpc, None, "123", false).unwrap();
        assert!(result.sound_error.is_none());
        assert!(result.deafen_at.is_none());
        assert_eq!(
            rpc.calls,
            vec![
                json!({ "mute": true, "deaf": null }),
                json!({ "sound_id": "123" })
            ]
        );
    }

    #[test]
    fn fallback_stops_when_mute_fails() {
        let mut rpc = FakeVoice {
            mute_error: true,
            ..FakeVoice::default()
        };
        assert!(begin_announcement(&mut rpc, None, "123", true).is_err());
        assert_eq!(rpc.calls.len(), 1);
    }

    #[test]
    fn deferred_deafen_failure_is_reported() {
        let mut rpc = FakeVoice {
            deafen_error: true,
            ..FakeVoice::default()
        };
        assert!(
            begin_announcement(&mut rpc, None, "123", true)
                .unwrap()
                .deafen_at
                .is_some()
        );
        assert_eq!(
            set_active_voice(&mut rpc, None, true),
            Err("Voice settings failed".into())
        );
        assert_eq!(rpc.calls.len(), 3);
    }

    #[test]
    fn cached_sound_metadata_skips_rpc_lookup_and_is_invalidated_on_error() {
        // A read-only ordinary file stands in for IPC. Any unexpected request
        // would fail, so the cache hit must succeed without touching the pipe.
        let pipe = File::open(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .unwrap();
        let args = json!({ "sound_id": "123", "guild_id": "456" });
        let mut rpc = RpcClient {
            pipe,
            nonce: 0,
            soundboard_args: HashMap::from([("123".into(), args.clone())]),
        };
        assert_eq!(rpc.prepare_soundboard_sound("123"), Ok(args));
        assert_eq!(rpc.nonce, 0);
        assert!(rpc.play_soundboard_sound("123").is_err());
        assert_eq!(rpc.nonce, 1);
        assert!(!rpc.soundboard_args.contains_key("123"));
    }

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
