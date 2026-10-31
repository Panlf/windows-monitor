//! 桌面悬浮窗：UpdateLayeredWindow 逐像素 alpha 渲染的 macOS 风格面板
//!
//! 渲染架构（每帧整幅重绘，无残影）：
//! 1. 32bpp DIB 上逐像素生成面板：垂直渐变 + 圆角 SDF 抗锯齿覆盖度 + 内侧高光描边
//! 2. GDI 在 DIB 上绘制文本（透明背景、灰度抗锯齿）
//! 3. 恢复被 GDI 清零的 alpha 字节（面板内 alpha = opacity × 覆盖度）
//! 4. UpdateLayeredWindow(ULW_ALPHA) 一次性提交
//!
//! 好处：圆角是真抗锯齿（非 GDI 硬边 region 裁剪）、半透明逐像素可控（毛玻璃质感）、
//! 文本永不叠影、不依赖未文档化的 SetWindowCompositionAttribute（与亚克力 API 相比
//! 无兼容性坑，且透明度可以直接复用设置里的 opacity）。

use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateDIBSection, CreateFontW, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, Ellipse, FillRect, GetDC, GetMonitorInfoW,
    MonitorFromPoint, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextCharacterExtra,
    SetTextColor, TextOutW, ANTIALIASED_QUALITY, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, BI_RGB, CLIP_DEFAULT_PRECIS, DIB_RGB_COLORS, DEFAULT_CHARSET,
    DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, FW_NORMAL, HBITMAP, HBRUSH,
    HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITORINFO, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_SELECTED};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetClientRect,
    GetCursorPos, GetWindowRect, IsWindowVisible, LoadCursorW, PostQuitMessage, RegisterClassW,
    SendMessageW, SetForegroundWindow, SetMenuInfo, SetTimer, SetWindowPos, ShowWindow,
    TrackPopupMenu, ULW_ALPHA, UpdateLayeredWindow, HTCAPTION, HMENU, IDC_ARROW, MENUINFO,
    MIM_BACKGROUND, MF_OWNERDRAW, SW_HIDE, SW_SHOW, SWP_NOMOVE, SWP_NOACTIVATE, SWP_NOZORDER,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_DPICHANGED, WM_DRAWITEM, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MEASUREITEM, WM_NCLBUTTONDOWN, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::metrics::{Metric, Sample, ALL_METRICS, CATEGORY_ORDER};
use crate::sample_row::Row;
use crate::sampler::Status;
use crate::util;

const WIDTH: i32 = 280;
const PAD: i32 = 14;
const HEADER_H: i32 = 40; // 顶部标题行（含状态点）
const SECTION_TITLE_H: i32 = 20;
const ROW_H: i32 = 21;
const LINE_GAP: i32 = 13; // 组间间距（含分隔线）
const ROUND_R: i32 = 14; // 圆角半径
const DOT: i32 = 12; // 状态点直径
const SEP_ID: u32 = 0xFFFF; // owner-draw 菜单分隔线 ID

// 状态点取值
const ST_IDLE: u8 = 0;
const ST_REC: u8 = 1;
const ST_ERR: u8 = 2;

// ---- DPI 感知：所有尺寸/字号按窗口所在显示器 DPI 缩放 ----

static DPI: AtomicI32 = AtomicI32::new(0);

fn dpi() -> i32 {
    let d = DPI.load(Ordering::Relaxed);
    if d > 0 { d } else { 96 }
}

/// 96 基准值 → 当前 DPI 像素值
fn s(v: i32) -> i32 {
    v * dpi() / 96
}

// ---- 字体缓存（按 DPI 重建） ----

struct Fonts {
    title: HFONT,
    section: HFONT,
    label: HFONT,
    value: HFONT,
}
// 仅 UI 线程访问，手工声明以放进静态变量
unsafe impl Send for Fonts {}

static FONTS: Mutex<Option<(i32, Fonts)>> = Mutex::new(None);

fn make_font(size: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            size, 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY,
            FF_DONTCARE.0 as u32, PCWSTR(util::wide("Segoe UI").as_ptr()),
        )
    }
}

fn fonts() -> Fonts {
    let d = dpi();
    let mut g = FONTS.lock().unwrap();
    if let Some((cached, f)) = g.as_ref() {
        if *cached == d {
            return Fonts { title: f.title, section: f.section, label: f.label, value: f.value };
        }
    }
    // DPI 变化重建前先删除旧字体，否则 4 个 HFONT 每次跨屏拖动泄漏一次
    if let Some((_, old)) = g.take() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(old.title.0));
            let _ = DeleteObject(HGDIOBJ(old.section.0));
            let _ = DeleteObject(HGDIOBJ(old.label.0));
            let _ = DeleteObject(HGDIOBJ(old.value.0));
        }
    }
    let f = Fonts {
        title: make_font(s(-15), FW_BOLD.0 as i32),
        section: make_font(s(-11), FW_NORMAL.0 as i32),
        label: make_font(s(-12), FW_NORMAL.0 as i32),
        value: make_font(s(-12), 600), // semibold：数值列醒目但不笨重
    };
    // HFONT 是 Copy，逐字段复制一份返回，所有权进静态缓存
    let out = Fonts { title: f.title, section: f.section, label: f.label, value: f.value };
    *g = Some((d, f));
    out
}

// ---- 面板画刷（仅右键菜单用） ----

struct BrushHandle(HBRUSH);
unsafe impl Send for BrushHandle {}
unsafe impl Sync for BrushHandle {}

static BRUSH_MENU_BG: OnceLock<BrushHandle> = OnceLock::new();

fn color(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

fn menu_bg_brush() -> HBRUSH {
    BRUSH_MENU_BG
        .get_or_init(|| BrushHandle(unsafe { CreateSolidBrush(color(44, 42, 41)) }))
        .0
}

// ---- 分层表面：32bpp 顶层向下 DIB（预乘 alpha），尺寸变化时重建 ----

struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u8,
    cov: Vec<u8>, // 每像素圆角覆盖度（0..255），供文字绘制后恢复 alpha
    w: i32,
    h: i32,
}
unsafe impl Send for Surface {}

static SURFACE: Mutex<Option<Surface>> = Mutex::new(None);

/// 在与窗口尺寸匹配的 DIB 表面上执行绘制；尺寸不符时重建
fn with_surface(w: i32, h: i32, f: impl FnOnce(&mut Surface)) -> bool {
    let mut g = match SURFACE.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some(sf) = g.as_mut() {
        if sf.w == w && sf.h == h {
            f(sf);
            return true;
        }
    }
    // 重建：先删旧（删 DC 会自动取消位图选中）
    if let Some(old) = g.take() {
        unsafe {
            let _ = DeleteDC(old.dc);
            let _ = DeleteObject(HGDIOBJ(old.bmp.0));
        }
    }
    unsafe {
        let screen = GetDC(None);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // 负高 = 顶层向下，逐像素索引更直观
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = match CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(b) => b,
            Err(_) => {
                let _ = ReleaseDC(None, screen);
                return false;
            }
        };
        let dc = CreateCompatibleDC(Some(screen));
        let _ = ReleaseDC(None, screen);
        let _ = SelectObject(dc, HGDIOBJ(bmp.0));
        let mut sf = Surface {
            dc,
            bmp,
            bits: bits as *mut u8,
            cov: vec![0; (w * h) as usize],
            w,
            h,
        };
        f(&mut sf);
        *g = Some(sf);
        true
    }
}

// ---- 面板几何：圆角矩形 SDF ----

/// 圆角矩形有向距离场（负值在内部），用于逐像素抗锯齿覆盖度
fn rr_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let hw = w * 0.5;
    let hh = h * 0.5;
    let dx = (px - hw).abs() - (hw - r);
    let dy = (py - hh).abs() - (hh - r);
    let ax = dx.max(0.0);
    let ay = dy.max(0.0);
    (ax * ax + ay * ay).sqrt() + dx.max(dy).min(0.0) - r
}

fn lerp(a: u32, b: u32, t: f32) -> u32 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u32
}

/// 面板逐像素：垂直渐变 + 圆角覆盖度 + 内侧高光描边；颜色按预乘 alpha 写入
fn fill_panel(sf: &mut Surface, op: u32) {
    let (w, h) = (sf.w, sf.h);
    let r = s(ROUND_R) as f32;
    let (wf, hf) = (w as f32, h as f32);
    // 对齐 iOS 参考图：低饱和深暖灰玻璃（非蓝黑），白色文字分级靠透明度区分
    let top = (64u32, 60u32, 57u32);
    let bot = (37u32, 34u32, 33u32);
    let border = (255u32, 255u32, 255u32); // 白色玻璃高光圈
    let opf = op.min(255) as f32 / 255.0;
    // 顶部柔光源（玻璃通透感关键）：顶部中心最亮，向四周平方衰减，颜色向白提亮
    let glow_cx = wf * 0.5;
    let glow_cy = hf * 0.10;
    let glow_r = (wf * wf + hf * hf).sqrt() * 0.72;
    for y in 0..h {
        let t = if h > 1 { y as f32 / (h - 1) as f32 } else { 0.0 };
        let cy = y as f32 + 0.5;
        // SAFETY: bits 指向 w*h*4 字节的 DIB，行/列索引均在界内
        let row = unsafe { sf.bits.offset(y as isize * w as isize * 4) };
        // 当前行的柔光强度（同行内仅 x 变化，y 部分提前算）
        let gdy = cy - glow_cy;
        for x in 0..w {
            let d = rr_sdf(x as f32 + 0.5, cy, wf, hf, r);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            sf.cov[(y * w + x) as usize] = (cov * 255.0) as u8;
            let p = unsafe { row.offset(x as isize * 4) };
            if cov <= 0.0 {
                unsafe { *p.add(3) = 0; }
                continue;
            }
            // 基础渐变 + 柔光提亮
            let (mut br, mut bg, mut bb) = (lerp(top.0, bot.0, t) as f32, lerp(top.1, bot.1, t) as f32, lerp(top.2, bot.2, t) as f32);
            let gdx = x as f32 + 0.5 - glow_cx;
            let gd = ((gdx * gdx + gdy * gdy).sqrt() / glow_r).min(1.0);
            let sheen = (1.0 - gd) * (1.0 - gd) * 0.16;
            br += (255.0 - br) * sheen;
            bg += (255.0 - bg) * sheen;
            bb += (255.0 - bb) * sheen;
            // 描边：距边缘 -0.75px 处最亮，向内衰减（玻璃白边）
            let glow = (1.0 - (d + 0.75).abs()).clamp(0.0, 1.0) * 0.45;
            let (mr, mg, mb) = if glow > 0.0 {
                (
                    br + (border.0 as f32 - br) * glow,
                    bg + (border.1 as f32 - bg) * glow,
                    bb + (border.2 as f32 - bb) * glow,
                )
            } else {
                (br, bg, bb)
            };
            let a = opf * cov;
            unsafe {
                *p.add(0) = (mb * a) as u8; // B
                *p.add(1) = (mg * a) as u8; // G
                *p.add(2) = (mr * a) as u8; // R
                *p.add(3) = (a * 255.0) as u8; // A
            }
        }
    }
}

/// GDI 文本会清零像素 alpha 字节：按覆盖度恢复（面板内 alpha = opacity × 覆盖度）
fn restore_alpha(sf: &mut Surface, op: u32) {
    let op = (op.min(255)) as u32;
    let n = (sf.w * sf.h) as usize;
    for i in 0..n {
        let c = sf.cov[i] as u32;
        if c > 0 {
            unsafe { *sf.bits.add(i * 4 + 3) = (op * c / 255) as u8 };
        }
    }
}

// ---- 窗口 ----

/// 注册窗口类（字体/表面惰性创建）
pub fn register(hinstance: windows::Win32::Foundation::HINSTANCE) {
    unsafe {
        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: super::app_icon(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszMenuName: windows::core::w!(""),
            lpszClassName: windows::core::w!("WinMonOverlay"),
        };
        RegisterClassW(&wc);
    }
}

/// 把坐标拉回离它最近的显示器工作区（至少 60px 可见）。
/// 防止 ini 里保存的坐标落在已断开的显示器上，导致悬浮窗开在屏幕外看不到。
fn clamp_to_monitor(x: i32, y: i32) -> (i32, i32) {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return (x, y);
        }
        let rc = &mi.rcWork;
        // 左边界允许窗口左移出屏 w-60（保证右侧 60px 在屏内），右边界同理
        let nx = x.max(rc.left - (s(WIDTH) - 60)).min(rc.right - 60);
        let ny = y.max(rc.top - (120 - 60)).min(rc.bottom - 60);
        (nx, ny)
    }
}

/// 创建悬浮窗（位置来自配置；内容在 relayout/paint_layered 时绘制）
pub fn create(
    hinstance: windows::Win32::Foundation::HINSTANCE,
    x: i32,
    y: i32,
    visible: bool,
) -> HWND {
    let (x, y) = clamp_to_monitor(x, y);
    unsafe {
        let d = GetDpiForSystem();
        DPI.store(if d == 0 { 96 } else { d as i32 }, Ordering::Relaxed);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            windows::core::w!("WinMonOverlay"),
            windows::core::w!("WinMon"),
            WS_POPUP,
            x,
            y,
            s(WIDTH),
            s(120),
            None,
            None,
            Some(hinstance),
            None,
        )
        .expect("创建悬浮窗失败");

        SetTimer(Some(hwnd), super::TIMER_REFRESH, 500, None);
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        hwnd
    }
}

pub fn set_visible(hwnd: HWND, visible: bool) {
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
}

/// 整幅重绘悬浮窗：面板 → 文本 → 恢复 alpha → UpdateLayeredWindow 提交
pub fn paint_layered(hwnd: HWND) {
    unsafe {
        let d = GetDpiForWindow(hwnd);
        DPI.store(if d == 0 { 96 } else { d as i32 }, Ordering::Relaxed);

        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return;
        }
        let (w, h) = (rc.right, rc.bottom);
        if w <= 0 || h <= 0 {
            return;
        }
        let op = super::with_ctx(|ctx| ctx.config.overlay_opacity).unwrap_or(225) as u32;
        let (sections, status) = super::with_ctx(|ctx| {
            let smp = ctx.shared.latest();
            let sections = build_sections(&ctx.config.display_metrics, smp.as_ref());
            let status = match &*ctx.shared.status.lock().unwrap() {
                Status::Recording { .. } => ST_REC,
                Status::Error(_) => ST_ERR,
                Status::Idle => ST_IDLE,
            };
            (sections, status)
        })
        .unwrap_or_default();

        if !with_surface(w, h, |sf| {
            fill_panel(sf, op);
            draw_content(sf.dc, w, &sections, status, op);
            restore_alpha(sf, op);
        }) {
            return;
        }

        // 提交到窗口（预乘 alpha：AC_SRC_ALPHA）
        let g = SURFACE.lock().unwrap();
        let Some(sf) = g.as_ref() else { return };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            None,
            Some(&size),
            Some(sf.dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}

/// 悬浮窗的指标分组：按固定类别顺序分层
struct Section {
    title: &'static str,
    rows: Vec<Row>,
}

fn build_sections(metrics: &[Metric], sample: Option<&Sample>) -> Vec<Section> {
    let mut sections = Vec::new();
    for cat in CATEGORY_ORDER {
        let selected: Vec<Metric> = ALL_METRICS
            .iter()
            .copied()
            .filter(|m| m.category() == cat && metrics.contains(m))
            .collect();
        if selected.is_empty() {
            continue;
        }
        let rows = Row::build(&selected, sample);
        sections.push(Section { title: cat, rows });
    }
    sections
}

fn layout_height(sections: &[Section]) -> i32 {
    let mut h = s(HEADER_H);
    for (i, sec) in sections.iter().enumerate() {
        if i > 0 {
            h += s(LINE_GAP);
        }
        h += s(SECTION_TITLE_H) + sec.rows.len() as i32 * s(ROW_H);
    }
    h + s(PAD)
}

/// 指标数量/DPI 变化后调整窗口高度并整幅重绘
pub fn relayout(hwnd: HWND) {
    unsafe {
        let d = GetDpiForWindow(hwnd);
        DPI.store(if d == 0 { 96 } else { d as i32 }, Ordering::Relaxed);
    }
    // 上下文拿不到（锁被占用）时跳过本次重排，绝不能 fallback 到最小高度——
    // 否则重入场景下悬浮窗会被错误缩成 60px 高的一条
    let Some(h) = super::with_ctx(|ctx| {
        let smp = ctx.shared.latest();
        layout_height(&build_sections(&ctx.config.display_metrics, smp.as_ref()))
    }) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        if rc.right != s(WIDTH) || rc.bottom != h {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                s(WIDTH),
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        paint_layered(hwnd);
    }
}

pub unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // 内容全部经 UpdateLayeredWindow 提交，PAINT 只需验证区域
        WM_PAINT => {
            unsafe {
                let _ = BeginPaint(hwnd, &mut PAINTSTRUCT::default());
            }
            LRESULT(0)
        }
        // 分层窗口内容自管，永远跳过系统背景擦除（避免闪烁）
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            // 隐藏时（托盘/右键菜单隐藏）完全跳过渲染，空闲 CPU 开销归零
            if unsafe { IsWindowVisible(hwnd).as_bool() } {
                // 数据没变化就不重绘：空闲时跳过整帧绘制，降低常驻 CPU 开销
                static LAST_T: AtomicI64 = AtomicI64::new(0);
                let cur = super::with_ctx(|ctx| ctx.shared.latest().map(|s| s.t))
                    .flatten()
                    .unwrap_or(0);
                if cur != LAST_T.swap(cur, Ordering::Relaxed) {
                    relayout(hwnd); // 磁盘数量变化/DPI 变化时需要重排
                }
            }
            LRESULT(0)
        }
        // 跨不同 DPI 显示器拖动：按系统建议的新位置尺寸调整，再重建字体/表面
        WM_DPICHANGED => {
            let d = (wparam.0 & 0xFFFF) as u32;
            if d != 0 {
                DPI.store(d as i32, Ordering::Relaxed);
            }
            unsafe {
                let p = lparam.0 as *const RECT;
                if !p.is_null() {
                    let rc = *p;
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rc.left,
                        rc.top,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            relayout(hwnd);
            LRESULT(0)
        }
        // owner-draw 菜单：量尺寸
        WM_MEASUREITEM => {
            unsafe {
                let mi = lparam.0 as *mut MEASUREITEMSTRUCT;
                let m = &mut *mi;
                if m.itemID == SEP_ID {
                    m.itemWidth = 10;
                    m.itemHeight = 9;
                } else {
                    m.itemWidth = 230;
                    m.itemHeight = 30;
                }
            }
            LRESULT(1)
        }
        // owner-draw 菜单：绘制
        WM_DRAWITEM => {
            unsafe {
                let di = lparam.0 as *mut DRAWITEMSTRUCT;
                draw_menu_item(&*di);
            }
            LRESULT(1)
        }
        // 左键按住拖动窗口（转入标题栏拖动逻辑）
        WM_LBUTTONDOWN => {
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                Some(LPARAM(0)),
            );
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_menu(hwnd);
            LRESULT(0)
        }
        // 托盘图标回调
        msg if msg == super::WM_APP_TRAY => {
            let mouse = (lparam.0 & 0xFFFF) as u32;
            if mouse == WM_LBUTTONUP {
                super::settings::open();
            } else if mouse == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_ok() {
                super::with_ctx(|ctx| {
                    ctx.config.overlay_x = rc.left;
                    ctx.config.overlay_y = rc.top;
                });
                super::save_config();
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd = (wparam.0 & 0xFFFF) as u32;
            super::handle_command(cmd, wparam);
            LRESULT(0)
        }
        WM_CLOSE => {
            // 点 X = 缩到系统托盘（程序继续后台驻留）；彻底退出请用右键菜单/设置里的“退出程序”
            super::with_ctx(|ctx| ctx.config.overlay_visible = false);
            let _ = ShowWindow(hwnd, SW_HIDE);
            super::save_config();
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn append_owner_item(menu: HMENU, texts: &mut Vec<Vec<u16>>, id: u32, text: &str) {
    texts.push(util::wide(text));
    let ptr = PCWSTR(texts.last().unwrap().as_ptr());
    let _ = AppendMenuW(menu, MF_OWNERDRAW, id as usize, ptr);
}

unsafe fn append_owner_sep(menu: HMENU) {
    let _ = AppendMenuW(menu, MF_OWNERDRAW, SEP_ID as usize, PCWSTR(std::ptr::null()));
}

/// 右键菜单（owner-draw 深色，配合毛玻璃主题）
unsafe fn show_menu(hwnd: HWND) {
    let rec = super::with_ctx(|ctx| {
        matches!(*ctx.shared.status.lock().unwrap(), Status::Recording { .. })
    })
    .unwrap_or(false);

    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    // owner-draw 菜单的 itemData 是文本指针（系统不复制），必须保证文本活到菜单销毁
    let mut texts: Vec<Vec<u16>> = Vec::new();

    append_owner_item(menu, &mut texts, super::CMD_SETTINGS, "设置(&S)");
    let visible_now = super::with_ctx(|ctx| ctx.config.overlay_visible).unwrap_or(true);
    let toggle_text = if visible_now { "隐藏悬浮窗(&H)" } else { "显示悬浮窗(&H)" };
    append_owner_item(menu, &mut texts, super::CMD_TOGGLE_OVERLAY, toggle_text);
    append_owner_sep(menu);
    if rec {
        append_owner_item(menu, &mut texts, super::CMD_STOP_RECORD, "停止记录(&T)");
    } else {
        append_owner_item(menu, &mut texts, super::CMD_START_RECORD, "开始记录(&R)");
    }
    append_owner_item(menu, &mut texts, super::CMD_REPORT, "生成分析报告(&G)");
    append_owner_item(menu, &mut texts, super::CMD_OPEN_DIR, "打开日志目录(&O)");
    append_owner_sep(menu);
    append_owner_item(menu, &mut texts, super::CMD_EXIT, "退出(&X)");

    // 菜单整体深色背景
    let mut mi = MENUINFO::default();
    mi.cbSize = std::mem::size_of::<MENUINFO>() as u32;
    mi.fMask = MIM_BACKGROUND;
    mi.hbrBack = menu_bg_brush();
    let _ = SetMenuInfo(menu, &mi);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

unsafe fn read_wide_ptr(p: *const u16) -> String {
    let mut buf = [0u16; 64];
    for i in 0..64 {
        let c = *p.add(i);
        if c == 0 {
            return String::from_utf16_lossy(&buf[..i]);
        }
        buf[i] = c;
    }
    String::from_utf16_lossy(&buf)
}

unsafe fn draw_menu_item(di: &DRAWITEMSTRUCT) {
    let hdc = di.hDC;
    let rc = &di.rcItem;
    let selected = di.itemState.0 & ODS_SELECTED.0 != 0;
    let disabled = di.itemState.0 & ODS_DISABLED.0 != 0;

    // 分隔线：只画线，不参与选中高亮
    if di.itemID == SEP_ID {
        let line = CreateSolidBrush(color(88, 84, 82));
        let lr = RECT {
            left: rc.left + 12,
            top: rc.top + 4,
            right: rc.right - 12,
            bottom: rc.top + 5,
        };
        FillRect(hdc, &lr, line);
        let _ = DeleteObject(HGDIOBJ(line.0));
        return;
    }

    // 背景：选中态用白色微透圆角块（内缩 3px，圆角 8px），否则深玻璃底
    if selected {
        let hot = color(60, 57, 55); // ≈ 白色 10% 叠在深底上
        let b = CreateSolidBrush(hot);
        let pen = CreatePen(PS_SOLID, 1, hot);
        let old_b = SelectObject(hdc, HGDIOBJ(b.0));
        let old_p = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = RoundRect(
            hdc,
            rc.left + 3,
            rc.top + 2,
            rc.right - 3,
            rc.bottom - 2,
            8,
            8,
        );
        SelectObject(hdc, old_b);
        SelectObject(hdc, old_p);
        let _ = DeleteObject(HGDIOBJ(b.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    } else {
        let b = CreateSolidBrush(color(44, 42, 41));
        FillRect(hdc, rc, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }

    if di.itemData == 0 {
        return;
    }
    let text = read_wide_ptr(di.itemData as *const u16);

    let tc = if disabled {
        color(140, 138, 136)
    } else {
        color(244, 243, 242)
    };
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, tc);
    SelectObject(hdc, HGDIOBJ(fonts().label.0));

    let mut tr = RECT {
        left: rc.left + 14,
        top: rc.top,
        right: rc.right - 8,
        bottom: rc.bottom,
    };
    let mut t = util::wide(&text);
    DrawTextW(hdc, &mut t, &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
}

/// 画实心圆点（状态指示）
unsafe fn draw_dot(hdc: HDC, x: i32, y: i32, d: i32, c: COLORREF) {
    let b = CreateSolidBrush(c);
    let old = SelectObject(hdc, HGDIOBJ(b.0));
    let _ = Ellipse(hdc, x, y, x + d, y + d);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(b.0));
}

/// 文本色：按 opacity 预乘（分层窗口要求预乘 alpha，否则文字过亮发灰）
fn text_color(r: u32, g: u32, b: u32, op: u32) -> COLORREF {
    let op = op.min(255);
    color((r * op / 255) as u8, (g * op / 255) as u8, (b * op / 255) as u8)
}

/// 绘制悬浮窗内容（分组分层 + 状态点；面板背景由 fill_panel 提供）
unsafe fn draw_content(hdc: HDC, w: i32, sections: &[Section], status: u8, op: u32) {
    SetBkMode(hdc, TRANSPARENT);
    let pad = s(PAD);
    let f = fonts();

    // 标题（纯白加粗）
    SelectObject(hdc, HGDIOBJ(f.title.0));
    SetTextColor(hdc, text_color(255, 255, 255, op));
    let _ = TextOutW(hdc, pad, s(11), util::wide("WinMon").as_slice());

    // 状态点（右上角）：空闲空白、记录中绿点、错误红点
    let dot_x = w - pad - s(DOT);
    let dot_y = (s(HEADER_H) - s(DOT)) / 2;
    match status {
        ST_REC => draw_dot(hdc, dot_x, dot_y, s(DOT), text_color(58, 200, 110, op)),
        ST_ERR => draw_dot(hdc, dot_x, dot_y, s(DOT), text_color(255, 95, 85, op)),
        _ => {}
    }

    let mut y = s(HEADER_H);
    for (i, sec) in sections.iter().enumerate() {
        if i > 0 {
            // 组间细线：白色 40% 预乘（清晰的白色透光细线）
            let a = (op * 40 / 100) as u8;
            let line = CreateSolidBrush(color(a, a, a));
            let lr = RECT {
                left: pad,
                top: y + s(LINE_GAP) / 2,
                right: w - pad,
                bottom: y + s(LINE_GAP) / 2 + 1,
            };
            FillRect(hdc, &lr, line);
            let _ = DeleteObject(HGDIOBJ(line.0));
            y += s(LINE_GAP);
        }
        // 组标题（白 70% 小字 + 1px 字距，参考图分组标题比正文弱但不灰暗）
        SelectObject(hdc, HGDIOBJ(f.section.0));
        SetTextColor(hdc, text_color(178, 176, 174, op));
        SetTextCharacterExtra(hdc, 1);
        let _ = TextOutW(hdc, pad, y + 2, util::wide(sec.title).as_slice());
        SetTextCharacterExtra(hdc, 0);
        y += s(SECTION_TITLE_H);

        // 指标行：左标签（白 88%）、右数值（纯白 semibold）
        for row in &sec.rows {
            SelectObject(hdc, HGDIOBJ(f.label.0));
            SetTextColor(hdc, text_color(225, 223, 221, op));
            let _ = TextOutW(hdc, pad, y + 3, util::wide(&row.label).as_slice());

            let vc = match row.pct {
                Some(p) if p >= 85.0 => text_color(255, 122, 110, op),
                Some(p) if p >= 60.0 => text_color(255, 176, 84, op),
                _ => text_color(255, 255, 255, op),
            };
            SelectObject(hdc, HGDIOBJ(f.value.0));
            SetTextColor(hdc, vc);
            let mut vrc = RECT {
                left: 0,
                top: y,
                right: w - pad,
                bottom: y + s(ROW_H),
            };
            let mut vtext: Vec<u16> = util::wide(&row.value);
            DrawTextW(hdc, &mut vtext, &mut vrc, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
            y += s(ROW_H);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

    /// 回归测试：创建悬浮窗、重排、整幅 ULW 绘制一轮不崩溃。
    /// 覆盖分组渲染、面板逐像素填充、状态点、菜单外的主绘制路径。
    #[test]
    fn overlay_create_relayout_paint_does_not_crash() {
        let _guard = super::super::UI_TEST_LOCK.lock().unwrap();
        unsafe {
            let hmodule = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap_or_default();
            let hinstance: windows::Win32::Foundation::HINSTANCE = hmodule.into();
            register(hinstance);
            *crate::ui::CTX.lock().unwrap() = Some(crate::ui::UiCtx {
                shared: crate::sampler::Shared::new(1),
                config: crate::config::Config::default(),
                overlay_hwnd: HWND::default(),
                settings_hwnd: None,
            });

            let hwnd = create(hinstance, 50, 50, true);
            relayout(hwnd);
            paint_layered(hwnd);
            let _ = DestroyWindow(hwnd);

            *crate::ui::CTX.lock().unwrap() = None;
        }
    }
}
