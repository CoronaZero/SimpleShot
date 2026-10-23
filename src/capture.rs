//! GDI 截屏：逐显示器捕获并拼接为整张虚拟桌面位图（32bpp BGRA，物理像素）
//!
//! 进程以 per-monitor v2 DPI aware 运行，因此所有屏幕坐标即物理像素，
//! 虚拟屏原点 (SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN) 即拼接位图的原点。

use crate::util::Res;
use std::ffi::c_void;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDCW, CreateDIBSection, DeleteDC, DeleteObject,
    DIB_RGB_COLORS, EnumDisplayMonitors, GetDeviceCaps, GetMonitorInfoW, HDC, HBITMAP, HGDIOBJ,
    HMONITOR, HORZRES, MONITORINFO, MONITORINFOEXW, SelectObject, SRCCOPY, VERTRES, BITMAPINFO,
    BITMAPINFOHEADER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// 自管理的整屏 DIB（32bpp BGRA，自上而下行序）
pub struct Dib {
    pub hbmp: HBITMAP,
    pub mem_dc: HDC,
    pub width: i32,
    pub height: i32,
    /// 像素首地址（BGRA 顺序，行 stride = width * 4）
    pub pixels: *mut u8,
    /// 虚拟屏原点（物理像素）
    pub vx: i32,
    pub vy: i32,
}

// 裸指针成员来自 GDI 分配的 DIB，跨线程不使用，这里仅让 Box 装载合法
unsafe impl Send for Dib {}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(HGDIOBJ(self.hbmp.0));
        }
    }
}

impl Dib {
    /// 创建 DIB 并选入兼容内存 DC
    fn new(width: i32, height: i32, vx: i32, vy: i32) -> Res<Self> {
        unsafe {
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // 负值 = 自上而下行序
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default()],
            };
            let mut px: *mut c_void = std::ptr::null_mut();
            let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut px, None, 0)
                .map_err(|e| format!("CreateDIBSection 失败：{e}"))?;
            let mem_dc = CreateCompatibleDC(None);
            if mem_dc.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
                return Err("CreateCompatibleDC 失败".into());
            }
            SelectObject(mem_dc, HGDIOBJ(hbmp.0));
            Ok(Self {
                hbmp,
                mem_dc,
                width,
                height,
                pixels: px as *mut u8,
                vx,
                vy,
            })
        }
    }
}

/// 捕获整个虚拟桌面（所有显示器，物理像素）
pub fn capture_virtual_screen() -> Res<Dib> {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if vw <= 0 || vh <= 0 {
            return Err("虚拟屏幕尺寸无效".into());
        }
        let dib = Dib::new(vw, vh, vx, vy)?;
        let ctx = EnumCtx { dib: &dib };
        let _ =
            EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(&ctx as *const _ as isize));
        Ok(dib)
    }
}

struct EnumCtx<'a> {
    dib: &'a Dib,
}

/// 显示器枚举回调：把每屏物理像素 BitBlt 到拼接位图的对应区域
unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    unsafe {
        let ctx = &*(lparam.0 as *const EnumCtx);
        let dib = ctx.dib;
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
            return BOOL(1);
        }
        let rc = info.monitorInfo.rcMonitor; // 该屏在虚拟屏中的物理像素区域
        let name = info.szDevice;
        // CreateDC 直接得到该屏的物理像素 DC
        let dc = CreateDCW(PCWSTR(name.as_ptr()), None, None, None);
        if dc.is_invalid() {
            return BOOL(1);
        }
        // 防御：监视器矩形与 DC 实际尺寸不一致时（DPI 过渡态/分辨率变化瞬间），
        // 以 DC 自身尺寸为上限 clamp，避免拷贝超出源 DC 范围
        let w = (rc.right - rc.left).min(GetDeviceCaps(Some(dc), HORZRES));
        let h = (rc.bottom - rc.top).min(GetDeviceCaps(Some(dc), VERTRES));
        let _ = BitBlt(
            dib.mem_dc,
            rc.left - dib.vx,
            rc.top - dib.vy,
            w,
            h,
            Some(dc),
            0,
            0,
            SRCCOPY,
        );
        let _ = DeleteDC(dc);
        BOOL(1)
    }
}

/// 从整屏 DIB 裁剪子矩形（入参为 DIB 本地坐标，即减去虚拟屏原点后的坐标）
///
/// 返回 (像素 BGRA, 宽, 高)；同时把 alpha 通道统一置 255，
/// 消除 GDI BitBlt 留下的未定义 alpha 值（否则 PNG 保存时会变透明）。
pub fn crop(dib: &Dib, rc: RECT) -> Option<(Vec<u8>, i32, i32)> {
    let x0 = rc.left.max(0).min(dib.width);
    let y0 = rc.top.max(0).min(dib.height);
    let x1 = rc.right.max(0).min(dib.width);
    let y1 = rc.bottom.max(0).min(dib.height);
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0 || h <= 0 {
        return None;
    }
    let stride = dib.width as usize * 4;
    let row_bytes = w as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * h as usize);
    unsafe {
        for y in y0..y1 {
            let row = dib.pixels.add(y as usize * stride + x0 as usize * 4);
            let mut line = std::slice::from_raw_parts(row, row_bytes).to_vec();
            for px in line.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
            out.append(&mut line);
        }
    }
    Some((out, w, h))
}
