//! 通用辅助：宽字符串转换、统一错误类型

use windows::core::PWSTR;

/// 统一错误类型：携带人类可读消息
pub type Res<T> = Result<T, String>;

/// UTF-8 转 NUL 结尾的 UTF-16
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读取 NUL 结尾的可变宽字符串（如 COM 接口返回的 PWSTR）
///
/// # Safety
/// `p` 必须指向合法的 NUL 结尾 UTF-16 缓冲区
pub unsafe fn from_wide_mut(p: PWSTR) -> String {
    unsafe {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *p.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p.0, len))
    }
}
