//! 截图后动作：复制到剪贴板 / 保存（对话框或屏幕截图文件夹）+ 结果气泡

use crate::config::{Config, ImgFormat, SaveMode};
use crate::png;
use crate::tray;
use crate::util::{self, Res};
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND, SYSTEMTIME};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{
    FileSaveDialog, IFileSaveDialog, IShellItem, SHGetKnownFolderPath,
    SIGDN_DESKTOPABSOLUTEEDITING, FOLDERID_Pictures, FOLDERID_Screenshots, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;

/// 执行截图后动作并用托盘气泡反馈结果
pub fn perform(hwnd: HWND, cfg: &Config, pixels: Vec<u8>, w: i32, h: i32) {
    let mut ok_msgs: Vec<String> = vec![];
    let mut err_msgs: Vec<String> = vec![];

    if cfg.clipboard {
        match set_clipboard_dib(&pixels, w, h) {
            Ok(()) => ok_msgs.push("已复制到剪贴板".into()),
            Err(e) => err_msgs.push(e),
        }
    }
    match cfg.save {
        SaveMode::None => {}
        SaveMode::Folder => match save_to_folder(cfg, &pixels, w, h) {
            Ok(path) => ok_msgs.push(format!("已保存到 {}", path.display())),
            Err(e) => err_msgs.push(e),
        },
        SaveMode::Ask => match save_via_dialog(hwnd, cfg, &pixels, w, h) {
            Ok(Some(path)) => ok_msgs.push(format!("已保存到 {}", path.display())),
            Ok(None) => {} // 用户取消了保存对话框
            Err(e) => err_msgs.push(e),
        },
    }

    let text = if ok_msgs.is_empty() && err_msgs.is_empty() {
        "未设置任何截图后动作，截图已丢弃".to_string()
    } else {
        let mut text = ok_msgs.join("，");
        if !text.is_empty() && !err_msgs.is_empty() {
            text.push_str("；");
        }
        text.push_str(&err_msgs.join("；"));
        text
    };
    tray::balloon(hwnd, &text);
}

/// 默认文件名主体：Screenshot_2026-10-23_15-30-45-123（毫秒防同秒覆盖）
fn default_file_stem() -> String {
    unsafe {
        let st: SYSTEMTIME = GetLocalTime();
        format!(
            "Screenshot_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}-{:03}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
        )
    }
}

/// 系统「屏幕截图」文件夹，不存在该 known folder 时回退「图片」
fn screenshots_dir() -> Res<PathBuf> {
    unsafe {
        let path: PWSTR = SHGetKnownFolderPath(&FOLDERID_Screenshots, KF_FLAG_DEFAULT, None)
            .or_else(|_| SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None))
            .map_err(|e| format!("获取屏幕截图文件夹失败：{e}"))?;
        let s = util::from_wide_mut(path);
        CoTaskMemFree(Some(path.0 as *const std::ffi::c_void));
        let dir = PathBuf::from(s);
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
        Ok(dir)
    }
}

fn write_image(path: &Path, pixels: &[u8], w: i32, h: i32, fmt: ImgFormat) -> Res<PathBuf> {
    let bytes = png::encode(pixels, w, h, fmt)?;
    std::fs::write(path, bytes).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    Ok(path.to_path_buf())
}

/// 直接保存到屏幕截图文件夹
fn save_to_folder(cfg: &Config, pixels: &[u8], w: i32, h: i32) -> Res<PathBuf> {
    let dir = screenshots_dir()?;
    let path = dir.join(format!("{}.{}", default_file_stem(), cfg.format.ext()));
    write_image(&path, pixels, w, h, cfg.format)
}

/// 弹出保存对话框让用户选择位置；返回 None 表示用户取消
#[allow(clippy::too_many_arguments)]
fn save_via_dialog(hwnd: HWND, cfg: &Config, pixels: &[u8], w: i32, h: i32) -> Res<Option<PathBuf>> {
    let ext = cfg.format.ext();
    let stem = default_file_stem();
    unsafe {
        let dialog: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("创建保存对话框失败：{e}"))?;
        let name_desc = util::to_wide(&format!("{} 图片 (*.{ext})", ext.to_uppercase()));
        let name_spec = util::to_wide(&format!("*.{ext}"));
        let all_desc = util::to_wide("所有文件");
        let all_spec = util::to_wide("*.*");
        let filters = [
            COMDLG_FILTERSPEC {
                pszName: PCWSTR(name_desc.as_ptr()),
                pszSpec: PCWSTR(name_spec.as_ptr()),
            },
            COMDLG_FILTERSPEC {
                pszName: PCWSTR(all_desc.as_ptr()),
                pszSpec: PCWSTR(all_spec.as_ptr()),
            },
        ];
        // SetFileTypes / SetFileName / Show / GetResult 是 IFileDialog trait 的方法
        dialog
            .SetFileTypes(&filters)
            .map_err(|e| format!("设置过滤器失败：{e}"))?;
        let file_w = util::to_wide(&format!("{stem}.{ext}"));
        dialog
            .SetFileName(PCWSTR(file_w.as_ptr()))
            .map_err(|e| format!("设置默认文件名失败：{e}"))?;

        match dialog.Show(Some(hwnd)) {
            Ok(()) => {}
            Err(e) => {
                if e.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                    return Ok(None);
                }
                return Err(format!("保存对话框出错：{e}"));
            }
        }
        let item: IShellItem = dialog.GetResult().map_err(|e| format!("获取保存路径失败：{e}"))?;
        let ppath = item
            .GetDisplayName(SIGDN_DESKTOPABSOLUTEEDITING)
            .map_err(|e| format!("获取保存路径失败：{e}"))?;
        let path = PathBuf::from(util::from_wide_mut(ppath));
        CoTaskMemFree(Some(ppath.0 as *const std::ffi::c_void));
        write_image(&path, pixels, w, h, cfg.format)?;
        Ok(Some(path))
    }
}

/// 把 BGRA 像素以 CF_DIB 格式写入剪贴板
fn set_clipboard_dib(pixels: &[u8], w: i32, h: i32) -> Res<()> {
    unsafe {
        // CF_DIB = BITMAPINFOHEADER + 自下而上像素
        let hdr = BITMAPINFOHEADER {
            biSize: 40,
            biWidth: w,
            biHeight: h, // 正值 = 自下而上
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut data = Vec::with_capacity(40 + pixels.len());
        data.extend_from_slice(&hdr.biSize.to_le_bytes());
        data.extend_from_slice(&hdr.biWidth.to_le_bytes());
        data.extend_from_slice(&hdr.biHeight.to_le_bytes());
        data.extend_from_slice(&hdr.biPlanes.to_le_bytes());
        data.extend_from_slice(&hdr.biBitCount.to_le_bytes());
        data.extend_from_slice(&hdr.biCompression.to_le_bytes());
        data.extend_from_slice(&hdr.biSizeImage.to_le_bytes());
        data.extend_from_slice(&hdr.biXPelsPerMeter.to_le_bytes());
        data.extend_from_slice(&hdr.biYPelsPerMeter.to_le_bytes());
        data.extend_from_slice(&hdr.biClrUsed.to_le_bytes());
        data.extend_from_slice(&hdr.biClrImportant.to_le_bytes());
        let row = w as usize * 4;
        for y in (0..h as usize).rev() {
            data.extend_from_slice(&pixels[y * row..(y + 1) * row]);
        }

        let hg = GlobalAlloc(GMEM_MOVEABLE, data.len())
            .map_err(|e| format!("分配剪贴板内存失败：{e}"))?;
        let p = GlobalLock(hg);
        if p.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err("GlobalLock 失败".into());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
        let _ = GlobalUnlock(hg);

        if OpenClipboard(None).is_err() {
            let _ = GlobalFree(Some(hg));
            return Err("打开剪贴板失败（可能被其他程序占用）".into());
        }
        let _ = EmptyClipboard();
        let placed = SetClipboardData(
            CF_DIB.0 as u32,
            Some(windows::Win32::Foundation::HANDLE(hg.0)),
        );
        let _ = CloseClipboard();
        match placed {
            Ok(_) => Ok(()), // 内存所有权已转移给系统，不能释放
            Err(e) => {
                let _ = GlobalFree(Some(hg));
                Err(format!("写入剪贴板失败：{e}"))
            }
        }
    }
}
