//! 系统托盘：图标、菜单、气泡提示、开机自启（注册表）

use crate::config::{Config, SaveMode};
use crate::hotkey;
use crate::util::{self, Res};
use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, POINT, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, HICON,
    HMENU, IMAGE_ICON, LR_DEFAULTCOLOR, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MENU_ITEM_FLAGS, MF_GRAYED, LoadImageW, SetForegroundWindow, TrackPopupMenu,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP,
};

/// 托盘回调消息（NOTIFYICONDATAW.uCallbackMessage）
pub const WM_TRAY: u32 = WM_APP + 1;

// Win32 鼠标消息值（NOTIFYICONDATAW 低版本回调直接传 lParam）
pub const WM_LBUTTONUP_VAL: u32 = 0x0202;
pub const WM_RBUTTONUP_VAL: u32 = 0x0205;

// 菜单项 ID
pub const TID_CAPTURE: u16 = 100;
pub const TID_CLIPBOARD: u16 = 101;
pub const TID_SAVE_NONE: u16 = 110;
pub const TID_SAVE_ASK: u16 = 111;
pub const TID_SAVE_FOLDER: u16 = 112;
pub const TID_FMT_PNG: u16 = 120;
pub const TID_FMT_JPEG: u16 = 121;
pub const TID_FMT_BMP: u16 = 122;
pub const TID_AUTOSTART: u16 = 130;
pub const TID_HOTKEY: u16 = 140;
pub const TID_EXIT: u16 = 150;
// v2.0 新增
pub const TID_FIXED_SHOT: u16 = 160;
pub const TID_ASPECT_FREE: u16 = 170;
pub const TID_ASPECT_1_1: u16 = 171;
pub const TID_ASPECT_4_3: u16 = 172;
pub const TID_ASPECT_16_9: u16 = 173;
pub const TID_ASPECT_9_16: u16 = 174;
pub const TID_ASPECT_CUSTOM: u16 = 175;
pub const TID_ASPECT_LAST: u16 = 176;
pub const TID_FIXED_NONE: u16 = 180;
pub const TID_FIXED_LAST: u16 = 181;
pub const TID_FIXED_INPUT: u16 = 182;
pub const TID_NOTIFY: u16 = 190;
pub const TID_HOTKEY2: u16 = 200;

const TRAY_UID: u32 = 1;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "ScreenshotRS";

/// 添加托盘图标，返回图标句柄（退出时需 DestroyIcon）
pub fn add(hwnd: HWND) -> Res<HICON> {
    unsafe {
        let icon = make_icon()?;
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        let tip = util::to_wide("SimpleShot截图工具");
        for (i, c) in tip.iter().enumerate().take(127) {
            nid.szTip[i] = *c;
        }
        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            Ok(icon)
        } else {
            let _ = DestroyIcon(icon);
            Err("添加托盘图标失败".into())
        }
    }
}

/// 移除托盘图标（退出时调用，避免残留“幽灵图标”）
pub fn remove(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 托盘气泡提示
pub fn balloon(hwnd: HWND, text: &str) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        let info = util::to_wide(text);
        for (i, c) in info.iter().enumerate().take(255) {
            nid.szInfo[i] = *c;
        }
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// 弹出托盘右键菜单；选择项通过 WM_COMMAND 派发回 hwnd
/// `has_last`：是否存在最近一次选区（用于固定区域子菜单的可用性）
pub fn show_menu(hwnd: HWND, cfg: &Config, has_last: bool) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let checked = |on: bool| {
            if on {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };
        let radio = |on: bool| {
            if on {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };

        // 立即截图
        append(menu, MF_STRING, TID_CAPTURE as usize, "立即截图");
        // 截取固定区域（未设置置灰）
        append(
            menu,
            if cfg.fixed.is_some() { MF_STRING } else { MF_STRING | MF_GRAYED },
            TID_FIXED_SHOT as usize,
            "截取固定区域",
        );
        // 复制到剪贴板（开关）
        append(menu, checked(cfg.clipboard), TID_CLIPBOARD as usize, "复制到剪贴板");
        // 保存到文件（三选一）
        let save = CreatePopupMenu().unwrap_or_default();
        append(save, radio(cfg.save == SaveMode::None), TID_SAVE_NONE as usize, "不保存");
        append(save, radio(cfg.save == SaveMode::Ask), TID_SAVE_ASK as usize, "保存前询问位置");
        append(save, radio(cfg.save == SaveMode::Folder), TID_SAVE_FOLDER as usize, "保存到「屏幕截图」文件夹");
        let w = util::to_wide("保存到文件");
        let _ = AppendMenuW(menu, MF_STRING | MF_POPUP, save.0 as usize, PCWSTR(w.as_ptr()));
        // 图片格式（三选一）
        let fmt = CreatePopupMenu().unwrap_or_default();
        append(fmt, radio(cfg.format == crate::config::ImgFormat::Png), TID_FMT_PNG as usize, "PNG");
        append(fmt, radio(cfg.format == crate::config::ImgFormat::Jpeg), TID_FMT_JPEG as usize, "JPEG");
        append(fmt, radio(cfg.format == crate::config::ImgFormat::Bmp), TID_FMT_BMP as usize, "BMP");
        let w = util::to_wide("图片格式");
        let _ = AppendMenuW(menu, MF_STRING | MF_POPUP, fmt.0 as usize, PCWSTR(w.as_ptr()));

        // ---------- 设置子菜单 ----------
        let settings = CreatePopupMenu().unwrap_or_default();
        // 宽高比（自由/五档预设/自定义/上次截图比例）
        let aspect = CreatePopupMenu().unwrap_or_default();
        append(aspect, radio(cfg.aspect.is_none()), TID_ASPECT_FREE as usize, "自由");
        append(aspect, radio(cfg.aspect == Some((1, 1))), TID_ASPECT_1_1 as usize, "1:1");
        append(aspect, radio(cfg.aspect == Some((4, 3))), TID_ASPECT_4_3 as usize, "4:3");
        append(aspect, radio(cfg.aspect == Some((16, 9))), TID_ASPECT_16_9 as usize, "16:9");
        append(aspect, radio(cfg.aspect == Some((9, 16))), TID_ASPECT_9_16 as usize, "9:16");
        // 当前值为非预设时在自定义项上显示当前值
        let is_preset = matches!(
            cfg.aspect,
            None | Some((1, 1)) | Some((4, 3)) | Some((16, 9)) | Some((9, 16))
        );
        let custom_text = match cfg.aspect {
            Some((w, h)) if !is_preset => format!("自定义…（当前 {w}:{h}）"),
            _ => "自定义…".to_string(),
        };
        append(aspect, MF_STRING, TID_ASPECT_CUSTOM as usize, &custom_text);
        append(
            aspect,
            if has_last { MF_STRING } else { MF_STRING | MF_GRAYED },
            TID_ASPECT_LAST as usize,
            "使用上次截图比例",
        );
        let w = util::to_wide("宽高比");
        let _ = AppendMenuW(settings, MF_STRING | MF_POPUP, aspect.0 as usize, PCWSTR(w.as_ptr()));
        // 固定区域
        let fixed = CreatePopupMenu().unwrap_or_default();
        let (fixed_state_text, fixed_state_on) = match cfg.fixed {
            None => ("未设置".to_string(), true),
            Some(rc) => (
                format!("已设置 ({}, {})-({}, {})，点击清除", rc.left, rc.top, rc.right, rc.bottom),
                true,
            ),
        };
        append(fixed, checked(fixed_state_on), TID_FIXED_NONE as usize, &fixed_state_text);
        append(
            fixed,
            if has_last { MF_STRING } else { MF_STRING | MF_GRAYED },
            TID_FIXED_LAST as usize,
            "使用最近一次选区",
        );
        append(fixed, MF_STRING, TID_FIXED_INPUT as usize, "手动输入…");
        let w = util::to_wide("固定区域");
        let _ = AppendMenuW(settings, MF_STRING | MF_POPUP, fixed.0 as usize, PCWSTR(w.as_ptr()));
        // 两个快捷键
        append(
            settings,
            MF_STRING,
            TID_HOTKEY as usize,
            &format!(
                "修改快捷键…（当前：{}）",
                hotkey::format_hotkey(cfg.hotkey_mods, cfg.hotkey_vk)
            ),
        );
        let hk2 = hotkey::format_hotkey(cfg.hotkey2_mods, cfg.hotkey2_vk);
        append(
            settings,
            MF_STRING,
            TID_HOTKEY2 as usize,
            &format!("修改固定区域快捷键…（当前：{}）", if hk2.is_empty() { "未设置".into() } else { hk2 }),
        );
        // 截图完成提示（开关，错误不受影响）
        append(settings, checked(cfg.notify), TID_NOTIFY as usize, "截图完成提示");
        // 开机自启（开关）
        append(settings, checked(autostart_enabled()), TID_AUTOSTART as usize, "开机自启");
        let w = util::to_wide("设置");
        let _ = AppendMenuW(menu, MF_STRING | MF_POPUP, settings.0 as usize, PCWSTR(w.as_ptr()));

        // 分隔线 + 退出
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        append(menu, MF_STRING, TID_EXIT as usize, "退出");

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 前置主窗口，否则菜单点击外部区域时不会关闭（Shell_NotifyIcon 经典问题）
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}

fn append(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let w = util::to_wide(text);
    unsafe {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(w.as_ptr()));
    }
}

/// 加载托盘图标：优先从 exe 内嵌资源加载（与 exe 文件图标同源，由 build.rs 生成嵌入），
/// 资源缺失时回退为运行时绘制同一图案
fn make_icon() -> Res<HICON> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        // 资源 ID = 1（RT_GROUP_ICON），与 exe 文件图标为同一资源
        let h = LoadImageW(
            Some(hinst.into()),
            PCWSTR(1usize as *const u16),
            IMAGE_ICON,
            32,
            32,
            LR_DEFAULTCOLOR,
        );
        if let Ok(h) = h {
            return Ok(HICON(h.0));
        }
    }
    draw_icon_runtime()
}

/// 运行时绘制托盘图标（图案与 build.rs 的 draw_icon_frame 保持一致）：
/// 取景框四角 + 中心点，避免依赖资源文件
fn draw_icon_runtime() -> Res<HICON> {
    const S: i32 = 32;
    // AND 掩码（1=透明），1bpp，行 4 字节对齐
    let mut and = vec![0xFFu8; (S as usize / 8) * S as usize];
    // XOR 彩色位，32bpp BGRA
    let mut xor = vec![0u8; (S as usize * 4) * S as usize];
    let mut put = |x: i32, y: i32, b: u8, g: u8, r: u8| {
        if !(0..S).contains(&x) || !(0..S).contains(&y) {
            return;
        }
        let px = y as usize * S as usize + x as usize;
        let off = px * 4;
        xor[off] = b;
        xor[off + 1] = g;
        xor[off + 2] = r;
        xor[off + 3] = 255;
        and[y as usize * 4 + (x / 8) as usize] &= !(1 << (7 - (x & 7)));
    };
    // 取景框：四角 L 形（天蓝色），线宽 3
    let (b, g, r) = (255u8, 120u8, 60u8);
    let t = 3;
    let m0 = 5;
    let m1 = S - 5;
    let arm = 8;
    for i in 0..arm {
        for k in 0..t {
            // 左上
            put(m0 + i, m0 + k, b, g, r);
            put(m0 + k, m0 + i, b, g, r);
            // 右上
            put(m1 - 1 - i, m0 + k, b, g, r);
            put(m1 - 1 - k, m0 + i, b, g, r);
            // 左下
            put(m0 + i, m1 - 1 - k, b, g, r);
            put(m0 + k, m1 - 1 - i, b, g, r);
            // 右下
            put(m1 - 1 - i, m1 - 1 - k, b, g, r);
            put(m1 - 1 - k, m1 - 1 - i, b, g, r);
        }
    }
    // 中心白点
    for dy in -2i32..=2 {
        for dx in -2i32..=2 {
            if dx.abs() + dy.abs() <= 2 {
                put(16 + dx, 16 + dy, 255, 255, 255);
            }
        }
    }
    unsafe {
        CreateIcon(None, S, S, 1, 32, and.as_ptr(), xor.as_ptr())
            .map_err(|e| format!("创建图标失败：{e}"))
    }
}

// ---------- 开机自启（HKCU Run 键） ----------

/// 查询开机自启是否开启
pub fn autostart_enabled() -> bool {
    unsafe {
        let sub = util::to_wide(RUN_KEY);
        let val = util::to_wide(RUN_VALUE);
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        ) == ERROR_SUCCESS
    }
}

/// 设置开机自启（写入/删除当前用户 Run 键）
pub fn autostart_set(on: bool) -> Res<()> {
    unsafe {
        let sub = util::to_wide(RUN_KEY);
        let val = util::to_wide(RUN_VALUE);
        if on {
            let exe = std::env::current_exe().map_err(|e| format!("获取 exe 路径失败：{e}"))?;
            let data = format!("\"{}\"", exe.display());
            let w = util::to_wide(&data);
            let r: WIN32_ERROR = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                PCWSTR(val.as_ptr()),
                2, // REG_SZ
                Some(w.as_ptr() as *const c_void),
                (w.len() * 2) as u32,
            );
            if r == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("写入自启注册表失败：错误码 {r:?}"))
            }
        } else {
            let r: WIN32_ERROR =
                RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), PCWSTR(val.as_ptr()));
            if r == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("删除自启注册表失败：错误码 {r:?}"))
            }
        }
    }
}

/// 启动时修正自启路径（exe 被移动后仍能自启）
pub fn autostart_fix_path() {
    if autostart_enabled() {
        // 覆写为当前路径
        let _ = autostart_set(true);
    }
}
