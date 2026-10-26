//! 配置读写：基于 Win32 原生 ini API
//! 存储位置：exe 同目录 config.ini 优先，不可写时回退 %LOCALAPPDATA%\SimpleShot

use crate::hotkey;
use crate::util::{self};
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Foundation::RECT;
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

/// 解析宽高比文本："free"/空 → None（自由）；"w:h" → Some((w, h))
pub fn parse_aspect(s: &str) -> Option<(i32, i32)> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("free") {
        return None;
    }
    let parts: Vec<&str> = s.split(':').map(|p| p.trim()).collect();
    if parts.len() != 2 {
        return None;
    }
    let w: i32 = parts[0].parse().ok()?;
    let h: i32 = parts[1].parse().ok()?;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// 宽高比序列化为文本
pub fn format_aspect(a: Option<(i32, i32)>) -> String {
    match a {
        None => "free".into(),
        Some((w, h)) => format!("{w}:{h}"),
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
    /// 截图完成后弹气泡提示（错误不受此开关影响）
    pub notify: bool,
    /// 框选宽高比（宽, 高）；None = 自由
    pub aspect: Option<(i32, i32)>,
    /// 固定截取区域（虚拟屏物理像素；None = 未设置）
    pub fixed: Option<RECT>,
    /// 框选热键（修饰键 MOD_* 位组合，不含 MOD_NOREPEAT——注册时统一附加）
    pub hotkey_mods: u32,
    /// 框选热键主键虚拟键码
    pub hotkey_vk: u32,
    /// 固定区域热键（vk 为 0 = 未设置）
    pub hotkey2_mods: u32,
    pub hotkey2_vk: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clipboard: true,
            save: SaveMode::Folder,
            format: ImgFormat::Png,
            notify: true,
            aspect: None,
            fixed: None,
            hotkey_mods: 0,
            // 默认 PrintScreen 键（VK_SNAPSHOT）
            hotkey_vk: 0x2C,
            hotkey2_mods: 0,
            hotkey2_vk: 0,
        }
    }
}

impl Config {
    /// 是否设置了任意截图后动作
    pub fn has_action(&self) -> bool {
        self.clipboard || self.save != SaveMode::None
    }
}

/// 配置文件路径：exe 同目录优先，写不进去时回退 %LOCALAPPDATA%\SimpleShot
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir_writable(dir) {
                return dir.join("config.ini");
            }
        }
    }
    let dir = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("SimpleShot");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.ini")
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".simpleshot-probe");
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

/// 解析固定区域文本 "x1,y1,x2,y2"
pub fn parse_fixed(s: &str) -> Option<RECT> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    if parts.len() != 4 {
        return None;
    }
    let v: Option<Vec<i32>> = parts.iter().map(|p| p.parse().ok()).collect();
    let v = v?;
    if v[2] > v[0] && v[3] > v[1] {
        Some(RECT {
            left: v[0],
            top: v[1],
            right: v[2],
            bottom: v[3],
        })
    } else {
        None
    }
}

/// 固定区域序列化为文本
pub fn format_fixed(rc: RECT) -> String {
    format!("{},{},{},{}", rc.left, rc.top, rc.right, rc.bottom)
}

/// 加载配置，读不到的项保持默认值
pub fn load(path: &Path) -> Config {
    let mut cfg = Config::default();
    let file_w = util::to_wide(&path.to_string_lossy());
    let file = PCWSTR(file_w.as_ptr());
    let sec_actions = util::to_wide("actions");
    let sec_region = util::to_wide("region");
    let sec_hotkey = util::to_wide("hotkey");

    let v = get_str(file, &sec_actions, "clipboard");
    if !v.is_empty() {
        cfg.clipboard = v == "true" || v == "1";
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
    let v = get_str(file, &sec_actions, "notify");
    if !v.is_empty() {
        cfg.notify = v == "true" || v == "1";
    }
    let v = get_str(file, &sec_region, "aspect");
    if !v.is_empty() {
        cfg.aspect = parse_aspect(&v);
    }
    let v = get_str(file, &sec_region, "fixed");
    if let Some(rc) = parse_fixed(&v) {
        cfg.fixed = Some(rc);
    }
    let hk = get_str(file, &sec_hotkey, "key");
    if let Some((mods, vk)) = hotkey::parse_hotkey(&hk) {
        cfg.hotkey_mods = mods;
        cfg.hotkey_vk = vk;
    }
    let hk2 = get_str(file, &sec_hotkey, "key2");
    if let Some((mods, vk)) = hotkey::parse_hotkey(&hk2) {
        cfg.hotkey2_mods = mods;
        cfg.hotkey2_vk = vk;
    }
    cfg
}

/// 保存配置（原子性由 ini API 保证，逐键写入）
pub fn save(cfg: &Config, path: &Path) {
    let file_w = util::to_wide(&path.to_string_lossy());
    let file = PCWSTR(file_w.as_ptr());
    let sec_actions = util::to_wide("actions");
    let sec_region = util::to_wide("region");
    let sec_hotkey = util::to_wide("hotkey");
    let sa = PCWSTR(sec_actions.as_ptr());
    let sr = PCWSTR(sec_region.as_ptr());
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
    put(sa, "notify", if cfg.notify { "true" } else { "false" });
    put(sr, "aspect", &format_aspect(cfg.aspect));
    put(sr, "fixed", &cfg.fixed.map(format_fixed).unwrap_or_default());
    put(
        sh,
        "key",
        &hotkey::format_hotkey(cfg.hotkey_mods, cfg.hotkey_vk),
    );
    put(
        sh,
        "key2",
        &hotkey::format_hotkey(cfg.hotkey2_mods, cfg.hotkey2_vk),
    );
}
