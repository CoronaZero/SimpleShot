//! 固定区域手动输入对话框（纯 Win32，无依赖）
//!
//! 模态运行：run() 内部跑局部消息循环（IsDialogMessage 提供 Tab 导航、
//! Esc=取消、Enter=确定），返回用户确认的矩形；取消/关闭返回 None。

use crate::util;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, GetStockObject, HBRUSH, DEFAULT_GUI_FONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
    GetSystemMetrics, GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW, IsWindow, LoadCursorW,
    MessageBoxW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetWindowTextW, ShowWindow,
    TranslateMessage, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, DC_HASDEFID,
    ES_AUTOHSCROLL, IDC_ARROW, IDOK, MB_ICONERROR, MB_OK, SM_CXSCREEN, SM_CYSCREEN,
    SW_SHOW, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_SETFONT, WM_USER,
    WNDCLASSEXW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_BORDER, WS_CAPTION, WS_CHILD, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE, MSG, BN_CLICKED,
};

const CLASS: &str = "SimpleShotInputWnd";

// 控件 ID（IDOK=1/IDCANCEL=2 用系统标准值，IsDialogMessage 的 Esc/Enter 语义直接生效）
const IDC_X1: i32 = 101;
const IDC_Y1: i32 = 102;
const IDC_X2: i32 = 103;
const IDC_Y2: i32 = 104;
/// 对话框返回的结果；None = 用户取消
static RESULT: Mutex<Option<RECT>> = Mutex::new(None);
static DLG_HWND: AtomicIsize = AtomicIsize::new(0);
static WHITE_BRUSH: AtomicIsize = AtomicIsize::new(0);

/// 对话框是否已打开（防重入）
pub fn is_open() -> bool {
    DLG_HWND.load(Ordering::SeqCst) != 0
}

/// 打开模态输入对话框，返回用户确认的固定区域；取消/关闭返回 None
pub fn run(owner: HWND, initial: RECT) -> Option<RECT> {
    unsafe {
        if is_open() {
            return None;
        }
        register_class();
        if let Ok(mut g) = RESULT.lock() {
            *g = None;
        }

        let win_w = 286i32;
        let win_h = 196i32;
        let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - win_h) / 2;
        let name_w = util::to_wide(CLASS);
        let title_w = util::to_wide("设置固定区域");
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(name_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            WS_CAPTION | WS_SYSMENU,
            x,
            y,
            win_w,
            win_h,
            Some(owner),
            None,
            None,
            Some(Box::into_raw(Box::new(initial)) as *const c_void),
        )
        .ok()?;
        DLG_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = SetForegroundWindow(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);

        // 模态循环：对话框销毁后退出；期间线程其他消息正常派发（托盘仍可用）
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let h = HWND(DLG_HWND.load(Ordering::SeqCst) as *mut c_void);
            if !IsWindow(Some(h)).as_bool() {
                // 对话框已销毁：丢弃残消息退出
                break;
            }
            if IsDialogMessageW(h, &mut msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        RESULT.lock().ok()?.take()
    }
}

/// 注册窗口类（进程内一次）
fn register_class() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wname = util::to_wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dlg_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst.into(),
            hIcon: windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(wname.as_ptr()),
            hIconSm: windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut()),
        };
        let _ = RegisterClassExW(&wc);
    }
}

unsafe extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
                let initial = *(cs.lpCreateParams as *const RECT);
                create_controls(hwnd, initial);
                LRESULT(0)
            }
            // IsDialogMessage 的 Enter：询问默认按钮
            WM_USER => {
                // DM_GETDEFID：HIWORD=DC_HASDEFID，LOWORD=控件 ID
                LRESULT(((DC_HASDEFID as isize) << 16) | IDOK.0 as isize)
            }
            WM_CTLCOLORSTATIC => {
                // STATIC 白底（画刷缓存复用）
                let mut b = WHITE_BRUSH.load(Ordering::SeqCst);
                if b == 0 {
                    b = CreateSolidBrush(COLORREF(0x00_FFFFFF)).0 as isize;
                    WHITE_BRUSH.store(b, Ordering::SeqCst);
                }
                LRESULT(b)
            }
            WM_COMMAND => {
                let (id, code) = ((wparam.0 & 0xFFFF) as i32, (wparam.0 >> 16) as i32);
                if code == BN_CLICKED as i32 {
                    if id == IDOK.0 {
                        match read_rect(hwnd) {
                            Ok(rc) => {
                                if let Ok(mut g) = RESULT.lock() {
                                    *g = Some(rc);
                                }
                                let _ = DestroyWindow(hwnd);
                            }
                            Err(e) => {
                                let w = util::to_wide(&e);
                                let t = util::to_wide("SimpleShot");
                                let _ = MessageBoxW(
                                    Some(hwnd),
                                    PCWSTR(w.as_ptr()),
                                    PCWSTR(t.as_ptr()),
                                    MB_OK | MB_ICONERROR,
                                );
                            }
                        }
                    } else if id == 2 {
                        // IDCANCEL（Esc / 取消按钮）
                        let _ = DestroyWindow(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                DLG_HWND.store(0, Ordering::SeqCst);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 创建子控件并预填初始值
unsafe fn create_controls(hwnd: HWND, initial: RECT) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let mk = |class: &str, text: &str, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE,
                      id: i32, x: i32, y: i32, w: i32, h: i32| -> HWND {
            let cls = util::to_wide(class);
            let txt = util::to_wide(text);
            let child = CreateWindowExW(
                ex,
                PCWSTR(cls.as_ptr()),
                PCWSTR(txt.as_ptr()),
                style,
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut c_void)),
                Some(hinst.into()),
                None,
            )
            .unwrap_or_default();
            let _ = SendMessageW(
                child,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
            child
        };

        let es = WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        );
        let ss = WS_CHILD | WS_VISIBLE;
        let bs_ok = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32);
        let bs_cancel = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32);

        mk("STATIC", "左上 X", ss, WINDOW_EX_STYLE(0), 0, 12, 15, 52, 16);
        mk("STATIC", "Y", ss, WINDOW_EX_STYLE(0), 0, 152, 15, 16, 16);
        mk("STATIC", "右下 X", ss, WINDOW_EX_STYLE(0), 0, 12, 47, 52, 16);
        mk("STATIC", "Y", ss, WINDOW_EX_STYLE(0), 0, 152, 47, 16, 16);

        let e_x1 = mk("EDIT", "", es, WINDOW_EX_STYLE(0), IDC_X1, 70, 12, 64, 22);
        let e_y1 = mk("EDIT", "", es, WINDOW_EX_STYLE(0), IDC_Y1, 174, 12, 64, 22);
        let e_x2 = mk("EDIT", "", es, WINDOW_EX_STYLE(0), IDC_X2, 70, 44, 64, 22);
        let e_y2 = mk("EDIT", "", es, WINDOW_EX_STYLE(0), IDC_Y2, 174, 44, 64, 22);

        mk("BUTTON", "确定", bs_ok, WINDOW_EX_STYLE(0), IDOK.0, 66, 84, 70, 26);
        mk("BUTTON", "取消", bs_cancel, WINDOW_EX_STYLE(0), 2, 152, 84, 70, 26);
        mk(
            "STATIC",
            "坐标为虚拟屏物理像素，右下角须大于左上角",
            ss,
            WINDOW_EX_STYLE(0),
            0,
            12,
            120,
            262,
            16,
        );

        // 预填初始值
        set_edit_text(e_x1, &initial.left.to_string());
        set_edit_text(e_y1, &initial.top.to_string());
        set_edit_text(e_x2, &initial.right.to_string());
        set_edit_text(e_y2, &initial.bottom.to_string());
    }
}

unsafe fn set_edit_text(hwnd: HWND, text: &str) {
    unsafe {
        let w = util::to_wide(text);
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

/// 读取四个输入框并校验
unsafe fn read_rect(hwnd: HWND) -> Result<RECT, String> {
    unsafe {
        let x1 = read_edit(hwnd, IDC_X1);
        let y1 = read_edit(hwnd, IDC_Y1);
        let x2 = read_edit(hwnd, IDC_X2);
        let y2 = read_edit(hwnd, IDC_Y2);
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) else {
            return Err("请输入四个整数坐标".into());
        };
        if x2 <= x1 || y2 <= y1 {
            return Err("坐标无效：右下角必须大于左上角".into());
        }
        Ok(RECT {
            left: x1,
            top: y1,
            right: x2,
            bottom: y2,
        })
    }
}

unsafe fn read_edit(hwnd_dlg: HWND, id: i32) -> Option<i32> {
    unsafe {
        let h = GetDlgItem(Some(hwnd_dlg), id).ok()?;
        let len = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; (len + 1) as usize];
        GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf).trim().parse().ok()
    }
}
