//! 全屏遮罩选区窗口：变暗遮罩 + 矩形拖拽 + 窗口悬停捕获（混合模式）
//!
//! 交互状态机：
//!   悬停：智能高亮鼠标下顶层窗口（描边+提亮+标题），可继续移动
//!   单击：捕获按下时刻快照的窗口边界
//!   按下并拖动超阈值：转入矩形选取，松开完成
//!   Esc / 右键：任意时刻取消
//!
//! 绘制注意：整屏截图 DIB 在 capture 阶段已选入其 mem_dc，
//! GDI 不允许把同一 DIB section 二次 SelectObject 到其他 DC，
//! 因此底图用 StretchDIBits 直接从像素内存绘制。

use crate::capture::Dib;
use crate::util::{self, Res};
use std::ffi::c_void;
use std::sync::Mutex;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CombineRgn, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateDIBSection, CreatePen, CreateRectRgn, CreateSolidBrush,
    DEFAULT_GUI_FONT, DT_CALCRECT, DT_SINGLELINE, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FillRect, GetStockObject, HBRUSH, HDC, HGDIOBJ, InvalidateRect,
    NULL_BRUSH, PAINTSTRUCT, PS_SOLID, Rectangle, SelectClipRgn, SelectObject, SetBkMode,
    SetTextColor, SRCCOPY, StretchDIBits, TRANSPARENT, AC_SRC_ALPHA, AC_SRC_OVER,
    BLENDFUNCTION, HBITMAP, RGN_DIFF,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GetClassNameW, GetCursorPos,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HICON, IDC_ARROW, IsIconic, IsWindowVisible, LoadCursorW,
    PostMessageW, RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW, WNDCLASSEXW, WM_APP,
    WM_CAPTURECHANGED, WM_DESTROY, WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN, WS_EX_TOPMOST,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
};

/// 主窗口消息：选区完成（选区 RECT 存于 REGION，DIB 本地坐标）
pub const M_REGION_DONE: u32 = WM_APP + 3;
/// 主窗口消息：选区取消
pub const M_REGION_CANCEL: u32 = WM_APP + 6;
/// 选区结果传递（跨窗口过程共享）
pub static REGION: Mutex<Option<RECT>> = Mutex::new(None);

const CLASS: &str = "ScreenShotOverlayWnd";

/// 拖拽判定阈值（物理像素）
const DRAG_THRESHOLD: i32 = 4;

/// 遮罩窗口状态
struct OverlayState {
    app_hwnd: HWND,
    /// 整屏截图像素内存（指向 App 的 Dib，窗口存活期间由 App 持有）
    screen_px: *mut u8,
    /// 1x1 半透明黑位图（premultiplied alpha=128）
    dim: HBITMAP,
    dim_dc: HDC,
    /// 窗口尺寸（= 虚拟屏物理尺寸）
    w: i32,
    h: i32,
    /// 虚拟屏原点
    vx: i32,
    vy: i32,
    pressed: bool,
    dragging: bool,
    start: POINT,
    cur: POINT,
    /// 悬停窗口（边界 + 标题）
    hover: Option<(RECT, String)>,
    /// 按下瞬间快照的窗口边界
    pending_win: Option<RECT>,
    /// 锁定的宽高比 (宽, 高)；自由比例时为 None
    aspect: Option<(i32, i32)>,
}

impl Drop for OverlayState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dim_dc);
            let _ = DeleteObject(HGDIOBJ(self.dim.0));
        }
        // screen_px 由 App 的 Dib 持有，不在此释放
    }
}

/// 注册遮罩窗口类（进程内一次）
pub fn register_class() -> Res<()> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wname = util::to_wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst.into(),
            hIcon: HICON(std::ptr::null_mut()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(wname.as_ptr()),
            hIconSm: HICON(std::ptr::null_mut()),
        };
        let _ = RegisterClassExW(&wc);
    }
    Ok(())
}

/// 创建覆盖整个虚拟桌面的遮罩窗口（全屏置顶）
pub fn create(app_hwnd: HWND, dib: &Dib, aspect: Option<(i32, i32)>) -> Res<HWND> {
    let (dim, dim_dc) = make_dim()?;
    let mut cur = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cur);
    }
    let st = Box::new(OverlayState {
        app_hwnd,
        screen_px: dib.pixels,
        dim,
        dim_dc,
        w: dib.width,
        h: dib.height,
        vx: dib.vx,
        vy: dib.vy,
        pressed: false,
        dragging: false,
        start: cur,
        cur,
        hover: None,
        pending_win: None,
        aspect,
    });
    let name_w = util::to_wide(CLASS);
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(name_w.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            dib.vx,
            dib.vy,
            dib.width,
            dib.height,
            None,
            None,
            None,
            Some(Box::into_raw(st) as *const c_void),
        )
        .map_err(|e| format!("创建遮罩窗口失败：{e}"))?;
        // 尽力前置以获取键盘焦点（Esc 取消需要焦点）
        let _ = SetForegroundWindow(hwnd);
        Ok(hwnd)
    }
}

/// 1x1 32bpp 全黑 premultiplied 位图，alpha=128（50% 变暗）
fn make_dim() -> Res<(HBITMAP, HDC)> {
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = 1;
        bmi.bmiHeader.biHeight = -1;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        let mut px: *mut c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut px, None, 0)
            .map_err(|e| format!("创建遮罩位图失败：{e}"))?;
        *(px as *mut u32) = 0x8000_0000; // BGRA: 黑，alpha=128
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            return Err("CreateCompatibleDC 失败".into());
        }
        SelectObject(dc, HGDIOBJ(hbmp.0));
        Ok((hbmp, dc))
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let st_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if st_ptr == 0 {
            if msg == WM_NCCREATE {
                // 安装窗口状态
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let st = &mut *(st_ptr as *mut OverlayState);

        match msg {
            WM_ERASEBKGND => LRESULT(1), // 自绘，消除闪烁
            WM_PAINT => {
                paint(hwnd, st);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                on_move(hwnd, st);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                on_down(hwnd, st);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                on_up(hwnd, st);
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                // 右键取消
                finish_cancel(hwnd, st);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
                finish_cancel(hwnd, st);
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // 捕获被系统剥夺（安全网）
                st.pressed = false;
                st.dragging = false;
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                // 屏幕分辨率/拓扑变化（云电脑 RDP 动态分辨率常见）：
                // 遮罩的屏幕快照已过时，取消本次截图，用户重新触发即按新分辨率截取
                finish_cancel(hwnd, st);
                LRESULT(0)
            }
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(st));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn on_move(hwnd: HWND, st: &mut OverlayState) {
    unsafe {
        let _ = GetCursorPos(&mut st.cur);
    }
    if st.pressed {
        // 按住移动：超过阈值判定为矩形拖拽
        if !st.dragging
            && (st.cur.x - st.start.x).abs() + (st.cur.y - st.start.y).abs() > DRAG_THRESHOLD
        {
            st.dragging = true;
            st.pending_win = None;
            st.hover = None;
        }
    } else {
        // 悬停识别（锁定宽高比时禁用窗口捕获，纯矩形拖拽）
        st.hover = if st.aspect.is_some() {
            None
        } else {
            find_window_at(st.cur)
        };
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn on_down(hwnd: HWND, st: &mut OverlayState) {
    unsafe {
        let _ = GetCursorPos(&mut st.start);
    }
    st.cur = st.start;
    st.pressed = true;
    st.dragging = false;
    // 按下瞬间快照悬停窗口（若有效），避免拖动中窗口动画导致错位；锁定比例时无窗口捕获
    st.pending_win = if st.aspect.is_some() {
        None
    } else {
        st.hover.as_ref().map(|(rc, _)| *rc)
    };
    unsafe {
        let _ = SetCapture(hwnd);
    }
}

fn on_up(hwnd: HWND, st: &mut OverlayState) {
    // 先快照判定结果，再释放捕获（释放会触发 WM_CAPTURECHANGED 清状态）
    let dragging = st.dragging;
    let pending = st.pending_win;
    if dragging {
        // 比例锁定在 drag_rect 内统一应用
        let rc = drag_rect(st);
        if rc.right > rc.left && rc.bottom > rc.top {
            finish_rect(hwnd, st, rc);
        } else {
            finish_cancel(hwnd, st);
        }
    } else if let Some(rc) = pending {
        finish_rect(hwnd, st, rc);
    } else {
        // 未拖拽且无有效窗口：静默取消
        finish_cancel(hwnd, st);
    }
    unsafe {
        let _ = ReleaseCapture();
    }
}

/// 拖拽选区矩形：以按下点为固定角，向鼠标方向展开；
/// 锁定宽高比时矩形在按下点→鼠标点范围内按比例最大化（不越过鼠标点）
fn drag_rect(st: &OverlayState) -> RECT {
    let (a, b) = (st.start, st.cur);
    let dx = (b.x - a.x).abs();
    let dy = (b.y - a.y).abs();
    let (w, h) = if let Some((rw, rh)) = st.aspect {
        // 候选宽 = |dy| * rw / rh（由高驱动）；取两驱动中的较小约束，等比例缩到框内
        let cand_w = dy * rw / rh;
        let w = dx.min(cand_w);
        (w, w * rh / rw)
    } else {
        (dx, dy)
    };
    let (x1, x2) = if b.x >= a.x {
        (a.x, a.x + w)
    } else {
        (a.x - w, a.x)
    };
    let (y1, y2) = if b.y >= a.y {
        (a.y, a.y + h)
    } else {
        (a.y - h, a.y)
    };
    RECT {
        left: x1,
        top: y1,
        right: x2,
        bottom: y2,
    }
}

/// 完成：把选区（虚拟屏坐标）转 DIB 本地坐标后通知主窗口并销毁自己
fn finish_rect(hwnd: HWND, st: &OverlayState, rc_virt: RECT) {
    let rc = RECT {
        left: rc_virt.left - st.vx,
        top: rc_virt.top - st.vy,
        right: rc_virt.right - st.vx,
        bottom: rc_virt.bottom - st.vy,
    };
    if let Ok(mut g) = REGION.lock() {
        *g = Some(rc);
    }
    unsafe {
        let _ = PostMessageW(Some(st.app_hwnd), M_REGION_DONE, WPARAM(0), LPARAM(0));
        let _ = DestroyWindow(hwnd);
    }
}

fn finish_cancel(hwnd: HWND, st: &OverlayState) {
    unsafe {
        let _ = PostMessageW(Some(st.app_hwnd), M_REGION_CANCEL, WPARAM(0), LPARAM(0));
        let _ = DestroyWindow(hwnd);
    }
}

// ---------- 悬停窗口识别 ----------

struct FindCtx {
    pt: POINT,
    my_pid: u32,
    found: Option<(RECT, String)>,
}

/// 找到光标下最上层有效顶层窗口（EnumWindows 按 z 序自顶向下枚举）
fn find_window_at(pt: POINT) -> Option<(RECT, String)> {
    unsafe {
        let mut ctx = FindCtx {
            pt,
            my_pid: std::process::id(),
            found: None,
        };
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut ctx as *mut _ as isize));
        ctx.found
    }
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let ctx = &mut *(lparam.0 as *mut FindCtx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        if IsIconic(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.my_pid {
            return BOOL(1);
        }
        // 排除桌面壳层与任务栏
        let mut cls = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut cls);
        let cls_name = String::from_utf16_lossy(&cls[..n as usize]);
        match cls_name.as_str() {
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "SHELLDLL_DefView" => {
                return BOOL(1)
            }
            _ => {}
        }
        // 排除挂起的 UWP 窗口（cloak 状态）
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            4,
        )
        .is_ok()
            && cloaked != 0
        {
            return BOOL(1);
        }
        // 边界：优先 DWM 视觉边界（含标题栏、不含阴影）
        let mut rc = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rc as *mut RECT as *mut c_void,
            16,
        )
        .is_err()
        {
            let _ = GetWindowRect(hwnd, &mut rc);
        }
        if ctx.pt.x < rc.left || ctx.pt.x >= rc.right || ctx.pt.y < rc.top || ctx.pt.y >= rc.bottom {
            return BOOL(1);
        }
        // 窗口标题
        let len = GetWindowTextLengthW(hwnd);
        let mut title_buf = vec![0u16; (len + 1) as usize];
        GetWindowTextW(hwnd, &mut title_buf);
        let title = String::from_utf16_lossy(&title_buf[..len as usize]);
        ctx.found = Some((rc, title));
        BOOL(0) // 命中最上层窗口即停止
    }
}

// ---------- 绘制 ----------

/// 当前提亮区域：拖拽矩形 / 按下快照窗口 / 悬停窗口
/// 标签：拖拽显示「起点坐标 · 宽 × 高」；悬停显示「标题 · 鼠标坐标」
fn current_selection(st: &OverlayState) -> Option<(RECT, String)> {
    if st.dragging {
        let rc = drag_rect(st);
        let label = format!(
            "({}, {}) · {} × {}",
            st.start.x,
            st.start.y,
            rc.right - rc.left,
            rc.bottom - rc.top
        );
        Some((rc, label))
    } else if st.pressed {
        st.pending_win.map(|rc| {
            let label = st
                .hover
                .as_ref()
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            (rc, label)
        })
    } else {
        st.hover.as_ref().map(|(rc, title)| {
            let label = format!("{} · ({}, {})",
                if title.is_empty() { "窗口" } else { title.as_str() },
                st.cur.x,
                st.cur.y
            );
            (*rc, label)
        })
    }
}

fn paint(hwnd: HWND, st: &mut OverlayState) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        if dc.is_invalid() {
            return;
        }
        // 双缓冲
        let mem = CreateCompatibleDC(Some(dc));
        if mem.is_invalid() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let bmp = CreateCompatibleBitmap(dc, st.w, st.h);
        let old_bmp = SelectObject(mem, HGDIOBJ(bmp.0));

        // 1. 底图：整屏截图
        //    注意：截图 DIB 已选入 capture 阶段的 mem_dc，GDI 不允许二次 SelectObject，
        //    因此用 StretchDIBits 直接从像素内存绘制（实测可靠）
        {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = st.w;
            bmi.bmiHeader.biHeight = -st.h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            StretchDIBits(
                mem,
                0,
                0,
                st.w,
                st.h,
                0,
                0,
                st.w,
                st.h,
                Some(st.screen_px as *const c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        // 2. 提亮区域之外的屏幕变暗
        let sel = current_selection(st).map(|(rc, label)| (to_local(st, rc), label));
        if let Some((rc, label)) = &sel {
            let full = CreateRectRgn(0, 0, st.w, st.h);
            let selr = CreateRectRgn(rc.left, rc.top, rc.right, rc.bottom);
            let diff = CreateRectRgn(0, 0, 0, 0);
            let _ = CombineRgn(Some(diff), Some(full), Some(selr), RGN_DIFF);
            let _ = SelectClipRgn(mem, Some(diff));
            let _ = AlphaBlend(mem, 0, 0, st.w, st.h, st.dim_dc, 0, 0, 1, 1, blend);
            let _ = SelectClipRgn(mem, None);
            let _ = DeleteObject(HGDIOBJ(full.0));
            let _ = DeleteObject(HGDIOBJ(selr.0));
            let _ = DeleteObject(HGDIOBJ(diff.0));

            // 3. 高亮边框
            let pen = CreatePen(PS_SOLID, 2, COLORREF(0x00_FFFFFF));
            let old_pen = SelectObject(mem, HGDIOBJ(pen.0));
            let old_brush = SelectObject(mem, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(mem, rc.left, rc.top, rc.right, rc.bottom);
            SelectObject(mem, old_pen);
            SelectObject(mem, old_brush);
            let _ = DeleteObject(HGDIOBJ(pen.0));

            // 4. 标签（拖拽显示尺寸 / 悬停显示窗口标题）
            draw_label(mem, st, rc, label);
        } else {
            // 无选区：全屏整体变暗
            let _ = AlphaBlend(mem, 0, 0, st.w, st.h, st.dim_dc, 0, 0, 1, 1, blend);
            // 悬停空白桌面且未按下：鼠标旁显示当前坐标（便于记录位置设固定区域）
            if !st.pressed && st.hover.is_none() {
                let label = format!("({}, {})", st.cur.x, st.cur.y);
                let mx = st.cur.x - st.vx + 14;
                let my = st.cur.y - st.vy + 18;
                draw_tag(mem, st, mx, my, &label);
            }
        }

        let _ = BitBlt(dc, 0, 0, st.w, st.h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 虚拟屏坐标 -> 窗口本地坐标
fn to_local(st: &OverlayState, rc: RECT) -> RECT {
    RECT {
        left: rc.left - st.vx,
        top: rc.top - st.vy,
        right: rc.right - st.vx,
        bottom: rc.bottom - st.vy,
    }
}

/// 小标签：深色底 + 白字，置于选区下方（越界时放上方）
unsafe fn draw_label(dc: HDC, st: &OverlayState, rc: &RECT, label: &str) {
    unsafe {
        if label.is_empty() {
            return;
        }
        let mut text = util::to_wide(label);
        let mut brc = RECT::default();
        // 测量文本尺寸
        DrawTextW(dc, &mut text, &mut brc, DT_CALCRECT | DT_SINGLELINE);
        let tw = (brc.right - brc.left).max(0);
        let th = (brc.bottom - brc.top).max(0);
        let pad = 4;
        let mut x = rc.left;
        let mut y = rc.bottom + 6;
        if y + th + pad * 2 > st.h {
            y = rc.top - th - pad * 2 - 6;
        }
        if y < 0 {
            y = 0;
        }
        if x + tw + pad * 2 > st.w {
            x = st.w - tw - pad * 2;
        }
        if x < 0 {
            x = 0;
        }
        draw_tag_at(dc, x, y, &mut text, tw, th);
    }
}

/// 在指定位置画坐标小标签（自动在屏幕边缘内收拢）
unsafe fn draw_tag(dc: HDC, st: &OverlayState, x: i32, y: i32, label: &str) {
    unsafe {
        let mut text = util::to_wide(label);
        let mut brc = RECT::default();
        DrawTextW(dc, &mut text, &mut brc, DT_CALCRECT | DT_SINGLELINE);
        let tw = (brc.right - brc.left).max(0);
        let th = (brc.bottom - brc.top).max(0);
        let x = (x.max(0)).min(st.w - tw - 8);
        let y = (y.max(0)).min(st.h - th - 8);
        draw_tag_at(dc, x, y, &mut text, tw, th);
    }
}

/// 标签绘制核心：深色底 + 白字
unsafe fn draw_tag_at(dc: HDC, x: i32, y: i32, text: &mut [u16], tw: i32, th: i32) {
    unsafe {
        let pad = 4;
        let frc = RECT {
            left: x,
            top: y,
            right: x + tw + pad * 2,
            bottom: y + th + pad * 2,
        };
        let brush = CreateSolidBrush(COLORREF(0x00_E0_20_20));
        FillRect(dc, &frc, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));

        let old_font = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        let _ = SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00_FFFFFF));
        let mut trc = RECT {
            left: x + pad,
            top: y + pad,
            right: x + pad + tw,
            bottom: y + pad + th,
        };
        DrawTextW(dc, text, &mut trc, DT_SINGLELINE);
        SelectObject(dc, old_font);
    }
}
