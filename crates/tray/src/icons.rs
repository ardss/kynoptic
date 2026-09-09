//! 托盘图标:运行时用 GDI 在 32bpp 内存 DIB 上绘制,零 .ico 资源文件。
//!
//! 母题来自品牌图标(B4 orbit-K):K 竖笔 + 斜笔 + 轨道弧 + 琥珀卫星点。
//! 三态(扁平版,小尺寸可辨认优先):
//! - Running = 蓝色 K 母题 + 琥珀点
//! - Paused  = 同母题全灰
//! - Error   = 黄三角(错误语义优先于品牌)
//!
//! 透明度实现:32bpp DIB 零初始化后,用非黑色画刷/画笔绘形;GDI 不写 alpha
//! 字节(保持 0),收尾时逐像素修复——纯黑像素(未绘制的背景)alpha=0(透明),
//! 其余 alpha=255。因此三态配色刻意避开纯黑。启动时各画一次,之后仅
//! NIM_MODIFY 换句柄,不重复分配。

#![allow(non_snake_case)]

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    CreateDIBSection, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, Ellipse, GetDC, Polygon,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, PS_SOLID,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GetSystemMetrics, ICONINFO, SM_CXSMICON,
};

use crate::state::TrayState;

/// 三态图标句柄([Running, Paused, Error])。
pub struct TrayIcons {
    handles: [windows_sys::Win32::UI::WindowsAndMessaging::HICON; 3],
}

impl TrayIcons {
    /// 启动时绘制全部三态。任一绘制失败返回 None(调用方降级处理)。
    pub fn create() -> Option<Self> {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        Some(Self {
            handles: [
                draw(IconShape::Running, size)?,
                draw(IconShape::Paused, size)?,
                draw(IconShape::Error, size)?,
            ],
        })
    }

    pub fn for_state(
        &self,
        state: TrayState,
    ) -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
        self.handles[state as usize]
    }
}

impl Drop for TrayIcons {
    fn drop(&mut self) {
        for h in &mut self.handles {
            if !h.is_null() {
                unsafe { DestroyIcon(*h) };
                *h = std::ptr::null_mut();
            }
        }
    }
}

/// 三态形状与配色(避开纯黑:纯黑被 alpha 修复当作透明背景)。
#[derive(Clone, Copy)]
enum IconShape {
    /// 采集中:品牌蓝 K 母题 + 琥珀点
    Running,
    /// 已暂停:同母题全灰
    Paused,
    /// 异常:黄三角
    Error,
}

/// COLORREF(0x00BBGGRR)。
const BLUE_MAIN: u32 = 0x00FFA14E; // #4EA1FF
const AMBER: u32 = 0x0054B4FF; // #FFB454
const GRAY_MAIN: u32 = 0x00A0A0A0;
const GRAY_DOT: u32 = 0x00C8C8C8;

impl IconShape {
    fn body_color(self) -> u32 {
        match self {
            Self::Running => BLUE_MAIN,
            Self::Paused => GRAY_MAIN,
            Self::Error => 0x0020C0E8,
        }
    }

    fn dot_color(self) -> u32 {
        match self {
            Self::Running => AMBER,
            Self::Paused => GRAY_DOT,
            Self::Error => 0x0020C0E8,
        }
    }
}

/// 绘制一枚图标:size 为系统小图标边长(SM_CXSMICON,通常 16)。
fn draw(shape: IconShape, size: i32) -> Option<windows_sys::Win32::UI::WindowsAndMessaging::HICON> {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = windows_sys::Win32::Graphics::Gdi::CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        if mem.is_null() {
            return None;
        }

        // 32bpp top-down DIB,零初始化(alpha 全 0 = 全透明背景)
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [std::mem::zeroed()],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color_bm =
            CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if color_bm.is_null() || bits.is_null() {
            DeleteDC(mem);
            return None;
        }
        // 1bpp 掩码:全 0 = 不透明(alpha 通道才是真正的透明面)
        let mask_bm =
            windows_sys::Win32::Graphics::Gdi::CreateBitmap(size, size, 1, 1, std::ptr::null());
        if mask_bm.is_null() {
            DeleteObject(color_bm);
            DeleteDC(mem);
            return None;
        }
        let old_bm = SelectObject(mem, color_bm);

        // 留 1px 边距的正方形内接几何
        let m = size as f64;
        let inset = 1.0;
        let lo = inset as i32;
        let hi = (m - inset).round() as i32;
        let _ = hi; // 部分形状直接用 m 计算边界

        match shape {
            IconShape::Running | IconShape::Paused => {
                let brush = CreateSolidBrush(shape.body_color());
                let old_brush = SelectObject(mem, brush);
                // 竖笔:左侧圆角矩形(细体,留出右侧空间给轨道)
                let stem_w = (m * 0.22).round() as i32;
                let top = (m * 0.10).round() as i32;
                let bot = (m * 0.90).round() as i32;
                windows_sys::Win32::Graphics::Gdi::RoundRect(mem, lo, top, lo + stem_w, bot, 6, 6);
                // 斜笔:一条粗线从竖笔中部到右下
                let pen = CreatePen(
                    PS_SOLID,
                    ((m * 0.16).round() as i32).max(1),
                    shape.body_color(),
                );
                let old_pen = SelectObject(mem, pen);
                windows_sys::Win32::Graphics::Gdi::MoveToEx(
                    mem,
                    lo + stem_w / 2,
                    (m * 0.52).round() as i32,
                    std::ptr::null_mut(),
                );
                windows_sys::Win32::Graphics::Gdi::LineTo(mem, (m * 0.86).round() as i32, bot);
                // 轨道弧(size>=24 才画,16px 下过于细碎)
                if size >= 24 {
                    let arc_pen = CreatePen(
                        PS_SOLID,
                        ((m * 0.07).round() as i32).max(1),
                        shape.body_color(),
                    );
                    let old_arc = SelectObject(mem, arc_pen);
                    let ar = m * 0.42;
                    let acx = m * 0.52;
                    let acy = m * 0.42;
                    windows_sys::Win32::Graphics::Gdi::Arc(
                        mem,
                        (acx - ar).round() as i32,
                        (acy - ar).round() as i32,
                        (acx + ar).round() as i32,
                        (acy + ar).round() as i32,
                        (acx + ar * 0.87).round() as i32,
                        (acy - ar * 0.50).round() as i32,
                        (acx - ar * 0.87).round() as i32,
                        (acy + ar * 0.50).round() as i32,
                    );
                    SelectObject(mem, old_arc);
                    DeleteObject(arc_pen);
                }
                SelectObject(mem, old_pen);
                DeleteObject(pen);
                // 琥珀卫星点(右上,轨道端点处)
                let dr = (m * 0.13).round() as i32;
                let dcx = (m * 0.84).round() as i32;
                let dcy = (m * 0.16).round() as i32;
                let dot = CreateSolidBrush(shape.dot_color());
                SelectObject(mem, dot);
                Ellipse(mem, dcx - dr, dcy - dr, dcx + dr, dcy + dr);
                SelectObject(mem, old_brush);
                DeleteObject(dot);
            }
            IconShape::Error => {
                // 顶点在上、底边在下,微收腰边距
                let mut pts: [POINT; 3] = [
                    POINT {
                        x: (m / 2.0).round() as i32,
                        y: (m * 0.10).round() as i32,
                    },
                    POINT {
                        x: (m * 0.92).round() as i32,
                        y: (m * 0.90).round() as i32,
                    },
                    POINT {
                        x: (m * 0.08).round() as i32,
                        y: (m * 0.90).round() as i32,
                    },
                ];
                let brush = CreateSolidBrush(shape.body_color());
                let pen = CreatePen(PS_SOLID, 1, 0x00005A6A);
                let old_brush = SelectObject(mem, brush);
                let old_pen = SelectObject(mem, pen);
                Polygon(mem, pts.as_mut_ptr(), 3);
                SelectObject(mem, old_brush);
                SelectObject(mem, old_pen);
                DeleteObject(brush);
                DeleteObject(pen);
            }
        }

        // alpha 修复:GDI 写色不写 alpha。纯黑(未绘制的零初始化背景)= 透明,
        // 其余像素 = 不透明。三态配色均避开纯黑。
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        for p in pixels.iter_mut() {
            if *p & 0x00FF_FFFF == 0 {
                *p = 0;
            } else {
                *p |= 0xFF00_0000;
            }
        }

        SelectObject(mem, old_bm);
        DeleteDC(mem);

        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bm,
            hbmColor: color_bm,
        };
        let hicon = CreateIconIndirect(&info);
        DeleteObject(color_bm);
        DeleteObject(mask_bm);
        if hicon.is_null() {
            None
        } else {
            Some(hicon)
        }
    }
}
