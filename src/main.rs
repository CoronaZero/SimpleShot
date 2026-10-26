//! SimpleShot：轻量级 Windows 截图工具
//! 纯 Win32 实现，唯一依赖 windows-rs。托盘常驻 + 双热键 + 矩形/窗口混合截图
//! + 固定宽高比 + 固定区域一键截图。

#![windows_subsystem = "windows"]

mod actions;
mod capture;
mod config;
mod dialog;
mod hotkey;
mod overlay;
mod png;
mod tray;
mod util;

use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM, ERROR_ALREADY_EXISTS,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW,
    GetSystemMetrics, GetWindowLongPtrW, GWLP_USERDATA, GetMessageW, HICON, MessageBoxW,
    PostQuitMessage, RegisterClassExW, SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WNDCLASSEXW, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_HOTKEY, WM_NCCREATE,
    CREATESTRUCTW, MB_ICONERROR, MB_OK, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use hotkey::{M_HOOK_CANCEL, M_HOOK_SET};
use overlay::{M_REGION_CANCEL, M_REGION_DONE};
use tray::{
    TID_ASPECT_16_9, TID_ASPECT_1_1, TID_ASPECT_4_3, TID_ASPECT_9_16, TID_ASPECT_CUSTOM,
    TID_ASPECT_FREE, TID_ASPECT_LAST, TID_AUTOSTART, TID_CAPTURE, TID_CLIPBOARD, TID_EXIT,
    TID_FIXED_INPUT, TID_FIXED_LAST, TID_FIXED_NONE, TID_FIXED_SHOT, TID_FMT_BMP, TID_FMT_JPEG,
    TID_FMT_PNG, TID_HOTKEY, TID_HOTKEY2, TID_NOTIFY, TID_SAVE_ASK, TID_SAVE_FOLDER,
    TID_SAVE_NONE, WM_LBUTTONUP_VAL, WM_RBUTTONUP_VAL, WM_TRAY,
};

const MAIN_CLASS: &str = "SimpleShotMsgWnd";

/// 全局应用状态（挂在主窗口 GWLP_USERDATA）
struct App {
    cfg: config::Config,
    cfg_path: PathBuf,
    hmain: HWND,
    hicon: HICON,
    hoverlay: Option<HWND>,
    dib: Option<capture::Dib>,
    /// 最近一次框选区域（虚拟屏物理像素），供「使用最近一次选区」固定
    last_region: Option<RECT>,
}

fn main() {
    unsafe {
        // 高 DPI 降级链：PMv2(Win10 1703+/Server 2019+) → PMv1(Win10 1607) → 系统级 aware
        // 关键：必须保证进程至少处于系统级 aware。若停在 unaware，
        // GetSystemMetrics/EnumDisplayMonitors（虚拟化坐标）与 CreateDC（物理 DC）
        // 语义不一致，云电脑（RDP 动态分辨率/非 96 DPI）下会导致
        // 遮罩窗口与截屏内容只覆盖屏幕左上角
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err()
            && SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE).is_err()
        {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        }
        // COM STA：保存对话框 / WIC 需要
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // 单实例互斥体：已有实例则静默退出
        let name = util::to_wide("Local\\SimpleShot_Instance");
        let _mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        // 配置 + 自启路径修正
        let cfg_path = config::config_path();
        let cfg = config::load(&cfg_path);
        tray::autostart_fix_path();

        // 注册窗口类
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        register_main_class(hinst.into());
        if let Err(e) = overlay::register_class() {
            msgbox(&e);
            return;
        }

        // 创建隐藏消息窗口（承载热键与托盘回调）
        let app = Box::new(App {
            cfg,
            cfg_path,
            hmain: HWND(std::ptr::null_mut()),
            hicon: HICON(std::ptr::null_mut()),
            hoverlay: None,
            dib: None,
            last_region: None,
        });
        let name_w = util::to_wide(MAIN_CLASS);
        let _hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(name_w.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            Some(Box::into_raw(app) as *const c_void),
        ) {
            Ok(h) => h,
            Err(e) => {
                // WM_CREATE 返回 -1（托盘初始化失败的提示已弹出）
                msgbox(&format!("创建主窗口失败：{e}"));
                return;
            }
        };

        // 消息循环
        let mut msg: MSG = std::mem::zeroed();
        loop {
            // GetMessageW：0 = WM_QUIT，-1 = 错误，均退出循环
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn register_main_class(hinst: windows::Win32::Foundation::HINSTANCE) {
    let wname = util::to_wide(MAIN_CLASS);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: Default::default(),
        lpfnWndProc: Some(main_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: HICON(std::ptr::null_mut()),
        hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR(std::ptr::null_mut()),
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(wname.as_ptr()),
        hIconSm: HICON(std::ptr::null_mut()),
    };
    unsafe {
        let _ = RegisterClassExW(&wc);
    }
}

/// 主窗口过程：热键 / 托盘回调 / 菜单命令 / 截图结果 全部在此分发
unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p == 0 {
            if msg == WM_NCCREATE {
                // 安装应用状态（CreateWindowExW 传入的 Box 指针）
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                let mut app = Box::from_raw(cs.lpCreateParams as *mut App);
                app.hmain = hwnd;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *(p as *mut App);

        match msg {
            WM_CREATE => {
                // 初始化托盘与双热键
                match tray::add(hwnd) {
                    Ok(icon) => app.hicon = icon,
                    Err(e) => {
                        msgbox(&e);
                        return LRESULT(-1); // 取消窗口创建
                    }
                }
                if let Err(e) = hotkey::register(
                    hwnd,
                    hotkey::HOTKEY_CAPTURE,
                    app.cfg.hotkey_mods,
                    app.cfg.hotkey_vk,
                ) {
                    tray::balloon(hwnd, &format!("{e}，可通过托盘菜单更换快捷键"));
                }
                if app.cfg.hotkey2_vk != 0 {
                    if let Err(e) = hotkey::register(
                        hwnd,
                        hotkey::HOTKEY_FIXED,
                        app.cfg.hotkey2_mods,
                        app.cfg.hotkey2_vk,
                    ) {
                        tray::balloon(hwnd, &format!("{e}，可通过托盘菜单更换固定区域快捷键"));
                    }
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                match wparam.0 as i32 {
                    hotkey::HOTKEY_CAPTURE => start_capture(app),
                    hotkey::HOTKEY_FIXED => fixed_capture(app),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TRAY => {
                let mouse = lparam.0 as u32;
                if mouse == WM_LBUTTONUP_VAL {
                    start_capture(app);
                } else if mouse == WM_RBUTTONUP_VAL {
                    tray::show_menu(hwnd, &app.cfg, app.last_region.is_some());
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as u16;
                on_command(hwnd, app, id);
                LRESULT(0)
            }
            M_REGION_DONE => {
                on_region_done(hwnd, app);
                LRESULT(0)
            }
            M_REGION_CANCEL => {
                app.hoverlay = None;
                app.dib = None;
                LRESULT(0)
            }
            M_HOOK_SET => {
                hotkey::stop_listening();
                // wparam = (目标热键 ID << 16) | 修饰键，lparam = 虚拟键码
                let target = (wparam.0 >> 16) as i32;
                let mods = (wparam.0 & 0xFFFF) as u32;
                let vk = lparam.0 as u32;
                apply_new_hotkey(hwnd, app, target, mods, vk);
                LRESULT(0)
            }
            M_HOOK_CANCEL => {
                hotkey::stop_listening();
                tray::balloon(hwnd, "已取消修改快捷键");
                LRESULT(0)
            }
            WM_DESTROY => {
                tray::remove(hwnd);
                hotkey::unregister(hwnd, hotkey::HOTKEY_CAPTURE);
                hotkey::unregister(hwnd, hotkey::HOTKEY_FIXED);
                let _ = DestroyIcon(app.hicon);
                PostQuitMessage(0);
                // 释放应用状态
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(p as *mut App));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 触发一次交互截图（已有遮罩窗口或输入对话框打开时忽略）
fn start_capture(app: &mut App) {
    if app.hoverlay.is_some() || dialog::is_open() {
        return;
    }
    let dib = match capture::capture_virtual_screen() {
        Ok(d) => d,
        Err(e) => {
            tray::balloon(app.hmain, &e);
            return;
        }
    };
    match overlay::create(app.hmain, &dib, app.cfg.aspect) {
        Ok(h) => {
            app.hoverlay = Some(h);
            app.dib = Some(dib);
        }
        Err(e) => tray::balloon(app.hmain, &e),
    }
}

/// 固定区域一键截图：不进遮罩，直接后台截取并执行动作
fn fixed_capture(app: &mut App) {
    let Some(fixed) = app.cfg.fixed else { return };
    if dialog::is_open() {
        return;
    }
    let dib = match capture::capture_virtual_screen() {
        Ok(d) => d,
        Err(e) => {
            tray::balloon(app.hmain, &e);
            return;
        }
    };
    // 虚拟屏坐标转 DIB 本地坐标；crop 内部负责 clamp 到当前屏幕
    let rc_local = RECT {
        left: fixed.left - dib.vx,
        top: fixed.top - dib.vy,
        right: fixed.right - dib.vx,
        bottom: fixed.bottom - dib.vy,
    };
    match capture::crop(&dib, rc_local) {
        Some((px, w, h)) => actions::perform(app.hmain, &app.cfg, px, w, h),
        None => tray::balloon(app.hmain, "固定区域超出当前屏幕范围"),
    }
}

/// 处理选区完成：裁剪并执行截图后动作，同时记录最近选区（虚拟屏坐标）
fn on_region_done(hwnd: HWND, app: &mut App) {
    let rc = overlay::REGION.lock().ok().and_then(|mut g| g.take());
    let dib = app.dib.take();
    app.hoverlay = None;
    if let (Some(rc), Some(dib)) = (rc, dib) {
        app.last_region = Some(RECT {
            left: rc.left + dib.vx,
            top: rc.top + dib.vy,
            right: rc.right + dib.vx,
            bottom: rc.bottom + dib.vy,
        });
        if let Some((px, w, h)) = capture::crop(&dib, rc) {
            actions::perform(hwnd, &app.cfg, px, w, h);
        }
    }
}

/// 托盘菜单命令分发
fn on_command(hwnd: HWND, app: &mut App, id: u16) {
    match id {
        TID_CAPTURE => start_capture(app),
        TID_FIXED_SHOT => fixed_capture(app),
        TID_CLIPBOARD => {
            app.cfg.clipboard = !app.cfg.clipboard;
            config::save(&app.cfg, &app.cfg_path);
            warn_if_no_action(hwnd, app);
        }
        TID_SAVE_NONE => {
            app.cfg.save = config::SaveMode::None;
            config::save(&app.cfg, &app.cfg_path);
            warn_if_no_action(hwnd, app);
        }
        TID_SAVE_ASK => {
            app.cfg.save = config::SaveMode::Ask;
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_SAVE_FOLDER => {
            app.cfg.save = config::SaveMode::Folder;
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_FMT_PNG => {
            app.cfg.format = config::ImgFormat::Png;
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_FMT_JPEG => {
            app.cfg.format = config::ImgFormat::Jpeg;
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_FMT_BMP => {
            app.cfg.format = config::ImgFormat::Bmp;
            config::save(&app.cfg, &app.cfg_path);
        }
        // ---------- 宽高比 ----------
        TID_ASPECT_FREE => {
            app.cfg.aspect = None;
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_ASPECT_1_1 => {
            app.cfg.aspect = Some((1, 1));
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_ASPECT_4_3 => {
            app.cfg.aspect = Some((4, 3));
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_ASPECT_16_9 => {
            app.cfg.aspect = Some((16, 9));
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_ASPECT_9_16 => {
            app.cfg.aspect = Some((9, 16));
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_ASPECT_CUSTOM => {
            if dialog::is_open() {
                return;
            }
            let cur = app.cfg.aspect.unwrap_or((16, 9));
            let init = dialog::DlgInit {
                kind: dialog::DlgKind::Aspect,
                fixed: RECT::default(),
                aspect: cur,
            };
            if let Some(dialog::DlgOut::Ratio(w, h)) = dialog::run(hwnd, init) {
                app.cfg.aspect = Some((w, h));
                config::save(&app.cfg, &app.cfg_path);
                tray::balloon(hwnd, &format!("宽高比已锁定为 {w}:{h}"));
            }
        }
        TID_ASPECT_LAST => {
            // 化简上次选区的宽高为最简整数比
            if let Some(rc) = app.last_region {
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;
                if w > 0 && h > 0 {
                    let g = gcd(w, h);
                    app.cfg.aspect = Some((w / g, h / g));
                    config::save(&app.cfg, &app.cfg_path);
                    tray::balloon(
                        hwnd,
                        &format!("宽高比已锁定为 {}:{}", w / g, h / g),
                    );
                }
            }
        }
        // ---------- 固定区域 ----------
        TID_FIXED_NONE => {
            if app.cfg.fixed.take().is_some() {
                config::save(&app.cfg, &app.cfg_path);
                tray::balloon(hwnd, "已清除固定区域");
            }
        }
        TID_FIXED_LAST => {
            if let Some(rc) = app.last_region {
                app.cfg.fixed = Some(rc);
                config::save(&app.cfg, &app.cfg_path);
                tray::balloon(
                    hwnd,
                    &format!(
                        "固定区域已设为 ({}, {})-({}, {})",
                        rc.left, rc.top, rc.right, rc.bottom
                    ),
                );
            }
        }
        TID_FIXED_INPUT => {
            if dialog::is_open() {
                return;
            }
            let initial = app.cfg.fixed.unwrap_or_else(default_fixed_rect);
            let init = dialog::DlgInit {
                kind: dialog::DlgKind::FixedRect,
                fixed: initial,
                aspect: (16, 9),
            };
            if let Some(dialog::DlgOut::Rect(rc)) = dialog::run(hwnd, init) {
                app.cfg.fixed = Some(rc);
                config::save(&app.cfg, &app.cfg_path);
                tray::balloon(
                    hwnd,
                    &format!(
                        "固定区域已设为 ({}, {})-({}, {})",
                        rc.left, rc.top, rc.right, rc.bottom
                    ),
                );
            }
        }
        // ---------- 其他设置 ----------
        TID_NOTIFY => {
            app.cfg.notify = !app.cfg.notify;
            config::save(&app.cfg, &app.cfg_path);
        }
        TID_AUTOSTART => {
            let on = !tray::autostart_enabled();
            match tray::autostart_set(on) {
                Ok(()) => tray::balloon(
                    hwnd,
                    if on { "已开启开机自启" } else { "已关闭开机自启" },
                ),
                Err(e) => tray::balloon(hwnd, &e),
            }
        }
        TID_HOTKEY => match hotkey::start_listening(hwnd, hotkey::HOTKEY_CAPTURE) {
            Ok(()) => tray::balloon(hwnd, "请按下新的快捷键组合（按 Esc 取消）"),
            Err(e) => tray::balloon(hwnd, &e),
        },
        TID_HOTKEY2 => match hotkey::start_listening(hwnd, hotkey::HOTKEY_FIXED) {
            Ok(()) => tray::balloon(hwnd, "请按下新的固定区域快捷键组合（按 Esc 取消）"),
            Err(e) => tray::balloon(hwnd, &e),
        },
        TID_EXIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

/// 最大公约数（宽高比化简用）
fn gcd(mut a: i32, mut b: i32) -> i32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.abs().max(1)
}

/// 输入对话框的默认值：屏幕中心 400×300
fn default_fixed_rect() -> RECT {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let cx = vx + vw / 2;
        let cy = vy + vh / 2;
        RECT {
            left: cx - 200,
            top: cy - 150,
            right: cx + 200,
            bottom: cy + 150,
        }
    }
}

/// 剪贴板与保存均未开启时提醒用户
fn warn_if_no_action(hwnd: HWND, app: &App) {
    if !app.cfg.has_action() {
        tray::balloon(hwnd, "当前未设置任何截图后动作，截图将被丢弃");
    }
}

/// 应用新热键：按目标分别写回；注册失败则回退旧热键
fn apply_new_hotkey(hwnd: HWND, app: &mut App, target: i32, mods: u32, vk: u32) {
    let (cur_mods, cur_vk) = if target == hotkey::HOTKEY_FIXED {
        (app.cfg.hotkey2_mods, app.cfg.hotkey2_vk)
    } else {
        (app.cfg.hotkey_mods, app.cfg.hotkey_vk)
    };
    hotkey::unregister(hwnd, target);
    if hotkey::register(hwnd, target, mods, vk).is_ok() {
        if target == hotkey::HOTKEY_FIXED {
            app.cfg.hotkey2_mods = mods;
            app.cfg.hotkey2_vk = vk;
        } else {
            app.cfg.hotkey_mods = mods;
            app.cfg.hotkey_vk = vk;
        }
        config::save(&app.cfg, &app.cfg_path);
        tray::balloon(hwnd, &format!("新快捷键：{}", hotkey::format_hotkey(mods, vk)));
    } else {
        // 新组合被占用，恢复旧热键
        if cur_vk != 0 {
            let _ = hotkey::register(hwnd, target, cur_mods, cur_vk);
        }
        let cur_name = if cur_vk == 0 {
            "未设置".to_string()
        } else {
            hotkey::format_hotkey(cur_mods, cur_vk)
        };
        tray::balloon(hwnd, &format!("该组合已被其他程序占用，仍使用 {cur_name}"));
    }
}

/// 致命错误消息框
fn msgbox(text: &str) {
    unsafe {
        let w = util::to_wide(text);
        let t = util::to_wide("SimpleShot");
        let _ = MessageBoxW(None, PCWSTR(w.as_ptr()), PCWSTR(t.as_ptr()), MB_OK | MB_ICONERROR);
    }
}
