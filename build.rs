//! 构建脚本：
//! 1. 嵌入 DPI manifest（Server 2016/RDP 下运行时 API 不可靠，manifest 加载时生效）
//! 2. 生成与托盘同图案的取景框图标（16/32/48 三尺寸），经 rc.exe 编译为
//!    .res 嵌入 exe —— exe 文件图标与托盘/任务栏图标同源一致
//!
//! 注意：draw_icon_frame 的图案与 src/tray.rs 的 draw_icon_runtime 保持一致，
//! 修改任一处需同步另一处。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // ---------- 1. DPI manifest 嵌入 ----------
    let manifest = manifest_dir.join("app.manifest");
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());

    // ---------- 2. 图标资源嵌入 ----------
    let ico_path = out_dir.join("icon.ico");
    fs::write(&ico_path, build_icon_ico()).expect("生成 icon.ico 失败");

    if let Some(rc) = find_rc_exe() {
        let rc_path = out_dir.join("icon.rc");
        // 资源 ID = 1：既是 exe 文件图标（首个 RT_GROUP_ICON），也被 tray.rs 用 LoadImage 加载
        fs::write(&rc_path, "1 ICON \"icon.ico\"\r\n").expect("写 icon.rc 失败");
        let res_path = out_dir.join("icon.res");
        let ok = Command::new(&rc)
            .arg("/nologo")
            .arg(format!("/fo{}", res_path.display()))
            .arg(format!("/I{}", out_dir.display()))
            .arg(rc_path.display().to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && res_path.exists() {
            println!("cargo:rustc-link-arg={}", res_path.display());
        } else {
            println!(
                "cargo:warning=rc.exe 编译图标资源失败，本次构建不嵌入图标（托盘会回退为运行时绘制的图标）"
            );
        }
    } else {
        println!("cargo:warning=未找到 rc.exe，跳过图标嵌入（托盘会回退为运行时绘制的图标）");
    }
}

/// 扫描常见 Windows Kits 目录，取最高版本的 rc.exe
fn find_rc_exe() -> Option<PathBuf> {
    let roots = [
        r"C:\Program Files (x86)\Windows Kits".to_string(),
        r"C:\Program Files\Windows Kits".to_string(),
        r"D:\Program Files (x86)\Windows Kits".to_string(),
        r"D:\Program Files\Windows Kits".to_string(),
    ];
    let mut best: Option<(String, PathBuf)> = None;
    for root in roots {
        let bin = PathBuf::from(root).join("10\\bin");
        let Ok(dirs) = fs::read_dir(&bin) else { continue };
        for d in dirs.flatten() {
            let rc = d.path().join("x64\\rc.exe");
            if rc.exists() {
                let ver = d.file_name().to_string_lossy().to_string();
                if best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
                    best = Some((ver, rc));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 生成 ICO 文件字节（48/32/16 三张，从大到小排列——首张作为文件图标更清晰）
fn build_icon_ico() -> Vec<u8> {
    let sizes = [48usize, 32, 16];
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &s in &sizes {
        let (px, and) = draw_icon_frame(s);
        let row = s * 4;
        let and_row = (s + 31) / 32 * 4; // ICO 规范：AND 掩码行必须 4 字节对齐
        // 单张位图数据 = BITMAPINFOHEADER + XOR 像素(bottom-up) + AND 掩码(bottom-up)
        let mut img = Vec::with_capacity(40 + px.len() + and.len());
        img.extend_from_slice(&40u32.to_le_bytes()); // biSize
        img.extend_from_slice(&(s as i32).to_le_bytes()); // biWidth
        img.extend_from_slice(&((s * 2) as i32).to_le_bytes()); // biHeight = 2h（XOR+AND，正 = bottom-up）
        img.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        img.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        img.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
        img.extend_from_slice(&((px.len() + and.len()) as u32).to_le_bytes()); // biSizeImage
        img.extend_from_slice(&[0u8; 16]); // 其余字段全 0
        for y in (0..s).rev() {
            img.extend_from_slice(&px[y * row..(y + 1) * row]);
        }
        for y in (0..s).rev() {
            img.extend_from_slice(&and[y * and_row..(y + 1) * and_row]);
        }
        images.push(img);
    }

    // ICONDIR + ICONDIRENTRY
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0u8, 0]); // reserved
    ico.extend_from_slice(&[1u8, 0]); // type = icon
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + images.len() * 16;
    for (i, img) in images.iter().enumerate() {
        let s = sizes[i];
        ico.extend_from_slice(&[s as u8, s as u8, 0, 0]); // 宽/高/色数/保留
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.len();
    }
    for img in &images {
        ico.extend_from_slice(img);
    }
    ico
}

/// 绘制取景框图案（四角 L 形天蓝框 + 中心白点），按尺寸比例缩放
///
/// 在 4 倍分辨率上绘制后降采样（超采样抗锯齿），得到平滑边缘的
/// 32bpp BGRA 半透明像素；返回 (像素 top-down, AND 掩码 top-down，4 字节行对齐，1=透明)
fn draw_icon_frame(s: usize) -> (Vec<u8>, Vec<u8>) {
    const SS: usize = 4; // 超采样倍数
    let big = s * SS;
    let bi = big as i32;

    // 在 4x 画布上以整数像素绘制覆盖区域
    let mut big_px: Vec<Option<(u8, u8, u8)>> = vec![None; big * big];
    let mut put = |x: i32, y: i32, b: u8, g: u8, r: u8| {
        if x < 0 || y < 0 || x >= bi || y >= bi {
            return;
        }
        big_px[y as usize * big + x as usize] = Some((b, g, r));
    };

    let t = (bi * 3 / 32).max(1); // 线宽
    let m0 = bi * 5 / 32; // 角内边距
    let m1 = bi - m0;
    let arm = (bi * 8 / 32).max(2); // 臂长
    let (b, g, r) = (255u8, 120u8, 60u8); // 天蓝 RGB(60,120,255) 的 BGRA 顺序
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
    // 中心白点（菱形）
    let c = bi / 2;
    let rad = (bi * 2 / 32).max(1);
    for dy in -rad..=rad {
        for dx in -rad..=rad {
            if dx.abs() + dy.abs() <= rad {
                put(c + dx, c + dy, 255, 255, 255);
            }
        }
    }

    // 降采样：每个输出像素的 alpha = 4x4 子像素覆盖率，颜色取占多数的色
    let mut px = vec![0u8; s * s * 4];
    let and_row = (s + 31) / 32 * 4; // 4 字节对齐（与 ICO 组装处一致）
    let mut and = vec![0xFFu8; and_row * s];
    for y in 0..s {
        for x in 0..s {
            let mut blue = 0usize;
            let mut white = 0usize;
            for sy in 0..SS {
                for sx in 0..SS {
                    match big_px[(y * SS + sy) * big + (x * SS + sx)] {
                        Some((255, 120, 60)) => blue += 1,
                        Some(_) => white += 1,
                        None => {}
                    }
                }
            }
            let (n, color) = if blue >= white {
                (blue, (255u8, 120u8, 60u8))
            } else {
                (white, (255u8, 255u8, 255u8))
            };
            if n > 0 {
                let a = (n * 255 / (SS * SS)) as u8;
                let off = (y * s + x) * 4;
                px[off] = color.0;
                px[off + 1] = color.1;
                px[off + 2] = color.2;
                px[off + 3] = a;
                if a >= 128 {
                    and[y * and_row + x / 8] &= !(1 << (7 - (x % 8)));
                }
            }
        }
    }
    (px, and)
}
