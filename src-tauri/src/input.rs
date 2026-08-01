use crate::config::{ConfigStore, Mod, Shortcut};
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
            CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG,
            SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
            WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Press,
    Release,
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
    mods: HashSet<ModKey>,
    active: bool,
    key_latched: bool,
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
            .name("willow-keyboard-hook".into())
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
                handle_key(shared, event.vkCode, down);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn handle_key(shared: &Shared, vk: u32, down: bool) {
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

    if capturing {
        if let Some(modifier) = modifier {
            if down {
                keys.mods.insert(modifier);
                let pressed = keys.mods.iter().copied().collect::<Vec<_>>();
                keys.capture_peak.extend(pressed);
            } else if !keys.capture_peak.is_empty() {
                let combo = shortcut_from(&keys.capture_peak, String::new());
                keys.mods.remove(&modifier);
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

    if let Some(modifier) = modifier {
        if down {
            keys.mods.insert(modifier);
        } else {
            keys.mods.remove(&modifier);
        }
        evaluate_modifier_shortcut(shared, &mut keys);
        return;
    }

    let shortcut = shared.config.get().shortcut;
    if shortcut.key.is_empty() {
        return;
    }
    if key_name(vk) != shortcut.key {
        return;
    }
    if down && !keys.key_latched && exact_mods(&keys.mods, &shortcut.mods) {
        keys.key_latched = true;
        let _ = shared.event_tx.send(InputEvent::Press);
    } else if !down && keys.key_latched {
        keys.key_latched = false;
        let _ = shared.event_tx.send(InputEvent::Release);
    }
}

fn evaluate_modifier_shortcut(shared: &Shared, keys: &mut Keys) {
    let shortcut = shared.config.get().shortcut;
    if !shortcut.key.is_empty() {
        return;
    }
    let required_down = shortcut
        .mods
        .iter()
        .all(|value| keys.mods.contains(&to_mod_key(value)));
    let exact = required_down && keys.mods.len() == shortcut.mods.len();
    if exact && !keys.active {
        keys.active = true;
        let _ = shared.event_tx.send(InputEvent::Press);
    } else if keys.active && !required_down {
        keys.active = false;
        let _ = shared.event_tx.send(InputEvent::Release);
    }
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
