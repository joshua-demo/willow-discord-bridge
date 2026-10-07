use crate::config::{Config, ConfigStore, DictationApp, Mod, Shortcut};
use serde::Serialize;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread,
    time::Duration,
};
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
            VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT,
            LLKHF_INJECTED, MSG, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN,
            WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Press,
    Release,
    HandsFreePress,
    Dismiss,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureResult {
    pub combo: Option<Shortcut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Default)]
struct Keys {
    physical_down: HashSet<u32>,
    mods: HashSet<ModKey>,
    active: bool,
    hands_free_active: bool,
    pressed_keys: HashSet<String>,
    capture_peak: HashSet<ModKey>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum ModKey {
    Ctrl,
    Alt,
    Cmd,
    Shift,
}

struct Shared {
    config: Arc<ConfigStore>,
    event_tx: mpsc::Sender<InputEvent>,
    keys: Mutex<Keys>,
    capture: Mutex<Option<mpsc::Sender<Shortcut>>>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

pub struct InputMonitor {
    shared: Arc<Shared>,
}

impl InputMonitor {
    pub fn start(
        config: Arc<ConfigStore>,
        event_tx: mpsc::Sender<InputEvent>,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            config,
            event_tx,
            keys: Mutex::new(Keys::default()),
            capture: Mutex::new(None),
        });
        SHARED
            .set(shared.clone())
            .map_err(|_| "Keyboard monitor already started")?;
        thread::Builder::new()
            .name("dictation-keyboard-hook".into())
            .spawn(|| unsafe {
                let module = GetModuleHandleW(None).unwrap_or_default();
                let hook = SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(keyboard_proc),
                    Some(HINSTANCE(module.0)),
                    0,
                )
                .expect("failed to install keyboard hook");
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self { shared })
    }

    pub fn reset_gesture(&self) {
        if let Ok(mut keys) = self.shared.keys.lock() {
            keys.active = false;
            keys.hands_free_active = false;
            keys.physical_down.clear();
            keys.mods.clear();
            keys.pressed_keys.clear();
            send_event(&self.shared.event_tx, InputEvent::Dismiss);
        }
    }

    pub fn capture(&self) -> CaptureResult {
        let (tx, rx) = mpsc::channel();
        match self.shared.capture.lock() {
            Ok(mut slot) if slot.is_none() => *slot = Some(tx),
            _ => {
                return CaptureResult {
                    combo: None,
                    reason: Some("Shortcut capture is already active".into()),
                };
            }
        }
        if let Ok(mut keys) = self.shared.keys.lock() {
            keys.capture_peak.clear();
        }
        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(combo) => CaptureResult {
                combo: Some(combo),
                reason: None,
            },
            Err(_) => {
                if let Ok(mut slot) = self.shared.capture.lock() {
                    *slot = None;
                }
                CaptureResult {
                    combo: None,
                    reason: Some("timeout".into()),
                }
            }
        }
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        if let Some(shared) = SHARED.get() {
            let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
            let up = wparam.0 == WM_KEYUP as usize || wparam.0 == WM_SYSKEYUP as usize;
            if down || up {
                handle_key(
                    shared,
                    event.vkCode,
                    down,
                    event.flags.contains(LLKHF_INJECTED),
                );
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn handle_key(shared: &Shared, vk: u32, down: bool, injected: bool) {
    handle_key_with_state(shared, vk, down, injected, physical_key_down);
}

fn physical_key_down(vk: u32) -> bool {
    // Low-level hooks run before Windows updates the current event's async
    // state. The event itself is applied separately below.
    #[cfg(not(test))]
    return unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) } < 0;
    #[cfg(test)]
    {
        let _ = vk;
        true // Unit tests supply explicit snapshots when simulating missed releases.
    }
}

fn handle_key_with_state(
    shared: &Shared,
    vk: u32,
    down: bool,
    injected: bool,
    is_down: impl Fn(u32) -> bool,
) {
    // Dictation apps paste with synthetic modifiers, sometimes before the user
    // releases the other shortcut key. Those events must not alter physical state.
    if injected {
        #[cfg(debug_assertions)]
        if let Some(modifier) = modifier_for(vk) {
            eprintln!(
                "{} ignored injected modifier {modifier:?} down={down}",
                crate::discord::now_ms()
            );
        }
        return;
    }
    let modifier = modifier_for(vk);
    let capturing = shared
        .capture
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false);
    let mut keys = match shared.keys.lock() {
        Ok(keys) => keys,
        Err(_) => return,
    };

    // Remove missed key-ups, but never add keys from Windows' async state:
    // synthetic input must not manufacture a physically observed shortcut.
    keys.physical_down
        .retain(|held| *held == vk || is_down(*held));
    if down {
        keys.physical_down.insert(vk);
    } else {
        keys.physical_down.remove(&vk);
    }
    keys.mods = keys
        .physical_down
        .iter()
        .filter_map(|held| modifier_for(*held))
        .collect();
    keys.pressed_keys = keys
        .physical_down
        .iter()
        .filter(|held| modifier_for(**held).is_none())
        .map(|held| key_name(*held))
        .collect();

    if capturing {
        if modifier.is_some() {
            if down {
                let pressed = keys.mods.iter().copied().collect::<Vec<_>>();
                keys.capture_peak.extend(pressed);
            } else if !keys.capture_peak.is_empty() {
                let combo = shortcut_from(&keys.capture_peak, String::new());
                drop(keys);
                finish_capture(shared, combo);
            }
        } else if down {
            if vk == VK_ESCAPE.0 as u32 {
                if let Ok(mut slot) = shared.capture.lock() {
                    *slot = None;
                }
            } else {
                let combo = shortcut_from(&keys.mods, key_name(vk));
                drop(keys);
                finish_capture(shared, combo);
            }
        }
        return;
    }

    let config = shared.config.get();
    if config.dictation_app == DictationApp::Wispr && down && vk == VK_ESCAPE.0 as u32 {
        send_event(&shared.event_tx, InputEvent::Dismiss);
        return;
    }
    evaluate_shortcuts_for_event(&mut keys, &config, &shared.event_tx, down);
}

#[cfg(test)]
fn evaluate_shortcuts(keys: &mut Keys, config: &Config, event_tx: &mpsc::Sender<InputEvent>) {
    evaluate_shortcuts_for_event(keys, config, event_tx, true);
}

fn evaluate_shortcuts_for_event(
    keys: &mut Keys,
    config: &Config,
    event_tx: &mpsc::Sender<InputEvent>,
    allow_start: bool,
) {
    let required_down = shortcut_down(&config.shortcut, keys);
    if allow_start && required_down && exact_mods(&keys.mods, &config.shortcut.mods) && !keys.active
    {
        keys.active = true;
        send_event(event_tx, InputEvent::Press);
    } else if keys.active && !required_down {
        keys.active = false;
        send_event(event_tx, InputEvent::Release);
    }
    if config.dictation_app == DictationApp::Wispr {
        let required_down = shortcut_down(&config.hands_free_shortcut, keys);
        if allow_start
            && required_down
            && exact_mods(&keys.mods, &config.hands_free_shortcut.mods)
            && !keys.hands_free_active
        {
            keys.hands_free_active = true;
            send_event(event_tx, InputEvent::HandsFreePress);
        } else if keys.hands_free_active && !required_down {
            keys.hands_free_active = false;
        }
    }
}

fn send_event(tx: &mpsc::Sender<InputEvent>, event: InputEvent) {
    #[cfg(debug_assertions)]
    eprintln!("{} input {event:?}", crate::discord::now_ms());
    let _ = tx.send(event);
}

fn shortcut_down(shortcut: &Shortcut, keys: &Keys) -> bool {
    shortcut
        .mods
        .iter()
        .all(|value| keys.mods.contains(&to_mod_key(value)))
        && (shortcut.key.is_empty() || keys.pressed_keys.contains(&shortcut.key))
}

fn finish_capture(shared: &Shared, combo: Shortcut) {
    if let Ok(mut slot) = shared.capture.lock() {
        if let Some(sender) = slot.take() {
            let _ = sender.send(combo);
        }
    }
}

fn shortcut_from(mods: &HashSet<ModKey>, key: String) -> Shortcut {
    let ordered = [ModKey::Ctrl, ModKey::Alt, ModKey::Cmd, ModKey::Shift]
        .into_iter()
        .filter(|value| mods.contains(value))
        .map(from_mod_key)
        .collect();
    Shortcut { mods: ordered, key }
}

fn exact_mods(pressed: &HashSet<ModKey>, required: &[Mod]) -> bool {
    pressed.len() == required.len()
        && required
            .iter()
            .all(|value| pressed.contains(&to_mod_key(value)))
}

fn modifier_for(vk: u32) -> Option<ModKey> {
    match vk as u16 {
        value if [VK_CONTROL.0, VK_LCONTROL.0, VK_RCONTROL.0].contains(&value) => {
            Some(ModKey::Ctrl)
        }
        value if [VK_MENU.0, VK_LMENU.0, VK_RMENU.0].contains(&value) => Some(ModKey::Alt),
        value if [VK_LWIN.0, VK_RWIN.0].contains(&value) => Some(ModKey::Cmd),
        value if [VK_SHIFT.0, VK_LSHIFT.0, VK_RSHIFT.0].contains(&value) => Some(ModKey::Shift),
        _ => None,
    }
}

fn key_name(vk: u32) -> String {
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return char::from_u32(vk).unwrap_or('?').to_string();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x6F);
    }
    format!("VK_{vk:X}")
}

fn to_mod_key(value: &Mod) -> ModKey {
    match value {
        Mod::Ctrl => ModKey::Ctrl,
        Mod::Alt => ModKey::Alt,
        Mod::Cmd => ModKey::Cmd,
        Mod::Shift => ModKey::Shift,
    }
}
fn from_mod_key(value: ModKey) -> Mod {
    match value {
        ModKey::Ctrl => Mod::Ctrl,
        ModKey::Alt => Mod::Alt,
        ModKey::Cmd => Mod::Cmd,
        ModKey::Shift => Mod::Shift,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyboard_test_shared() -> (Shared, mpsc::Receiver<InputEvent>) {
        let (event_tx, rx) = mpsc::channel();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bridge-input-{unique}.json"));
        (
            Shared {
                config: Arc::new(ConfigStore::load(path)),
                event_tx,
                keys: Mutex::new(Keys::default()),
                capture: Mutex::new(None),
            },
            rx,
        )
    }

    #[test]
    fn control_alone_never_starts_the_default_shortcut() {
        let (shared, rx) = keyboard_test_shared();
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn missed_windows_release_cannot_turn_control_into_a_shortcut() {
        let (shared, rx) = keyboard_test_shared();
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Release);
        // Win was released but its hook event was missed.
        handle_key_with_state(&shared, VK_LCONTROL.0 as u32, true, false, |_| false);
        assert!(rx.try_recv().is_err());
        assert!(!shared.keys.lock().unwrap().mods.contains(&ModKey::Cmd));
        // A genuinely new Win press must still work while Ctrl is held.
        handle_key_with_state(&shared, VK_LWIN.0 as u32, true, false, |_| true);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
    }

    #[test]
    fn windows_async_state_cannot_add_an_unobserved_modifier() {
        let (shared, rx) = keyboard_test_shared();
        // Async state may contain injected keys; only observed physical downs
        // can contribute to a new shortcut.
        handle_key_with_state(&shared, VK_LCONTROL.0 as u32, true, false, |_| true);
        assert!(rx.try_recv().is_err());
        assert!(!shared.keys.lock().unwrap().mods.contains(&ModKey::Cmd));
    }

    #[test]
    fn missed_releases_end_an_active_hold_on_the_next_physical_event() {
        let (shared, rx) = keyboard_test_shared();
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        handle_key_with_state(&shared, VK_LCONTROL.0 as u32, true, false, |_| false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Release);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn releasing_one_side_does_not_forget_the_other_control_key() {
        let (shared, rx) = keyboard_test_shared();
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_RCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert!(rx.try_recv().is_err());
        handle_key(&shared, VK_RCONTROL.0 as u32, false, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Release);
    }

    #[test]
    fn missed_nonmodifier_release_cannot_complete_a_custom_shortcut() {
        let (mut shared, rx) = keyboard_test_shared();
        let path = std::env::temp_dir().join(format!(
            "bridge-custom-input-{}.json",
            crate::discord::now_ms()
        ));
        shared.config = Arc::new(ConfigStore::load(path.clone()));
        let mut config = shared.config.get();
        config.shortcut = Shortcut {
            mods: vec![Mod::Ctrl],
            key: "F8".into(),
        };
        shared.config.replace_public(config).unwrap();
        handle_key(&shared, 0x77, true, false);
        handle_key_with_state(&shared, VK_LCONTROL.0 as u32, true, false, |_| false);
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn capture_key_releases_do_not_leave_windows_stuck() {
        let (shared, events) = keyboard_test_shared();
        let (tx, captured) = mpsc::channel();
        *shared.capture.lock().unwrap() = Some(tx);
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert_eq!(captured.try_recv().unwrap().mods, vec![Mod::Ctrl, Mod::Cmd]);
        handle_key(&shared, VK_LWIN.0 as u32, false, false);
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn keyup_cannot_start_a_shortcut_when_an_extra_modifier_is_released() {
        let (shared, events) = keyboard_test_shared();
        handle_key(&shared, VK_LSHIFT.0 as u32, true, false);
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        handle_key(&shared, VK_LSHIFT.0 as u32, false, false);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn injected_paste_modifier_cannot_restart_after_physical_release() {
        let (shared, rx) = keyboard_test_shared();
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        // Release Ctrl first, leaving Win physically held while dictation pastes.
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Release);
        handle_key(&shared, VK_LCONTROL.0 as u32, true, true);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, true);
        handle_key(&shared, VK_LWIN.0 as u32, false, false);
        assert!(
            rx.try_recv().is_err(),
            "paste must not start another mute cycle"
        );
        assert!(shared.keys.lock().unwrap().mods.is_empty());
    }

    #[test]
    fn injected_modifier_release_cannot_end_a_physical_hold() {
        let (shared, rx) = keyboard_test_shared();
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LWIN.0 as u32, true, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, true);
        handle_key(&shared, VK_LCONTROL.0 as u32, true, true);
        assert!(
            rx.try_recv().is_err(),
            "injected keys must not change a physical hold"
        );
        handle_key(&shared, VK_LWIN.0 as u32, false, false);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Release);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn injected_shortcut_cannot_start_dictation() {
        let (shared, rx) = keyboard_test_shared();
        for (vk, down) in [
            (VK_LCONTROL.0, true),
            (VK_LWIN.0, true),
            (VK_LCONTROL.0, false),
            (VK_LWIN.0, false),
        ] {
            handle_key(&shared, vk as u32, down, true);
        }
        assert!(rx.try_recv().is_err());
        assert!(shared.keys.lock().unwrap().mods.is_empty());
    }

    #[test]
    fn injected_keys_cannot_complete_shortcut_capture() {
        let (shared, _) = keyboard_test_shared();
        let (tx, rx) = mpsc::channel();
        *shared.capture.lock().unwrap() = Some(tx);
        handle_key(&shared, VK_LCONTROL.0 as u32, true, true);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, true);
        assert!(rx.try_recv().is_err());
        handle_key(&shared, VK_LCONTROL.0 as u32, true, false);
        handle_key(&shared, VK_LCONTROL.0 as u32, false, false);
        assert_eq!(rx.try_recv().unwrap().mods, vec![Mod::Ctrl]);
    }

    #[test]
    fn wispr_hands_free_emits_once_per_press_and_primary_releases() {
        let config = Config {
            dictation_app: DictationApp::Wispr,
            ..Config::default()
        };
        let (tx, rx) = mpsc::channel();
        let mut keys = Keys::default();
        keys.mods.extend([ModKey::Ctrl, ModKey::Cmd]);
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        keys.pressed_keys.insert("VK_20".into());
        evaluate_shortcuts(&mut keys, &config, &tx);
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::HandsFreePress);
        assert!(rx.try_recv().is_err());
        keys.mods.clear();
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Release);
        keys.mods.extend([ModKey::Ctrl, ModKey::Cmd]);
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::HandsFreePress);
    }

    #[test]
    fn willow_ignores_the_hands_free_shortcut() {
        let config = Config::default();
        let (tx, rx) = mpsc::channel();
        let mut keys = Keys::default();
        keys.mods.extend([ModKey::Ctrl, ModKey::Cmd]);
        keys.pressed_keys.insert("VK_20".into());
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::Press);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn custom_hands_free_keys_are_supported() {
        let mut config = Config {
            dictation_app: DictationApp::Wispr,
            ..Config::default()
        };
        config.hands_free_shortcut = Shortcut {
            mods: vec![Mod::Alt],
            key: "F8".into(),
        };
        let (tx, rx) = mpsc::channel();
        let mut keys = Keys::default();
        keys.mods.insert(ModKey::Alt);
        keys.pressed_keys.insert("F8".into());
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert_eq!(rx.try_recv().unwrap(), InputEvent::HandsFreePress);
        keys.pressed_keys.clear();
        evaluate_shortcuts(&mut keys, &config, &tx);
        assert!(!keys.hands_free_active);
        assert!(rx.try_recv().is_err());
    }
}
