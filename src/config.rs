//! 配置读写：基于 Win32 原生 ini API
//! 存储位置：exe 同目录 config.ini 优先，不可写时回退 %LOCALAPPDATA%\Screenshot-rs

use crate::hotkey;
use crate::util::{self, Res};
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::System::WindowsProgramming::{
    GetPrivateProfileStringW, WritePrivateProfileStringW,
};

/// 截图后的保存模式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveMode {
    /// 不保存
    None,
    /// 弹出保存对话框
    Ask,
    /// 直接保存到屏幕截图文件夹
    Folder,
}

/// 图片保存格式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImgFormat {
    Png,
    Jpeg,
    Bmp,
}

impl ImgFormat {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Bmp => "bmp",
        }
    }
}

/// 运行配置
#[derive(Clone, Copy)]
pub struct Config {
    /// 截图后复制到剪贴板
    pub clipboard: bool,
    /// 保存模式
    pub save: SaveMode,
    /// 保存格式
    pub format: ImgFormat,
    /// 全局热键（修饰键 MOD_* 位组合，不含 MOD_NOREPEAT——注册时统一附加）
    pub hotkey_mods: u32,
    /// 热键主键虚拟键码
    pub hotkey_vk: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clipboard: true,
            save: SaveMode::Folder,
            format: ImgFormat::Png,
            hotkey_mods: 0,
            // 默认 PrintScreen 键（VK_SNAPSHOT）
            hotkey_vk: 0x2C,
        }
    }
}

impl Config {
    /// 是否设置了任意截图后动作
    pub fn has_action(&self) -> bool {
        self.clipboard || self.save != SaveMode::None
    }
}

/// 配置文件路径：exe 同目录优先，写不进去时回退 %LOCALAPPDATA%\Screenshot-rs
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir_writable(dir) {
                return dir.join("config.ini");
            }
        }
    }
    let dir = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join("Screenshot-rs");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.ini")
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".screenshot-rs-probe");
    match std::fs::OpenOptions::new().create(true).append(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 读取一个字符串配置项（读不到返回空串）
fn get_str(file: PCWSTR, sec: &[u16], key: &str) -> String {
    let key_w = util::to_wide(key);
    let mut buf = [0u16; 256];
    let n = unsafe {
        GetPrivateProfileStringW(
            PCWSTR(sec.as_ptr()),
            PCWSTR(key_w.as_ptr()),
            PCWSTR::null(),
            Some(&mut buf),
            file,
        )
    };
    String::from_utf16_lossy(&buf[..n as usize])
}

/// 加载配置，读不到的项保持默认值
pub fn load(path: &Path) -> Config {
    let mut cfg = Config::default();
    let file_w = util::to_wide(&path.to_string_lossy());
    let file = PCWSTR(file_w.as_ptr());
    let sec_actions = util::to_wide("actions");
    let sec_hotkey = util::to_wide("hotkey");

    let cb = get_str(file, &sec_actions, "clipboard");
    if !cb.is_empty() {
        cfg.clipboard = cb == "true" || cb == "1";
    }
    match get_str(file, &sec_actions, "save").as_str() {
        "none" => cfg.save = SaveMode::None,
        "ask" => cfg.save = SaveMode::Ask,
        "folder" => cfg.save = SaveMode::Folder,
        _ => {}
    }
    match get_str(file, &sec_actions, "format").as_str() {
        "png" => cfg.format = ImgFormat::Png,
        "jpeg" | "jpg" => cfg.format = ImgFormat::Jpeg,
        "bmp" => cfg.format = ImgFormat::Bmp,
        _ => {}
    }
    let hk = get_str(file, &sec_hotkey, "key");
    if let Some((mods, vk)) = hotkey::parse_hotkey(&hk) {
        cfg.hotkey_mods = mods;
        cfg.hotkey_vk = vk;
    }
    cfg
}

/// 保存配置（原子性由 ini API 保证，逐键写入）
pub fn save(cfg: &Config, path: &Path) {
    let file_w = util::to_wide(&path.to_string_lossy());
    let file = PCWSTR(file_w.as_ptr());
    let sec_actions = util::to_wide("actions");
    let sec_hotkey = util::to_wide("hotkey");
    let sa = PCWSTR(sec_actions.as_ptr());
    let sh = PCWSTR(sec_hotkey.as_ptr());

    let put = |sec: PCWSTR, key: &str, val: &str| {
        let key_w = util::to_wide(key);
        let val_w = util::to_wide(val);
        unsafe {
            let _ = WritePrivateProfileStringW(
                sec,
                PCWSTR(key_w.as_ptr()),
                PCWSTR(val_w.as_ptr()),
                file,
            );
        }
    };

    put(sa, "clipboard", if cfg.clipboard { "true" } else { "false" });
    put(
        sa,
        "save",
        match cfg.save {
            SaveMode::None => "none",
            SaveMode::Ask => "ask",
            SaveMode::Folder => "folder",
        },
    );
    put(
        sa,
        "format",
        match cfg.format {
            ImgFormat::Png => "png",
            ImgFormat::Jpeg => "jpeg",
            ImgFormat::Bmp => "bmp",
        },
    );
    put(
        sh,
        "key",
        &hotkey::format_hotkey(cfg.hotkey_mods, cfg.hotkey_vk),
    );
}

/// 取配置文件路径（供诊断）
#[allow(dead_code)]
pub fn probe() -> Res<PathBuf> {
    Ok(config_path())
}
