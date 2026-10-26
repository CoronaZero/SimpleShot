//! 全局热键：双热键注册（框选/固定区域）、文本解析/格式化、修改监听（临时低级键盘钩子）

use crate::util::Res;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
    RegisterHotKey, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_TAB,
    UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HHOOK, KBDLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// 热键注册 ID：交互框选截图
pub const HOTKEY_CAPTURE: i32 = 1;
/// 热键注册 ID：固定区域截图
pub const HOTKEY_FIXED: i32 = 2;

/// 主窗口消息：钩子捕获到新组合
/// wparam = (目标热键 ID << 16) | 修饰键，lparam = 虚拟键码
pub const M_HOOK_SET: u32 = WM_APP + 4;
/// 主窗口消息：用户按 Esc 取消修改
pub const M_HOOK_CANCEL: u32 = WM_APP + 5;

static HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);
static HOOK_TARGET: AtomicIsize = AtomicIsize::new(0);
/// 本次监听要修改的热键 ID
static LISTEN_ID: AtomicIsize = AtomicIsize::new(0);

/// 注册全局热键（内部统一附加 MOD_NOREPEAT 防按住重复触发）
pub fn register(hwnd: HWND, id: i32, mods: u32, vk: u32) -> Res<()> {
    unsafe {
        RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0), vk)
            .map_err(|e| format!("热键 {} 注册失败：{e}", format_hotkey(mods, vk)))
    }
}

/// 注销全局热键
pub fn unregister(hwnd: HWND, id: i32) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), id);
    }
}

/// 解析热键文本（如 "Ctrl+Alt+A"、"PrtSc"）为 (修饰键, 虚拟键码)；失败返回 None
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut mods = 0u32;
    let mut vk = 0u32;
    for part in s.split('+') {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL.0,
            "alt" => mods |= MOD_ALT.0,
            "shift" => mods |= MOD_SHIFT.0,
            "win" | "super" | "meta" => mods |= MOD_WIN.0,
            name => {
                // 只允许一个主键
                if vk != 0 {
                    return None;
                }
                vk = key_to_vk(name)?;
            }
        }
    }
    if vk == 0 {
        return None;
    }
    Some((mods, vk))
}

/// 单个键名 -> 虚拟键码
fn key_to_vk(name: &str) -> Option<u32> {
    let name = name.to_ascii_lowercase();
    Some(match name.as_str() {
        "prtsc" | "printscreen" | "print" => VK_SNAPSHOT.0 as u32,
        "space" => VK_SPACE.0 as u32,
        "tab" => VK_TAB.0 as u32,
        // Esc 保留给“取消”语义，不允许做热键
        "esc" | "escape" => return None,
        _ if name.len() == 1 => {
            let c = name.as_bytes()[0];
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase() as u32
            } else {
                return None;
            }
        }
        _ => {
            // F1..F12
            let f = name.strip_prefix('f')?;
            let n: u32 = f.parse().ok()?;
            if (1..=12).contains(&n) {
                0x6F + n
            } else {
                return None;
            }
        }
    })
}

/// 虚拟键码 -> 键名
fn vk_to_name(vk: u32) -> String {
    let v = vk as u16;
    if v == VK_SNAPSHOT.0 {
        "PrtSc".into()
    } else if v == VK_SPACE.0 {
        "Space".into()
    } else if v == VK_TAB.0 {
        "Tab".into()
    } else if (0x70..=0x7B).contains(&v) {
        format!("F{}", v - 0x6F)
    } else if (v as u8).is_ascii_alphanumeric() {
        (v as u8 as char).to_string()
    } else {
        format!("0x{vk:X}")
    }
}

/// (修饰键, 虚拟键码) -> 可读文本（如 "Ctrl+Alt+A"）；vk=0 返回空串（未设置）
pub fn format_hotkey(mods: u32, vk: u32) -> String {
    if vk == 0 {
        return String::new();
    }
    let mut s = String::new();
    if mods & MOD_CONTROL.0 != 0 {
        s += "Ctrl+";
    }
    if mods & MOD_ALT.0 != 0 {
        s += "Alt+";
    }
    if mods & MOD_SHIFT.0 != 0 {
        s += "Shift+";
    }
    if mods & MOD_WIN.0 != 0 {
        s += "Win+";
    }
    s + &vk_to_name(vk)
}

/// 是否为修饰键本身
fn is_modifier(vk: u32) -> bool {
    let v = vk as u16;
    v == VK_LCONTROL.0
        || v == VK_RCONTROL.0
        || v == VK_LMENU.0
        || v == VK_RMENU.0
        || v == VK_LSHIFT.0
        || v == VK_RSHIFT.0
        || v == VK_LWIN.0
        || v == VK_RWIN.0
        || v == VK_CONTROL.0
        || v == VK_MENU.0
        || v == VK_SHIFT.0
}

/// 进入快捷键监听模式：安装临时低级键盘钩子，捕获一次按键组合
/// `id` 指明本次修改的是哪个热键（HOTKEY_CAPTURE / HOTKEY_FIXED）
pub fn start_listening(hwnd: HWND, id: i32) -> Res<()> {
    HOOK_TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
    LISTEN_ID.store(id as isize, Ordering::SeqCst);
    unsafe {
        let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0)
            .map_err(|e| format!("安装键盘钩子失败：{e}"))?;
        HOOK_HANDLE.store(h.0 as isize, Ordering::SeqCst);
    }
    Ok(())
}

/// 停止监听（幂等）
pub fn stop_listening() {
    let h = HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
        }
    }
}

/// 低级键盘钩子回调：监听期间吞掉按键，捕获到主键后通知主窗口
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let target = HOOK_TARGET.load(Ordering::SeqCst);
        if target == 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let msg = wparam.0 as u32;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = kb.vkCode;
            if vk == VK_ESCAPE.0 as u32 {
                // Esc：取消修改
                let _ = PostMessageW(
                    Some(HWND(target as *mut c_void)),
                    M_HOOK_CANCEL,
                    WPARAM(0),
                    LPARAM(0),
                );
                stop_listening();
                return LRESULT(1);
            }
            if !is_modifier(vk) {
                // 按住修饰键的状态下按下主键：组装组合并派发
                let mut mods = 0u32;
                if GetAsyncKeyState(VK_CONTROL.0 as i32) < 0 {
                    mods |= MOD_CONTROL.0;
                }
                if GetAsyncKeyState(VK_MENU.0 as i32) < 0 {
                    mods |= MOD_ALT.0;
                }
                if GetAsyncKeyState(VK_SHIFT.0 as i32) < 0 {
                    mods |= MOD_SHIFT.0;
                }
                if GetAsyncKeyState(VK_LWIN.0 as i32) < 0 || GetAsyncKeyState(VK_RWIN.0 as i32) < 0 {
                    mods |= MOD_WIN.0;
                }
                let listen_id = LISTEN_ID.load(Ordering::SeqCst) as usize;
                let _ = PostMessageW(
                    Some(HWND(target as *mut c_void)),
                    M_HOOK_SET,
                    WPARAM((listen_id << 16) | mods as usize),
                    LPARAM(vk as isize),
                );
                stop_listening();
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}
