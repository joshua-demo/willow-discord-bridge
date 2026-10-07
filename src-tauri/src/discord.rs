use crate::{
    config::{ConfigStore, DiscordRpc},
    pipe::{DiscordPipe, RPC_TIMEOUT, RpcTransport},
    recovery::{Connection, Notice},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RpcState {
    Disconnected,
    Connecting,
    Reconnecting,
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
    PlaySound(String, Instant, mpsc::SyncSender<Result<bool, String>>),
    SetMute(bool),
    RefreshActive,
    Shutdown,
}

const SOUNDBOARD_START_GRACE: Duration = Duration::from_millis(100);

pub(crate) fn start_worker(
    store: Arc<ConfigStore>,
    status: Arc<Mutex<BridgeStatus>>,
    on_status: impl Fn(BridgeStatus, Option<Notice>) + Send + 'static,
) -> mpsc::Sender<DiscordCommand> {
    crate::recovery::start_worker(store, status, on_status)
}

pub(crate) trait VoiceControl {
    fn get_voice_settings(&mut self) -> Result<VoiceState, String>;
    fn soundboard_allowed(&mut self, guild_ids: &[String]) -> Result<bool, String>;
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

// Used by standalone/MIDI playback as well as the dictation announcement gate.
pub(crate) fn play_allowed_sound(
    rpc: &mut impl VoiceControl,
    sound_id: &str,
    guild_ids: &[String],
) -> Result<bool, String> {
    if !rpc.soundboard_allowed(guild_ids)? {
        return Ok(false);
    }
    rpc.play_soundboard_sound(sound_id)?;
    Ok(true)
}

pub(crate) struct Announcement {
    pub(crate) sound_error: Option<String>,
    pub(crate) sound_attempted: bool,
    pub(crate) deafen_at: Option<Instant>,
}

pub(crate) fn begin_announcement(
    rpc: &mut impl VoiceControl,
    prior: Option<VoiceState>,
    sound_id: &str,
    deafen_while_active: bool,
    guild_ids: &[String],
) -> Result<Announcement, String> {
    let allowed = rpc.soundboard_allowed(guild_ids);
    if !matches!(allowed, Ok(true)) {
        // A different guild, DM, no call, or failed lookup must never block mute/deafen.
        // No sound was requested, so there is no need for the playback grace period.
        set_active_voice(rpc, prior, deafen_while_active)?;
        return Ok(Announcement {
            sound_error: allowed.err(),
            sound_attempted: false,
            deafen_at: None,
        });
    }
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
        sound_attempted: true,
        deafen_at,
    })
}

pub(crate) fn set_active_voice(
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

#[cfg(test)]
fn active_voice_state(prior: Option<VoiceState>, deafen: bool) -> Option<VoiceState> {
    prior.map(|state| VoiceState {
        mute: true,
        deaf: state.deaf || deafen,
    })
}

fn preserved_voice(prior: VoiceState, applied: VoiceState, current: VoiceState) -> VoiceState {
    let manual_deaf = current.deaf != applied.deaf;
    VoiceState {
        // A manual deafen may also mute. Keep both instead of opening the mic.
        mute: prior.mute
            || if current.mute != applied.mute || (manual_deaf && current.deaf) {
                current.mute
            } else {
                false
            },
        // Starting restrictions are sticky: a later snapshot must never clear them.
        deaf: prior.deaf || (manual_deaf && current.deaf),
    }
}

pub(crate) fn preserve_manual_voice(
    rpc: &mut impl VoiceControl,
    prior: &mut Option<VoiceState>,
    applied: Option<VoiceState>,
) -> Result<(), String> {
    if let (Some(before), Some(last)) = (*prior, applied) {
        let current = rpc.get_voice_settings()?;
        let preserved = preserved_voice(before, last, current);
        #[cfg(debug_assertions)]
        eprintln!(
            "{} voice preserve prior={before:?} applied={last:?} current={current:?} restore={preserved:?}",
            now_ms()
        );
        *prior = Some(preserved);
    }
    Ok(())
}

pub(crate) fn restore_voice(
    rpc: &mut impl VoiceControl,
    mut prior: Option<VoiceState>,
    applied: Option<VoiceState>,
) -> Result<(), String> {
    preserve_manual_voice(rpc, &mut prior, applied)?;
    if let Some(state) = prior {
        rpc.set_voice_settings(state.mute, Some(state.deaf))?;
    }
    Ok(()) // Unknown initial state: never guess that unmuting is safe.
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VoiceState {
    pub(crate) mute: bool,
    pub(crate) deaf: bool,
}

pub(crate) struct RpcClient<T = DiscordPipe> {
    pipe: T,
    nonce: u64,
    soundboard_args: HashMap<String, Value>,
    user_id: Option<String>,
    broken: bool,
}

impl RpcClient<DiscordPipe> {
    fn connect(client_id: &str) -> Result<Self, String> {
        let mut client = Self {
            pipe: DiscordPipe::open().map_err(|error| error.to_string())?,
            nonce: 0,
            soundboard_args: HashMap::new(),
            user_id: None,
            broken: false,
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

impl<T: RpcTransport> RpcClient<T> {
    fn send_request(&mut self, command: &str, args: Value) -> Result<String, String> {
        #[cfg(debug_assertions)]
        if command == "SET_VOICE_SETTINGS" {
            eprintln!(
                "{} rpc set mute={} deaf={}",
                now_ms(),
                args["mute"],
                args["deaf"]
            );
        }
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        self.write_frame(1, &json!({ "cmd": command, "args": args, "nonce": nonce }))?;
        Ok(nonce)
    }

    fn request(&mut self, command: &str, args: Value) -> Result<Value, String> {
        // Only an explicit Connect can issue AUTHORIZE and wait for its dialog.
        self.pipe.begin_exchange(if command == "AUTHORIZE" {
            Duration::from_secs(120)
        } else {
            RPC_TIMEOUT
        });
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
            if opcode == 2 {
                self.broken = true;
                return Err("Discord closed the RPC connection".into());
            }
            if opcode != 1 {
                continue;
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
        let data = self.request("AUTHENTICATE", json!({ "access_token": token }))?;
        self.user_id = Some(
            data.pointer("/user/id")
                .and_then(Value::as_str)
                .ok_or("Discord did not identify the signed-in account")?
                .into(),
        );
        Ok(())
    }

    fn soundboard_allowed(&mut self, guild_ids: &[String]) -> Result<bool, String> {
        if guild_ids.is_empty() {
            return Ok(false);
        }
        // Check the current voice destination, not the sound's source guild.
        // Don't cache this: the user can switch calls between shortcut presses.
        let channel = self.request("GET_SELECTED_VOICE_CHANNEL", json!({}))?;
        Ok(channel
            .get("guild_id")
            .and_then(Value::as_str)
            .is_some_and(|guild| guild_ids.iter().any(|id| id == guild)))
    }

    fn get_voice_settings(&mut self) -> Result<VoiceState, String> {
        if let Some(user_id) = self.user_id.clone() {
            let channel = self.request("GET_SELECTED_VOICE_CHANNEL", json!({}))?;
            if !channel.is_null() {
                let state = own_channel_voice_state(&channel, &user_id)?;
                #[cfg(debug_assertions)]
                eprintln!(
                    "{} voice snapshot source=channel mute={} deaf={}",
                    now_ms(),
                    state.mute,
                    state.deaf
                );
                return Ok(state);
            }
        }
        // Outside a call there is no channel voice state to inspect.
        let data = self.request("GET_VOICE_SETTINGS", json!({}))?;
        Ok(VoiceState {
            mute: data
                .get("mute")
                .and_then(Value::as_bool)
                .ok_or("Discord did not return a mute state")?,
            deaf: data
                .get("deaf")
                .and_then(Value::as_bool)
                .ok_or("Discord did not return a deafen state")?,
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
        self.pipe.begin_exchange(RPC_TIMEOUT);
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
        self.write_bytes(opcode, &body)
    }

    fn write_bytes(&mut self, opcode: u32, body: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&opcode.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body);
        let result = self
            .pipe
            .write_all(&frame)
            .map_err(|error| error.to_string());
        self.broken |= result.is_err();
        result
    }

    fn read_frame(&mut self) -> Result<(u32, Value), String> {
        let result = (|| {
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
            if opcode == 3 {
                // Ping bodies are opaque. Echo the exact bytes even when empty
                // or not JSON, both at idle and while awaiting RPC replies.
                self.write_bytes(4, &body)?;
                return Ok((opcode, Value::Null));
            }
            if opcode == 2 || opcode == 4 {
                return Ok((opcode, Value::Null));
            }
            let payload = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
            Ok((opcode, payload))
        })();
        self.broken |= result.is_err();
        result
    }

    fn poll_idle(&mut self) -> Result<(), String> {
        self.pipe.begin_exchange(RPC_TIMEOUT);
        // Bound each drain so an event stream cannot starve shortcut commands.
        for _ in 0..32 {
            let available = self.pipe.available().map_err(|error| {
                self.broken = true;
                error.to_string()
            })?;
            if available == 0 {
                break;
            }
            let (opcode, _) = self.read_frame()?;
            if opcode == 2 {
                self.broken = true;
                return Err("Discord closed the RPC connection".into());
            }
        }
        Ok(())
    }
}

impl<T: RpcTransport> VoiceControl for RpcClient<T> {
    fn get_voice_settings(&mut self) -> Result<VoiceState, String> {
        RpcClient::get_voice_settings(self)
    }

    fn soundboard_allowed(&mut self, guild_ids: &[String]) -> Result<bool, String> {
        RpcClient::soundboard_allowed(self, guild_ids)
    }

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

impl<T: RpcTransport + Send> Connection for RpcClient<T> {
    fn account_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }
    fn broken(&self) -> bool {
        self.broken
    }
    fn poll_idle(&mut self) -> Result<(), String> {
        RpcClient::poll_idle(self)
    }
    fn heartbeat(&mut self) -> Result<(), String> {
        self.request("GET_VOICE_SETTINGS", json!({})).map(|_| ())
    }
}

#[derive(Debug)]
pub(crate) struct ConnectFailure {
    pub(crate) message: String,
    pub(crate) needs_action: bool,
}

impl ConnectFailure {
    pub(crate) fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_action: false,
        }
    }
    pub(crate) fn action(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_action: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenPlan {
    Cached,
    Refresh,
    Authorize,
    NeedsAuthorization,
}

fn token_plan(credentials: &DiscordRpc, interactive: bool, force_refresh: bool) -> TokenPlan {
    if !force_refresh
        && credentials.access_token.is_some()
        && credentials.token_expires_at.unwrap_or(0) > now_ms() + 60_000
    {
        TokenPlan::Cached
    } else if credentials.refresh_token.is_some() {
        TokenPlan::Refresh
    } else if interactive {
        TokenPlan::Authorize
    } else {
        TokenPlan::NeedsAuthorization
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

pub(crate) fn connect_authenticated(
    store: &ConfigStore,
    interactive: bool,
) -> Result<RpcClient, ConnectFailure> {
    let config = store.get();
    let credentials = config.discord_rpc;
    if credentials.client_id.is_empty() || credentials.client_secret.is_empty() {
        return Err(ConnectFailure::action(
            "Discord Client ID and Client Secret are required",
        ));
    }
    // Check authorization before opening a pipe: background startup must never
    // raise an OAuth dialog, even when Discord isn't running yet.
    let plan = token_plan(&credentials, interactive, false);
    if plan == TokenPlan::NeedsAuthorization {
        return Err(ConnectFailure::action(
            "Discord authorization is required. Click Connect to authorize.",
        ));
    }
    let mut rpc = RpcClient::connect(&credentials.client_id).map_err(ConnectFailure::retry)?;
    let token = obtain_token(&mut rpc, store, &credentials, interactive, plan)?;
    if let Err(error) = rpc.authenticate(&token) {
        if rpc.broken {
            return Err(ConnectFailure::retry(error));
        }
        if plan != TokenPlan::Cached {
            return Err(ConnectFailure::action(error));
        }
        // A cached token can be revoked before its recorded expiry. Refresh it
        // once silently; only an explicit Connect may open authorization.
        let token = obtain_token(
            &mut rpc,
            store,
            &credentials,
            interactive,
            token_plan(&credentials, interactive, true),
        )?;
        rpc.authenticate(&token).map_err(|error| {
            if rpc.broken {
                ConnectFailure::retry(error)
            } else {
                ConnectFailure::action(error)
            }
        })?;
    }
    if !config.soundboard_sound_id.is_empty() {
        // Metadata lookup is nonfatal, but a transport failure is not a healthy
        // connection. No sound is ever played while reconnecting.
        let _ = rpc.prepare_soundboard_sound(&config.soundboard_sound_id);
        if rpc.broken {
            return Err(ConnectFailure::retry(
                "Discord pipe disconnected while preparing sound metadata",
            ));
        }
    }
    Ok(rpc)
}

fn obtain_token(
    rpc: &mut RpcClient,
    store: &ConfigStore,
    credentials: &DiscordRpc,
    interactive: bool,
    plan: TokenPlan,
) -> Result<String, ConnectFailure> {
    match plan {
        TokenPlan::Cached => Ok(credentials.access_token.clone().expect("cached token")),
        TokenPlan::Refresh => {
            match exchange_token(credentials, None, credentials.refresh_token.as_deref()) {
                Ok(tokens) => save_tokens(store, &tokens),
                Err(error) if error.needs_action && interactive => {
                    authorize_new(rpc, store, credentials)
                }
                Err(error) => Err(error),
            }
        }
        TokenPlan::Authorize => authorize_new(rpc, store, credentials),
        TokenPlan::NeedsAuthorization => Err(ConnectFailure::action(
            "Discord authorization has expired. Click Connect to authorize again.",
        )),
    }
}

fn authorize_new(
    rpc: &mut RpcClient,
    store: &ConfigStore,
    credentials: &DiscordRpc,
) -> Result<String, ConnectFailure> {
    let data = rpc
        .request(
            "AUTHORIZE",
            json!({
                "scopes": ["rpc", "rpc.voice.write"],
                "client_id": credentials.client_id,
            }),
        )
        .map_err(|error| {
            if rpc.broken {
                ConnectFailure::retry(error)
            } else {
                ConnectFailure::action(error)
            }
        })?;
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| ConnectFailure::action("Discord did not return an authorization code"))?;
    let tokens = exchange_token(credentials, Some(code), None)?;
    save_tokens(store, &tokens)
}

fn save_tokens(store: &ConfigStore, tokens: &TokenResponse) -> Result<String, ConnectFailure> {
    let expires_at = now_ms() + tokens.expires_in * 1000;
    store
        .update_tokens(
            tokens.access_token.clone(),
            tokens.refresh_token.clone(),
            expires_at,
        )
        .map_err(ConnectFailure::action)?;
    Ok(tokens.access_token.clone())
}

fn exchange_token(
    credentials: &DiscordRpc,
    code: Option<&str>,
    refresh: Option<&str>,
) -> Result<TokenResponse, ConnectFailure> {
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
    let response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| ConnectFailure::retry(error.to_string()))?
        .post("https://discord.com/api/oauth2/token")
        .form(&form)
        .send()
        .map_err(|error| ConnectFailure::retry(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        // Never include the token response body or submitted credentials in errors.
        let message = format!(
            "Discord token request failed (HTTP {}).{}",
            status.as_u16(),
            if status.is_client_error() && status.as_u16() != 429 {
                " Check credentials and click Connect to authorize again."
            } else {
                " Background reconnects will continue."
            }
        );
        return Err(if status.is_client_error() && status.as_u16() != 429 {
            ConnectFailure::action(message)
        } else {
            ConnectFailure::retry(message)
        });
    }
    response
        .json::<TokenResponse>()
        .map_err(|_| ConnectFailure::retry("Discord returned an invalid token response"))
}

fn own_channel_voice_state(channel: &Value, user_id: &str) -> Result<VoiceState, String> {
    let state = channel
        .get("voice_states")
        .and_then(Value::as_array)
        .and_then(|states| {
            states
                .iter()
                .find(|state| state.pointer("/user/id").and_then(Value::as_str) == Some(user_id))
        })
        .and_then(|state| state.get("voice_state"))
        .ok_or("Discord did not return your channel voice state")?;
    Ok(VoiceState {
        mute: state
            .get("self_mute")
            .and_then(Value::as_bool)
            .ok_or("Discord did not return your self-mute state")?,
        deaf: state
            .get("self_deaf")
            .and_then(Value::as_bool)
            .ok_or("Discord did not return your self-deafen state")?,
    })
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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{Read, Write},
    };

    #[test]
    fn background_token_plans_never_open_authorization() {
        let mut credentials = DiscordRpc::default();
        assert_eq!(
            token_plan(&credentials, false, false),
            TokenPlan::NeedsAuthorization
        );
        assert_eq!(token_plan(&credentials, true, false), TokenPlan::Authorize);
        credentials.access_token = Some("test-token".into());
        credentials.token_expires_at = Some(now_ms() + 120_000);
        assert_eq!(token_plan(&credentials, false, false), TokenPlan::Cached);
        assert_eq!(
            token_plan(&credentials, false, true),
            TokenPlan::NeedsAuthorization
        );
        credentials.refresh_token = Some("test-refresh".into());
        assert_eq!(token_plan(&credentials, false, true), TokenPlan::Refresh);
        credentials.token_expires_at = Some(0);
        assert_eq!(token_plan(&credentials, false, false), TokenPlan::Refresh);
    }

    #[test]
    fn idle_ping_is_answered_without_voice_changes_or_sound() {
        let mut rpc = scripted_rpc(&[
            (3, json!({"ping": true})),
            (1, json!({"evt": "VOICE_SETTINGS_UPDATE"})),
        ]);
        rpc.pipe.minimum_requests = 0;
        rpc.poll_idle().unwrap();
        assert_eq!(
            decoded_frames(&rpc.pipe.outgoing),
            vec![(4, json!({"ping": true}))]
        );
        assert_eq!(rpc.nonce, 0);
        assert!(!rpc.broken);
    }

    #[test]
    fn opaque_and_empty_ping_bodies_are_echoed_byte_for_byte() {
        for body in [&b"not JSON\x00\xff"[..], &b""[..]] {
            let mut incoming = 3_u32.to_le_bytes().to_vec();
            incoming.extend_from_slice(&(body.len() as u32).to_le_bytes());
            incoming.extend_from_slice(body);
            let mut rpc = scripted_rpc(&[]);
            rpc.pipe.incoming = std::io::Cursor::new(incoming.clone());
            rpc.pipe.minimum_requests = 0;
            rpc.poll_idle().unwrap();
            incoming[..4].copy_from_slice(&4_u32.to_le_bytes());
            assert_eq!(rpc.pipe.outgoing, incoming);
            assert!(!rpc.broken);
        }
    }

    #[test]
    fn idle_close_marks_the_transport_unusable() {
        let mut rpc = scripted_rpc(&[(2, json!({"code": 1000}))]);
        rpc.pipe.minimum_requests = 0;
        assert!(rpc.poll_idle().is_err());
        assert!(rpc.broken);
    }

    #[test]
    fn unanswered_request_marks_the_transport_unusable() {
        let mut rpc = scripted_rpc(&[]);
        rpc.pipe.minimum_requests = 1;
        assert!(rpc.request("GET_VOICE_SETTINGS", json!({})).is_err());
        assert!(rpc.broken);
    }

    #[test]
    fn sound_command_rejection_does_not_mark_a_healthy_pipe_broken() {
        let mut rpc = scripted_rpc(&[(
            1,
            json!({"nonce": "1", "evt": "ERROR", "data": {"message": "Invalid Sound"}}),
        )]);
        rpc.pipe.minimum_requests = 1;
        assert!(rpc.play_soundboard_sound("123").is_err());
        assert!(!rpc.broken);
    }

    #[test]
    fn heartbeat_only_reads_voice_settings() {
        let mut rpc = scripted_rpc(&[(
            1,
            json!({"nonce": "1", "data": {"mute": false, "deaf": false}}),
        )]);
        rpc.pipe.minimum_requests = 1;
        Connection::heartbeat(&mut rpc).unwrap();
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1["cmd"], "GET_VOICE_SETTINGS");
    }

    const SOUNDBOARD_GUILD_ID: &str = "1539407179117760542";

    fn allowed_guilds() -> Vec<String> {
        vec![SOUNDBOARD_GUILD_ID.into()]
    }

    #[derive(Default)]
    struct FakeVoice {
        calls: Vec<Value>,
        sound_error: Option<String>,
        guild_check: Option<Result<bool, String>>,
        mute_error: bool,
        deafen_error: bool,
        current: Option<VoiceState>,
    }

    impl VoiceControl for FakeVoice {
        fn get_voice_settings(&mut self) -> Result<VoiceState, String> {
            self.current.ok_or("Voice state unavailable".into())
        }

        fn soundboard_allowed(&mut self, guild_ids: &[String]) -> Result<bool, String> {
            if guild_ids.is_empty() {
                return Ok(false);
            }
            self.guild_check.clone().unwrap_or(Ok(true))
        }

        fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
            self.calls.push(json!({ "mute": mute, "deaf": deaf }));
            if (deaf == Some(true) && self.deafen_error) || (deaf != Some(true) && self.mute_error)
            {
                return Err("Voice settings failed".into());
            }
            if let Some(current) = self.current.as_mut() {
                current.mute = mute;
                if let Some(deaf) = deaf {
                    current.deaf = deaf;
                }
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
        minimum_requests: usize,
    }

    impl RpcTransport for TestPipe {
        fn begin_exchange(&mut self, _: Duration) {}
        fn available(&self) -> std::io::Result<usize> {
            Ok(self
                .incoming
                .get_ref()
                .len()
                .saturating_sub(self.incoming.position() as usize))
        }
    }

    impl Read for TestPipe {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            // Enforce pipelining at the transport, not just in the voice mock.
            let requests = decoded_frames(&self.outgoing);
            assert!(
                requests.iter().filter(|(opcode, _)| *opcode == 1).count() >= self.minimum_requests
            );
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
                minimum_requests: 2,
            },
            nonce: 0,
            soundboard_args: HashMap::from([(
                "123".into(),
                json!({ "sound_id": "123", "guild_id": "456" }),
            )]),
            user_id: None,
            broken: false,
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
            (
                1,
                json!({ "nonce": "1", "data": { "id": "789", "guild_id": SOUNDBOARD_GUILD_ID } }),
            ),
            (1, json!({ "nonce": "3", "data": null })),
            (1, json!({ "nonce": "2", "data": { "mute": true } })),
            (
                1,
                json!({ "nonce": "4", "data": { "mute": true, "deaf": true } }),
            ),
        ]);
        rpc.pipe.minimum_requests = 1;
        let before = Instant::now();
        let result = begin_announcement(
            &mut rpc,
            Some(VoiceState {
                mute: false,
                deaf: false,
            }),
            "123",
            true,
            &allowed_guilds(),
        )
        .unwrap();
        let after = Instant::now();
        assert!(result.sound_error.is_none());
        assert!(result.sound_attempted);
        let deadline = result.deafen_at.unwrap();
        assert!(deadline >= before + Duration::from_millis(100));
        assert!(deadline <= after + Duration::from_millis(100));
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].1["cmd"], "GET_SELECTED_VOICE_CHANNEL");
        assert_eq!(frames[1].1["cmd"], "SET_VOICE_SETTINGS");
        assert_eq!(frames[2].1["cmd"], "PLAY_SOUNDBOARD_SOUND");
        set_active_voice(&mut rpc, None, true).unwrap();
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames[3].1["args"], json!({ "mute": true, "deaf": true }));
    }

    #[test]
    fn soundboard_scope_checks_current_voice_guild_on_every_attempt() {
        let channels = [
            json!({ "id": "789", "guild_id": SOUNDBOARD_GUILD_ID }),
            json!({ "id": "987", "guild_id": "456" }), // Sound source guild is not the destination.
            Value::Null,                               // No voice connection.
            json!({ "id": "789", "type": 3 }),         // Group DM.
            json!({ "id": "789", "guild_id": null }),
            json!({ "id": "789", "guild_id": 1539407179117760542_u64 }),
        ];
        let frames: Vec<_> = channels
            .iter()
            .enumerate()
            .map(|(i, channel)| (1, json!({ "nonce": (i + 1).to_string(), "data": channel })))
            .collect();
        let mut rpc = scripted_rpc(&frames);
        rpc.pipe.minimum_requests = 1;
        for i in 0..channels.len() {
            assert_eq!(rpc.soundboard_allowed(&allowed_guilds()), Ok(i == 0));
        }
        for (_, request) in decoded_frames(&rpc.pipe.outgoing) {
            assert_eq!(request["cmd"], "GET_SELECTED_VOICE_CHANNEL");
            assert_eq!(request["args"], json!({}));
        }
    }

    #[test]
    fn guild_allowlist_changes_apply_to_the_next_attempt() {
        let mut rpc = scripted_rpc(&[
            (
                1,
                json!({ "nonce": "1", "data": { "guild_id": "1322604750080053409" } }),
            ),
            (
                1,
                json!({ "nonce": "2", "data": { "guild_id": "1322604750080053409" } }),
            ),
            (
                1,
                json!({ "nonce": "3", "data": { "guild_id": SOUNDBOARD_GUILD_ID } }),
            ),
        ]);
        rpc.pipe.minimum_requests = 1;
        assert_eq!(rpc.soundboard_allowed(&allowed_guilds()), Ok(false));
        let mut guilds = allowed_guilds();
        guilds.push("1322604750080053409".into());
        assert_eq!(rpc.soundboard_allowed(&guilds), Ok(true));
        guilds.retain(|id| id != SOUNDBOARD_GUILD_ID);
        assert_eq!(rpc.soundboard_allowed(&guilds), Ok(false));
    }

    #[test]
    fn empty_allowlist_skips_lookup_and_playback_without_blocking_mute() {
        let mut rpc = scripted_rpc(&[]);
        assert_eq!(rpc.soundboard_allowed(&[]), Ok(false));
        assert_eq!(play_allowed_sound(&mut rpc, "123", &[]), Ok(false));
        assert!(rpc.pipe.outgoing.is_empty());
        for deafen in [false, true] {
            let mut voice = FakeVoice::default();
            let result = begin_announcement(
                &mut voice,
                Some(VoiceState {
                    mute: false,
                    deaf: false,
                }),
                "123",
                deafen,
                &[],
            )
            .unwrap();
            assert!(!result.sound_attempted);
            assert!(result.deafen_at.is_none());
            assert_eq!(voice.calls, vec![json!({ "mute": true, "deaf": deafen })]);
        }
    }

    #[test]
    fn outside_soundboard_guild_mute_and_deafen_still_work_without_grace() {
        for deafen in [false, true] {
            let mut rpc = scripted_rpc(&[
                (
                    1,
                    json!({ "nonce": "1", "data": { "id": "789", "guild_id": "other-guild" } }),
                ),
                (1, json!({ "nonce": "2", "data": {} })),
            ]);
            rpc.pipe.minimum_requests = 1;
            let result = begin_announcement(
                &mut rpc,
                Some(VoiceState {
                    mute: false,
                    deaf: false,
                }),
                "123",
                deafen,
                &allowed_guilds(),
            )
            .unwrap();
            assert!(result.sound_error.is_none());
            assert!(!result.sound_attempted);
            assert!(result.deafen_at.is_none());
            let frames = decoded_frames(&rpc.pipe.outgoing);
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[1].1["cmd"], "SET_VOICE_SETTINGS");
            assert_eq!(frames[1].1["args"], json!({ "mute": true, "deaf": deafen }));
        }
    }

    #[test]
    fn failed_guild_lookup_skips_sound_but_does_not_block_mute_or_deafen() {
        let mut rpc = scripted_rpc(&[
            (
                1,
                json!({ "nonce": "1", "evt": "ERROR", "data": { "message": "Lookup failed" } }),
            ),
            (1, json!({ "nonce": "2", "data": {} })),
        ]);
        rpc.pipe.minimum_requests = 1;
        let result = begin_announcement(&mut rpc, None, "123", true, &allowed_guilds()).unwrap();
        assert_eq!(result.sound_error, Some("Lookup failed".into()));
        assert!(!result.sound_attempted);
        assert!(result.deafen_at.is_none());
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].1["args"], json!({ "mute": true, "deaf": true }));
    }

    #[test]
    fn standalone_sound_only_plays_when_guild_is_confirmed() {
        for allowed in [Ok(true), Ok(false), Err("Lookup failed".into())] {
            let mut rpc = FakeVoice {
                guild_check: Some(allowed.clone()),
                ..FakeVoice::default()
            };
            assert_eq!(
                play_allowed_sound(&mut rpc, "123", &allowed_guilds()),
                allowed.clone()
            );
            assert_eq!(rpc.calls.len(), usize::from(allowed == Ok(true)));
            // MIDI sound gating must never change voice settings.
            assert!(rpc.calls.iter().all(|call| call.get("sound_id").is_some()));
        }
    }

    #[test]
    fn failed_sound_still_deafens_immediately_and_returns_the_sound_error() {
        let mut rpc = FakeVoice {
            sound_error: Some("Invalid Sound".into()),
            ..FakeVoice::default()
        };
        let result = begin_announcement(&mut rpc, None, "123", true, &allowed_guilds()).unwrap();
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
        let result = begin_announcement(&mut rpc, None, "123", false, &allowed_guilds()).unwrap();
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
        assert!(begin_announcement(&mut rpc, None, "123", true, &allowed_guilds()).is_err());
        assert_eq!(rpc.calls.len(), 1);
    }

    #[test]
    fn deferred_deafen_failure_is_reported() {
        let mut rpc = FakeVoice {
            deafen_error: true,
            ..FakeVoice::default()
        };
        assert!(
            begin_announcement(&mut rpc, None, "123", true, &allowed_guilds())
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
    fn restore_keeps_each_starting_voice_state_with_auto_deafen_on_or_off() {
        for mute in [false, true] {
            for deaf in [false, true] {
                for auto_deafen in [false, true] {
                    let prior = VoiceState { mute, deaf };
                    let applied = active_voice_state(Some(prior), auto_deafen);
                    let mut rpc = FakeVoice {
                        current: applied,
                        ..FakeVoice::default()
                    };
                    restore_voice(&mut rpc, Some(prior), applied).unwrap();
                    assert_eq!(rpc.current, Some(prior));
                    assert_eq!(rpc.calls, vec![json!({ "mute": mute, "deaf": deaf })]);
                }
            }
        }
    }

    #[test]
    fn manual_deafen_during_mute_only_is_kept_on_stop() {
        let prior = VoiceState {
            mute: false,
            deaf: false,
        };
        let applied = active_voice_state(Some(prior), false);
        let mut rpc = FakeVoice {
            current: Some(VoiceState {
                mute: true,
                deaf: true,
            }),
            ..FakeVoice::default()
        };
        restore_voice(&mut rpc, Some(prior), applied).unwrap();
        assert_eq!(rpc.calls, vec![json!({ "mute": true, "deaf": true })]);
    }

    #[test]
    fn settings_refresh_does_not_undo_manual_deafen() {
        let mut prior = Some(VoiceState {
            mute: false,
            deaf: false,
        });
        let mut applied = active_voice_state(prior, false);
        let mut rpc = FakeVoice {
            current: Some(VoiceState {
                mute: true,
                deaf: true,
            }),
            ..FakeVoice::default()
        };
        preserve_manual_voice(&mut rpc, &mut prior, applied).unwrap();
        set_active_voice(&mut rpc, prior, false).unwrap();
        applied = active_voice_state(prior, false);
        restore_voice(&mut rpc, prior, applied).unwrap();
        assert_eq!(
            rpc.current,
            Some(VoiceState {
                mute: true,
                deaf: true
            })
        );
        assert!(
            rpc.calls
                .iter()
                .all(|call| call["mute"] == true && call["deaf"] == true)
        );
    }

    #[test]
    fn manual_deafen_during_sound_grace_is_not_owned_by_the_bridge() {
        let mut prior = Some(VoiceState {
            mute: false,
            deaf: false,
        });
        let applied = active_voice_state(prior, false);
        let mut rpc = FakeVoice {
            current: Some(VoiceState {
                mute: true,
                deaf: true,
            }),
            ..FakeVoice::default()
        };
        preserve_manual_voice(&mut rpc, &mut prior, applied).unwrap();
        set_active_voice(&mut rpc, prior, true).unwrap();
        restore_voice(&mut rpc, prior, active_voice_state(prior, true)).unwrap();
        assert_eq!(
            rpc.current,
            Some(VoiceState {
                mute: true,
                deaf: true
            })
        );
    }

    #[test]
    fn disabling_auto_deafen_releases_only_the_bridges_deafen() {
        for already_deaf in [false, true] {
            let mut prior = Some(VoiceState {
                mute: false,
                deaf: already_deaf,
            });
            let applied = active_voice_state(prior, true);
            let mut rpc = FakeVoice {
                current: applied,
                ..FakeVoice::default()
            };
            preserve_manual_voice(&mut rpc, &mut prior, applied).unwrap();
            set_active_voice(&mut rpc, prior, false).unwrap();
            assert_eq!(
                rpc.current,
                Some(VoiceState {
                    mute: true,
                    deaf: already_deaf
                })
            );
            restore_voice(&mut rpc, prior, active_voice_state(prior, false)).unwrap();
            assert_eq!(
                rpc.current,
                Some(VoiceState {
                    mute: false,
                    deaf: already_deaf
                })
            );
        }
    }

    #[test]
    fn starting_mute_and_deafen_cannot_be_cleared_by_later_snapshots() {
        for mute in [false, true] {
            for deaf in [false, true] {
                let prior = VoiceState { mute, deaf };
                let mut rpc = FakeVoice {
                    current: Some(VoiceState {
                        mute: false,
                        deaf: false,
                    }),
                    ..FakeVoice::default()
                };
                restore_voice(&mut rpc, Some(prior), active_voice_state(Some(prior), true))
                    .unwrap();
                assert_eq!(rpc.current, Some(prior));
            }
        }
    }

    fn channel_state(mute: bool, deaf: bool) -> Value {
        json!({ "id": "channel", "voice_states": [
            { "user": { "id": "someone-else" }, "voice_state": { "self_mute": false, "self_deaf": false } },
            { "user": { "id": "self" }, "voice_state": { "self_mute": mute, "self_deaf": deaf, "mute": false, "deaf": false } }
        ] })
    }

    #[test]
    fn authenticated_snapshot_uses_own_channel_self_state() {
        let mut rpc = scripted_rpc(&[(
            1,
            json!({ "nonce": "1", "data": channel_state(true, true) }),
        )]);
        rpc.user_id = Some("self".into());
        rpc.pipe.minimum_requests = 1;
        assert_eq!(
            rpc.get_voice_settings(),
            Ok(VoiceState {
                mute: true,
                deaf: true
            })
        );
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1["cmd"], "GET_SELECTED_VOICE_CHANNEL");
    }

    #[test]
    fn pre_deafened_channel_state_is_still_deafened_after_prompt_release() {
        let mut rpc = scripted_rpc(&[
            (
                1,
                json!({ "nonce": "1", "data": channel_state(false, true) }),
            ),
            (1, json!({ "nonce": "2", "data": {} })),
            // Even a later false snapshot cannot erase the starting deafen.
            (
                1,
                json!({ "nonce": "3", "data": channel_state(false, false) }),
            ),
            (1, json!({ "nonce": "4", "data": {} })),
        ]);
        rpc.user_id = Some("self".into());
        rpc.pipe.minimum_requests = 1;
        let prior = Some(rpc.get_voice_settings().unwrap());
        assert!(prior.unwrap().deaf);
        set_active_voice(&mut rpc, prior, false).unwrap();
        restore_voice(&mut rpc, prior, active_voice_state(prior, false)).unwrap();
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames[1].1["args"], json!({ "mute": true, "deaf": true }));
        assert_eq!(frames[3].1["args"], json!({ "mute": false, "deaf": true }));
    }

    #[test]
    fn channel_snapshot_does_not_confuse_server_mute_with_self_mute() {
        let mut channel = channel_state(false, false);
        channel["voice_states"][1]["voice_state"]["mute"] = json!(true);
        channel["voice_states"][1]["voice_state"]["deaf"] = json!(true);
        assert_eq!(
            own_channel_voice_state(&channel, "self"),
            Ok(VoiceState {
                mute: false,
                deaf: false
            })
        );
        assert!(own_channel_voice_state(&channel, "missing-user").is_err());
        channel["voice_states"][1]["voice_state"]["self_deaf"] = Value::Null;
        assert!(own_channel_voice_state(&channel, "self").is_err());
    }

    #[test]
    fn out_of_call_snapshot_falls_back_to_voice_settings() {
        let mut rpc = scripted_rpc(&[
            (1, json!({ "nonce": "1", "data": null })),
            (
                1,
                json!({ "nonce": "2", "data": { "mute": true, "deaf": true } }),
            ),
        ]);
        rpc.user_id = Some("self".into());
        rpc.pipe.minimum_requests = 1;
        assert_eq!(
            rpc.get_voice_settings(),
            Ok(VoiceState {
                mute: true,
                deaf: true
            })
        );
        let frames = decoded_frames(&rpc.pipe.outgoing);
        assert_eq!(frames[0].1["cmd"], "GET_SELECTED_VOICE_CHANNEL");
        assert_eq!(frames[1].1["cmd"], "GET_VOICE_SETTINGS");
    }

    #[test]
    fn authentication_records_the_account_for_channel_snapshot_matching() {
        let mut rpc = scripted_rpc(&[(
            1,
            json!({ "nonce": "1", "data": { "user": { "id": "self" } } }),
        )]);
        rpc.pipe.minimum_requests = 1;
        rpc.authenticate("test-token").unwrap();
        assert_eq!(rpc.user_id.as_deref(), Some("self"));
    }

    #[test]
    fn unknown_or_unreadable_voice_state_does_not_guess_an_unmute() {
        let mut rpc = FakeVoice::default();
        restore_voice(&mut rpc, None, None).unwrap();
        assert!(rpc.calls.is_empty());
        let prior = Some(VoiceState {
            mute: false,
            deaf: false,
        });
        assert!(restore_voice(&mut rpc, prior, active_voice_state(prior, false)).is_err());
        assert!(rpc.calls.is_empty());
    }

    #[test]
    fn rpc_voice_snapshot_requires_both_boolean_states() {
        for data in [
            json!({}),
            json!({ "mute": false }),
            json!({ "mute": "false", "deaf": false }),
            json!({ "mute": false, "deaf": null }),
        ] {
            let mut rpc = scripted_rpc(&[(1, json!({ "nonce": "1", "data": data }))]);
            rpc.pipe.minimum_requests = 1;
            assert!(rpc.get_voice_settings().is_err());
            assert_eq!(
                decoded_frames(&rpc.pipe.outgoing)[0].1["cmd"],
                "GET_VOICE_SETTINGS"
            );
        }
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
            user_id: None,
            broken: false,
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
