//! 图片编码：PNG/JPEG 走 WIC（系统自带），BMP 手写文件头
//! 剪贴板恒定位图（CF_DIB），本模块只负责保存文件的字节编码

use crate::config::ImgFormat;
use crate::util::{self, Res};
use std::ffi::c_void;
use windows::Win32::Foundation::{HGLOBAL, HANDLE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_ContainerFormatPng,
    GUID_WICPixelFormat32bppBGRA, IWICBitmapFrameEncode, IWICImagingFactory,
    WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::{
    CoCreateInstance, IStream, CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::Com::StructuredStorage::{
    CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2, PROPBAG2,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Variant::VARIANT;
use windows::core::PWSTR;

/// 编码 BGRA 像素为图片字节
pub fn encode(pixels: &[u8], w: i32, h: i32, fmt: ImgFormat) -> Res<Vec<u8>> {
    match fmt {
        ImgFormat::Bmp => Ok(bmp_encode(pixels, w, h)),
        _ => wic_encode(pixels, w, h, fmt),
    }
}

/// WIC 编码（PNG / JPEG）
fn wic_encode(pixels: &[u8], w: i32, h: i32, fmt: ImgFormat) -> Res<Vec<u8>> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("创建 WIC 工厂失败：{e}"))?;
        let container = match fmt {
            ImgFormat::Png => &GUID_ContainerFormatPng,
            _ => &GUID_ContainerFormatJpeg,
        };
        let encoder = factory
            .CreateEncoder(container, std::ptr::null())
            .map_err(|e| format!("创建编码器失败：{e}"))?;
        let stream: IStream =
            CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true)
                .map_err(|e| format!("创建内存流失败：{e}"))?;
        encoder
            .Initialize(&stream, WICBitmapEncoderNoCache)
            .map_err(|e| format!("初始化编码器失败：{e}"))?;

        let mut frame_opt: Option<IWICBitmapFrameEncode> = None;
        let mut props_opt: Option<IPropertyBag2> = None;
        encoder
            .CreateNewFrame(&mut frame_opt, &mut props_opt)
            .map_err(|e| format!("创建帧失败：{e}"))?;
        let frame = frame_opt.ok_or("CreateNewFrame 未返回帧")?;
        let props = props_opt.ok_or("CreateNewFrame 未返回属性袋")?;
        if fmt == ImgFormat::Jpeg {
            // JPEG 质量固定 0.9
            set_jpeg_quality(&props, 0.90)?;
        }
        frame
            .Initialize(&props)
            .map_err(|e| format!("初始化帧失败：{e}"))?;
        frame
            .SetSize(w as u32, h as u32)
            .map_err(|e| format!("SetSize 失败：{e}"))?;
        let bitmap = factory
            .CreateBitmapFromMemory(
                w as u32,
                h as u32,
                &GUID_WICPixelFormat32bppBGRA,
                (w as u32) * 4,
                pixels,
            )
            .map_err(|e| format!("CreateBitmapFromMemory 失败：{e}"))?;
        frame
            .WriteSource(&bitmap, std::ptr::null())
            .map_err(|e| format!("写入像素失败：{e}"))?;
        frame.Commit().map_err(|e| format!("提交帧失败：{e}"))?;
        encoder
            .Commit()
            .map_err(|e| format!("提交编码失败：{e}"))?;

        // 从 HGLOBAL 读回编码结果
        let hglobal: HGLOBAL = GetHGlobalFromStream(&stream)
            .map_err(|e| format!("GetHGlobalFromStream 失败：{e}"))?;
        let size = GlobalSize(hglobal);
        let p = GlobalLock(hglobal);
        if p.is_null() {
            return Err("GlobalLock 失败".into());
        }
        let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        let _ = GlobalUnlock(hglobal);
        Ok(bytes)
    }
}

/// 设置 JPEG 编码质量（WIC 标准属性 ImageQuality）
unsafe fn set_jpeg_quality(bag: &IPropertyBag2, quality: f32) -> Res<()> {
    unsafe {
        let mut name_w = util::to_wide("ImageQuality");
        let mut prop: PROPBAG2 = std::mem::zeroed();
        prop.pstrName = PWSTR(name_w.as_mut_ptr());
        // VARIANT 布局（x64）：vt(2B) + 保留(6B) + 值(8B)
        // VT_R4 = 4，fltVal 位于偏移 8。零值初始化后按偏移写入，避免触碰版本相关的 union 字段名
        let mut var: VARIANT = std::mem::zeroed();
        let vp = std::ptr::from_mut(&mut var);
        *(vp as *mut u16) = 4; // VT_R4
        *((vp as *mut u8).add(8) as *mut f32) = quality;
        bag.Write(1, &prop, vp as *const VARIANT)
            .map_err(|e| format!("设置 JPEG 质量失败：{e}"))?;
        Ok(())
    }
}

/// 手写 BMP 编码（BITMAPFILEHEADER + BITMAPINFOHEADER + 自下而上像素）
fn bmp_encode(pixels: &[u8], w: i32, h: i32) -> Vec<u8> {
    let w = w as usize;
    let h = h as usize;
    let row = w * 4;
    let data_size = row * h;
    let file_size = 14 + 40 + data_size;
    let mut out = Vec::with_capacity(file_size);
    // BITMAPFILEHEADER（14 字节）
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]); // 保留字
    out.extend_from_slice(&54u32.to_le_bytes()); // 像素数据偏移
    // BITMAPINFOHEADER（40 字节，biHeight 取正 = 自下而上）
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as u32).to_le_bytes());
    out.extend_from_slice(&(h as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(data_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // XPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // YPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // ClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // ClrImportant
    // 像素：源为自上而下，BMP 需要自下而上，逐行倒序
    for y in (0..h).rev() {
        out.extend_from_slice(&pixels[y * row..(y + 1) * row]);
    }
    out
}

/// 抑制未使用导入警告（HANDLE 曾用于剪贴板路径，保留给后续重构）
#[allow(dead_code)]
fn _keep(_: HANDLE) {}
#[allow(dead_code)]
fn _keep2(_: *mut c_void) {}
