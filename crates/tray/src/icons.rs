//! 托盘图标:运行时用 GDI 在 32bpp 内存 DIB 上绘制,零 .ico 资源文件。
//!
//! 三态(中性色,v0.1 不做深浅色主题自适应):
//! - Running = 实心圆(绿)
//! - Paused  = 空心圆(灰描边)
//! - Error   = 黄三角
//!
//! 透明度实现:32bpp DIB 零初始化后,用非黑色画刷/画笔绘形;GDI 不写 alpha
//! 字节(保持 0),收尾时逐像素修复——纯黑像素(未绘制的背景)alpha=0(透明),
//! 其余 alpha=255。因此三态配色刻意避开纯黑。启动时各画一次,之后仅
//! NIM_MODIFY 换句柄,不重复分配。

#![allow(non_snake_case)]

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    CreateDIBSection, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, Ellipse, GetDC,
    GetStockObject, Polygon, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, NULL_BRUSH, PS_SOLID,
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
                draw(IconShape::FilledCircle, size)?,
                draw(IconShape::HollowCircle, size)?,
                draw(IconShape::Triangle, size)?,
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
    FilledCircle,
    HollowCircle,
    Triangle,
}

impl IconShape {
    fn brush_color(self) -> u32 {
        match self {
            // 中性绿(RGB, COLORREF 为 0x00BBGGRR)
            Self::FilledCircle => 0x005FA02E,
            Self::HollowCircle => 0x00000000, // 内部不填(NULL_BRUSH),仅用 pen
            // 中性黄
            Self::Triangle => 0x0020C0E8,
        }
    }

    fn pen_color(self) -> u32 {
        match self {
            Self::FilledCircle => 0x00487924,
            // 中性灰描边
            Self::HollowCircle => 0x008A8A8A,
            // 深黄褐描边
            Self::Triangle => 0x00005A6A,
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

        match shape {
            IconShape::FilledCircle => {
                let brush = CreateSolidBrush(shape.brush_color());
                let pen = CreatePen(PS_SOLID, 1, shape.pen_color());
                let old_brush = SelectObject(mem, brush);
                let old_pen = SelectObject(mem, pen);
                Ellipse(mem, lo, lo, hi, hi);
                SelectObject(mem, old_brush);
                SelectObject(mem, old_pen);
                DeleteObject(brush);
                DeleteObject(pen);
            }
            IconShape::HollowCircle => {
                let pen = CreatePen(
                    PS_SOLID,
                    ((m / 10.0).round() as i32).max(1),
                    shape.pen_color(),
                );
                let old_pen = SelectObject(mem, pen);
                let old_brush = SelectObject(mem, GetStockObject(NULL_BRUSH));
                Ellipse(mem, lo, lo, hi, hi);
                SelectObject(mem, old_pen);
                SelectObject(mem, old_brush);
                DeleteObject(pen);
            }
            IconShape::Triangle => {
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
                let brush = CreateSolidBrush(shape.brush_color());
                let pen = CreatePen(PS_SOLID, 1, shape.pen_color());
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
