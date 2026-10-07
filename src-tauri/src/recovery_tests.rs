use super::*;
use crate::config::Config;
use serde_json::{Value, json};
use std::{collections::VecDeque, path::PathBuf};

struct Server {
    account: String,
    voice: VoiceState,
    online: bool,
    calls: Vec<Value>,
    attempts: Vec<bool>,
    connect_errors: VecDeque<ConnectFailure>,
    lose_next_write: bool,
    apply_lost_write: bool,
    lose_next_sound: bool,
    reject_heartbeat: bool,
    snapshot_error: bool,
    release_during_snapshot: Option<Arc<Mutex<BridgeStatus>>>,
}

struct FakeConnection {
    server: Arc<Mutex<Server>>,
    account: String,
    broken: bool,
}

impl VoiceControl for FakeConnection {
    fn get_voice_settings(&mut self) -> Result<VoiceState, String> {
        let server = self.server.lock().unwrap();
        if !server.online {
            self.broken = true;
            return Err("pipe disconnected".into());
        }
        if let Some(status) = &server.release_during_snapshot {
            status.lock().unwrap().active = false;
        }
        if server.snapshot_error {
            return Err("voice state unavailable".into());
        }
        Ok(server.voice)
    }
    fn soundboard_allowed(&mut self, guilds: &[String]) -> Result<bool, String> {
        Ok(!guilds.is_empty())
    }
    fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
        let mut server = self.server.lock().unwrap();
        server.calls.push(json!({"mute": mute, "deaf": deaf}));
        let fail = !server.online || std::mem::take(&mut server.lose_next_write);
        if !fail || server.apply_lost_write {
            server.voice.mute = mute;
            if let Some(deaf) = deaf {
                server.voice.deaf = deaf;
            }
        }
        if fail {
            self.broken = true;
            return Err("lost write reply".into());
        }
        Ok(())
    }
    fn play_soundboard_sound(&mut self, _: &str) -> Result<(), String> {
        let mut server = self.server.lock().unwrap();
        server.calls.push(json!({"sound": true}));
        if std::mem::take(&mut server.lose_next_sound) {
            self.broken = true;
            return Err("lost sound reply".into());
        }
        Ok(())
    }
    fn mute_and_play(&mut self, deaf: Option<bool>, sound: &str) -> Result<Option<String>, String> {
        self.set_voice_settings(true, deaf)?;
        match self.play_soundboard_sound(sound) {
            Err(error) if self.broken => Err(error),
            result => Ok(result.err()),
        }
    }
}

impl Connection for FakeConnection {
    fn account_id(&self) -> Option<&str> {
        Some(&self.account)
    }
    fn broken(&self) -> bool {
        self.broken
    }
    fn poll_idle(&mut self) -> Result<(), String> {
        if !self.server.lock().unwrap().online {
            self.broken = true;
            Err("closed idle pipe".into())
        } else {
            Ok(())
        }
    }
    fn heartbeat(&mut self) -> Result<(), String> {
        let mut server = self.server.lock().unwrap();
        server.calls.push(json!({"heartbeat": true}));
        if server.reject_heartbeat {
            Err("not authenticated".into())
        } else {
            Ok(())
        }
    }
}

struct Fixture {
    worker: Worker<FakeConnection>,
    server: Arc<Mutex<Server>>,
    notices: Arc<Mutex<Vec<Notice>>>,
    path: PathBuf,
}

impl Fixture {
    fn new(config: Config) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bridge-recovery-{unique}.json"));
        let store = Arc::new(ConfigStore::load(path.clone()));
        store.replace_public(config).unwrap();
        let status = Arc::new(Mutex::new(BridgeStatus::default()));
        let notices = Arc::new(Mutex::new(Vec::new()));
        let record = notices.clone();
        let observed_status = status.clone();
        let worker = Worker::new(
            store,
            status,
            Box::new(move |_, notice| {
                // Callbacks may read status without being locked out by the worker.
                assert!(observed_status.try_lock().is_ok());
                if let Some(notice) = notice {
                    record.lock().unwrap().push(notice);
                }
            }),
            Instant::now(),
        );
        Self {
            worker,
            server: Arc::new(Mutex::new(Server {
                account: "account-a".into(),
                voice: VoiceState {
                    mute: false,
                    deaf: false,
                },
                online: true,
                calls: Vec::new(),
                attempts: Vec::new(),
                connect_errors: VecDeque::new(),
                lose_next_write: false,
                apply_lost_write: true,
                lose_next_sound: false,
                reject_heartbeat: false,
                snapshot_error: false,
                release_during_snapshot: None,
            })),
            notices,
            path,
        }
    }

    fn connector(&self) -> Box<dyn FnMut(bool) -> Result<FakeConnection, ConnectFailure>> {
        let server = self.server.clone();
        Box::new(move |interactive| {
            let mut state = server.lock().unwrap();
            state.attempts.push(interactive);
            if let Some(error) = state.connect_errors.pop_front() {
                return Err(error);
            }
            state.online = true;
            Ok(FakeConnection {
                server: server.clone(),
                account: state.account.clone(),
                broken: false,
            })
        })
    }

    fn active(&self, active: bool) {
        self.worker.status.lock().unwrap().active = active;
    }
    fn rpc(&self) -> RpcState {
        self.worker.status.lock().unwrap().rpc.clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn retry_backoff_caps_at_thirty_seconds_and_pauses_for_authorization() {
    let now = Instant::now();
    let mut retry = Retry::default();
    for seconds in [1, 2, 4, 8, 16, 30, 30, 30] {
        retry.failed(now, false);
        assert_eq!(retry.next, Some(now + Duration::from_secs(seconds)));
        assert!(!retry.due(now));
    }
    retry.failed(now, true);
    assert!(retry.paused);
    assert!(retry.next.is_none());
    retry.recovered();
    retry.failed(now, false);
    assert_eq!(retry.next, Some(now + Duration::from_secs(1)));
}

#[test]
fn startup_and_retries_connect_without_manual_click_or_oauth_permission() {
    let mut config = Config::default();
    config.discord_rpc.client_id = "test-client".into();
    config.discord_rpc.client_secret = "test-secret".into();
    let mut f = Fixture::new(config);
    f.server
        .lock()
        .unwrap()
        .connect_errors
        .push_back(ConnectFailure::retry("Discord not running"));
    let mut connector = f.connector();
    f.worker.tick(Instant::now(), &mut connector);
    assert_eq!(f.rpc(), RpcState::Reconnecting);
    let deadline = f.worker.retry.next.unwrap();
    f.worker
        .tick(deadline - Duration::from_millis(1), &mut connector);
    assert_eq!(f.server.lock().unwrap().attempts, vec![false]);
    f.worker.tick(deadline, &mut connector);
    assert_eq!(f.rpc(), RpcState::Connected);
    assert_eq!(f.server.lock().unwrap().attempts, vec![false, false]);
    assert!(f.notices.lock().unwrap().is_empty());
}

#[test]
fn idle_close_is_detected_and_reconnected_without_a_shortcut() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.server.lock().unwrap().online = false;
    f.worker.tick(f.worker.next_poll, &mut connector);
    assert_eq!(f.rpc(), RpcState::Reconnecting);
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert_eq!(f.rpc(), RpcState::Connected);
    assert_eq!(f.server.lock().unwrap().attempts, vec![true, false]);
    assert!(
        f.server.lock().unwrap().calls.is_empty(),
        "idle recovery must not change voice or play sounds"
    );
}

#[test]
fn heartbeat_authentication_failure_schedules_background_recovery() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.server.lock().unwrap().reject_heartbeat = true;
    f.worker.tick(f.worker.next_heartbeat, &mut connector);
    assert_eq!(f.rpc(), RpcState::Reconnecting);
    assert!(f.worker.retry.next.is_some());
}

#[test]
fn release_while_offline_restores_all_starting_states_after_reconnect() {
    for mute in [false, true] {
        for deaf in [false, true] {
            for auto_deafen in [false, true] {
                let mut f = Fixture::new(Config {
                    deafen_while_active: auto_deafen,
                    ..Config::default()
                });
                let before = VoiceState { mute, deaf };
                f.server.lock().unwrap().voice = before;
                let mut connector = f.connector();
                f.worker
                    .command(DiscordCommand::Connect, Instant::now(), &mut connector);
                f.active(true);
                f.worker.command(
                    DiscordCommand::SetMute(true),
                    Instant::now(),
                    &mut connector,
                );
                f.server.lock().unwrap().online = false;
                f.worker.tick(f.worker.next_poll, &mut connector);
                assert!(f.worker.session.is_some());
                f.active(false);
                f.worker.command(
                    DiscordCommand::SetMute(false),
                    Instant::now(),
                    &mut connector,
                );
                assert!(
                    f.worker.session.is_some(),
                    "offline release must retain the snapshot"
                );
                f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
                assert_eq!(f.server.lock().unwrap().voice, before);
                assert!(f.worker.session.is_none());
                assert_eq!(f.rpc(), RpcState::Connected);
            }
        }
    }
}

#[test]
fn failed_restore_keeps_ownership_until_a_successful_background_restore() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    {
        let mut server = f.server.lock().unwrap();
        server.lose_next_write = true;
        server.apply_lost_write = false;
    }
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(false),
        Instant::now(),
        &mut connector,
    );
    assert!(f.worker.session.is_some());
    assert!(f.server.lock().unwrap().voice.mute);
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert!(!f.server.lock().unwrap().voice.mute);
    assert!(f.worker.session.is_none());
}

#[test]
fn lost_activation_reply_keeps_the_original_snapshot_for_release() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.server.lock().unwrap().lose_next_write = true;
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    assert!(f.server.lock().unwrap().voice.mute);
    assert!(f.worker.session.is_some());
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(false),
        Instant::now(),
        &mut connector,
    );
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert!(!f.server.lock().unwrap().voice.mute);
}

#[test]
fn reconnect_during_active_dictation_does_not_replay_a_lost_sound() {
    let mut f = Fixture::new(Config {
        soundboard_sound_id: "123".into(),
        ..Config::default()
    });
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.server.lock().unwrap().lose_next_sound = true;
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    assert_eq!(f.rpc(), RpcState::Reconnecting);
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert_eq!(f.rpc(), RpcState::Connected);
    assert!(f.server.lock().unwrap().voice.mute);
    assert_eq!(
        f.server
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|call| call.get("sound").is_some())
            .count(),
        1
    );
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(false),
        Instant::now(),
        &mut connector,
    );
    assert!(!f.server.lock().unwrap().voice.mute);
}

#[test]
fn standalone_sound_is_not_queued_or_replayed_when_offline() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker.retry.start(Instant::now());
    let (reply, result) = mpsc::sync_channel(1);
    f.worker.command(
        DiscordCommand::PlaySound("123".into(), Instant::now(), reply),
        Instant::now(),
        &mut connector,
    );
    assert!(result.try_recv().unwrap().is_err());
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert!(f.server.lock().unwrap().calls.is_empty());
}

#[test]
fn released_gesture_during_slow_connect_cannot_late_mute_or_play() {
    let mut f = Fixture::new(Config {
        soundboard_sound_id: "123".into(),
        ..Config::default()
    });
    let status = f.worker.status.clone();
    let mut real_connector = f.connector();
    let mut slow_connector = move |interactive| {
        status.lock().unwrap().active = false;
        real_connector(interactive)
    };
    f.active(true);
    f.worker.wanted = true;
    f.worker.retry.start(Instant::now());
    f.worker.tick(Instant::now(), &mut slow_connector);
    assert!(f.server.lock().unwrap().calls.is_empty());
    assert!(!f.worker.wanted);
    assert!(f.worker.session.is_none());
}

#[test]
fn stale_queued_start_is_ignored_after_release() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    assert!(f.server.lock().unwrap().calls.is_empty());
}

#[test]
fn another_account_is_never_unmuted_with_the_old_accounts_snapshot() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    f.server.lock().unwrap().online = false;
    f.worker.tick(f.worker.next_poll, &mut connector);
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(false),
        Instant::now(),
        &mut connector,
    );
    f.server.lock().unwrap().account = "account-b".into();
    let writes = f.server.lock().unwrap().calls.len();
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert!(f.server.lock().unwrap().voice.mute);
    assert_eq!(f.server.lock().unwrap().calls.len(), writes);
    assert!(
        f.notices
            .lock()
            .unwrap()
            .contains(&Notice::VoiceStateUncertain)
    );
    // Escalate the critical outage warning when account safety needs attention.
    assert!(
        f.worker
            .status
            .lock()
            .unwrap()
            .rpc_error
            .as_deref()
            .unwrap()
            .contains("account")
    );
}

#[test]
fn long_outage_notifies_once_then_resets_after_recovery() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    let start = Instant::now();
    f.worker.failed("disconnected".into(), false, start);
    f.worker.retry.next = Some(start + WARN_AFTER + Duration::from_secs(100));
    f.worker.tick(start + WARN_AFTER, &mut connector);
    f.worker
        .tick(start + WARN_AFTER + Duration::from_secs(1), &mut connector);
    assert_eq!(*f.notices.lock().unwrap(), vec![Notice::Unavailable]);
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert_eq!(f.rpc(), RpcState::Connected);
    f.active(true);
    f.worker.failed(
        "another outage".into(),
        false,
        start + Duration::from_secs(200),
    );
    assert_eq!(f.notices.lock().unwrap().len(), 2);
}

#[test]
fn authorization_failure_notifies_once_and_stops_background_attempts() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.server
        .lock()
        .unwrap()
        .connect_errors
        .push_back(ConnectFailure::action("Click Connect to authorize"));
    f.worker.retry.start(Instant::now());
    f.worker.tick(Instant::now(), &mut connector);
    f.worker
        .tick(Instant::now() + Duration::from_secs(1000), &mut connector);
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    assert_eq!(f.rpc(), RpcState::Disconnected);
    assert_eq!(f.server.lock().unwrap().attempts, vec![false]);
    assert_eq!(
        *f.notices.lock().unwrap(),
        vec![Notice::AuthorizationRequired]
    );
    f.active(false);
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    assert_eq!(f.rpc(), RpcState::Connected);
    assert_eq!(f.server.lock().unwrap().attempts, vec![false, true]);
}

#[test]
fn sound_queued_during_authentication_is_not_played_after_connect() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    let requested = Instant::now();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    let (reply, result) = mpsc::sync_channel(1);
    f.worker.command(
        DiscordCommand::PlaySound("123".into(), requested, reply),
        Instant::now(),
        &mut connector,
    );
    assert!(result.try_recv().unwrap().is_err());
    assert!(f.server.lock().unwrap().calls.is_empty());
}

#[test]
fn manual_deafen_after_lost_announcement_reply_survives_offline_release() {
    let mut f = Fixture::new(Config {
        deafen_while_active: true,
        soundboard_sound_id: "123".into(),
        ..Config::default()
    });
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.active(true);
    f.server.lock().unwrap().lose_next_sound = true;
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    assert_eq!(f.rpc(), RpcState::Reconnecting);
    // Only mute was sent before the reply was lost; deafen is a manual action.
    f.server.lock().unwrap().voice = VoiceState {
        mute: true,
        deaf: true,
    };
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(false),
        Instant::now(),
        &mut connector,
    );
    f.worker.tick(f.worker.retry.next.unwrap(), &mut connector);
    assert_eq!(
        f.server.lock().unwrap().voice,
        VoiceState {
            mute: true,
            deaf: true
        }
    );
}

#[test]
fn fresh_standalone_sound_still_plays_once_on_a_healthy_connection() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    let requested = Instant::now();
    let (reply, result) = mpsc::sync_channel(1);
    f.worker.command(
        DiscordCommand::PlaySound("123".into(), requested, reply),
        Instant::now(),
        &mut connector,
    );
    assert_eq!(result.try_recv().unwrap(), Ok(true));
    assert_eq!(f.server.lock().unwrap().calls, vec![json!({"sound": true})]);
}

#[test]
fn critical_outage_warning_escalates_once_when_authorization_needs_attention() {
    let mut f = Fixture::new(Config::default());
    f.active(true);
    f.worker
        .failed("disconnected".into(), false, Instant::now());
    f.worker
        .failed("authorization expired".into(), true, Instant::now());
    f.worker
        .failed("authorization expired".into(), true, Instant::now());
    assert_eq!(
        *f.notices.lock().unwrap(),
        vec![Notice::Unavailable, Notice::AuthorizationRequired]
    );
}

#[test]
fn warning_deadline_wakes_even_when_the_next_attempt_is_later() {
    let mut f = Fixture::new(Config::default());
    let now = Instant::now();
    f.worker.failed("disconnected".into(), false, now);
    f.worker.retry.next = Some(now + Duration::from_secs(100));
    assert_eq!(f.worker.deadline(), Some(now + WARN_AFTER));
}

#[test]
fn release_during_snapshot_read_cannot_late_mute() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.server.lock().unwrap().release_during_snapshot = Some(f.worker.status.clone());
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    assert!(f.server.lock().unwrap().calls.is_empty());
    assert!(f.worker.session.is_none());
    assert!(!f.worker.wanted);
}

#[test]
fn unknown_starting_state_protects_the_mic_but_never_guesses_an_unmute() {
    let mut f = Fixture::new(Config {
        soundboard_sound_id: "123".into(),
        ..Config::default()
    });
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.server.lock().unwrap().snapshot_error = true;
    for _ in 0..2 {
        f.active(true);
        f.worker.command(
            DiscordCommand::SetMute(true),
            Instant::now(),
            &mut connector,
        );
        assert!(f.server.lock().unwrap().voice.mute);
        f.active(false);
        f.worker.command(
            DiscordCommand::SetMute(false),
            Instant::now(),
            &mut connector,
        );
        assert!(f.server.lock().unwrap().voice.mute);
    }
    assert_eq!(
        *f.notices.lock().unwrap(),
        vec![Notice::VoiceStateUnavailable]
    );
    assert!(
        f.server
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|call| call["mute"] == true && call.get("sound").is_none())
    );
    assert!(f.worker.status.lock().unwrap().rpc_error.is_some());
}

#[test]
fn manual_connect_preserves_an_existing_dictation_snapshot() {
    let mut f = Fixture::new(Config::default());
    let mut connector = f.connector();
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.active(true);
    f.worker.command(
        DiscordCommand::SetMute(true),
        Instant::now(),
        &mut connector,
    );
    f.worker
        .command(DiscordCommand::Connect, Instant::now(), &mut connector);
    f.active(false);
    f.worker.command(
        DiscordCommand::SetMute(false),
        Instant::now(),
        &mut connector,
    );
    assert!(!f.server.lock().unwrap().voice.mute);
    assert!(f.worker.session.is_none());
}
