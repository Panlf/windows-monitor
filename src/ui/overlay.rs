//! 桌面悬浮窗：半透明置顶面板，显示所选指标，可拖动、右键菜单

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, InvalidateRect, RoundRect,
    SelectObject, SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, FW_NORMAL,
    HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetClientRect,
    GetCursorPos, GetWindowRect, LoadCursorW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow,
    TrackPopupMenu, HTCAPTION, IDC_ARROW, LWA_ALPHA, LWA_COLORKEY, MF_SEPARATOR, MF_STRING,
    SW_HIDE, SW_SHOW, SWP_NOMOVE, SWP_NOACTIVATE, SWP_NOZORDER, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_RIGHTBUTTON, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_EXITSIZEMOVE, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_NCLBUTTONDOWN, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::sample_row::Row;
use crate::sampler::Status;
use crate::util;

const WIDTH: i32 = 260;
const HEADER_H: i32 = 28;
const ROW_H: i32 = 21;
const PAD: i32 = 10;

// GDI 句柄缓存：创建一次、进程退出由系统回收，避免每帧创建/销毁对象。
// 仅 UI 线程访问，Send/Sync 手工声明只为放进静态变量。
struct FontHandle(HFONT);
unsafe impl Send for FontHandle {}
unsafe impl Sync for FontHandle {}

struct BrushHandle(HBRUSH);
unsafe impl Send for BrushHandle {}
unsafe impl Sync for BrushHandle {}

struct MemDc {
    dc: HDC,
    bmp: HBITMAP,
    w: i32,
    h: i32,
}
unsafe impl Send for MemDc {}
unsafe impl Sync for MemDc {}

static FONT_NORMAL: OnceLock<FontHandle> = OnceLock::new();
static FONT_BOLD: OnceLock<FontHandle> = OnceLock::new();
static BRUSH_KEY: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_PANEL: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_BORDER: OnceLock<BrushHandle> = OnceLock::new();
static MEM_DC: Mutex<Option<MemDc>> = Mutex::new(None);

fn make_font(weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -13, 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY,
            FF_DONTCARE.0 as u32, PCWSTR(util::wide("Segoe UI").as_ptr()),
        )
    }
}

fn font_normal() -> HFONT {
    FONT_NORMAL.get_or_init(|| FontHandle(make_font(FW_NORMAL.0 as i32))).0
}

fn font_bold() -> HFONT {
    FONT_BOLD.get_or_init(|| FontHandle(make_font(FW_BOLD.0 as i32))).0
}

fn brush(get: &OnceLock<BrushHandle>, c: COLORREF) -> HBRUSH {
    get.get_or_init(|| BrushHandle(unsafe { CreateSolidBrush(c) })).0
}

/// 取或重建与窗口尺寸匹配的缓存内存 DC（双缓冲绘制用）
unsafe fn mem_dc(hdc: HDC, w: i32, h: i32) -> HDC {
    let mut cache = MEM_DC.lock().unwrap();
    if let Some(c) = cache.as_ref() {
        if c.w == w && c.h == h {
            return c.dc;
        }
    }
    // 尺寸变化：释放旧缓存（先删 DC 再删位图，删 DC 会自动取消位图选中）
    if let Some(old) = cache.take() {
        let _ = DeleteDC(old.dc);
        let _ = DeleteObject(HGDIOBJ(old.bmp.0));
    }
    let dc = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let _ = SelectObject(dc, HGDIOBJ(bmp.0));
    *cache = Some(MemDc { dc, bmp, w, h });
    dc
}

fn color(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// 注册窗口类（字体/画刷惰性创建，见 static 缓存）
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

/// 创建悬浮窗（位置与透明度来自配置）
pub fn create(
    hinstance: windows::Win32::Foundation::HINSTANCE,
    x: i32,
    y: i32,
    opacity: u8,
    visible: bool,
) -> HWND {
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            windows::core::w!("WinMonOverlay"),
            windows::core::w!("WinMon"),
            WS_POPUP,
            x,
            y,
            WIDTH,
            120,
            None,
            None,
            Some(hinstance),
            None,
        )
        .expect("创建悬浮窗失败");
        let _ = SetLayeredWindowAttributes(hwnd, color(255, 0, 255), opacity, LWA_ALPHA | LWA_COLORKEY);
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

/// 指标数量变化后调整窗口高度（高度没变时不动窗口也不重绘）
pub fn relayout(hwnd: HWND) {
    let rows = super::with_ctx(|ctx| {
        let s = ctx.shared.latest();
        Row::build(&ctx.config.display_metrics, s.as_ref()).len()
    })
    .unwrap_or(1);
    let h = HEADER_H + rows as i32 * ROW_H + PAD;
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        if rc.bottom != h {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                WIDTH,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

pub unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            // 数据没变化就不重绘：空闲时（数值不变）跳过整帧绘制，降低常驻 CPU/GDI 开销
            static LAST_T: AtomicI64 = AtomicI64::new(0);
            let cur = super::with_ctx(|ctx| ctx.shared.latest().map(|s| s.t))
                .flatten()
                .unwrap_or(0);
            if cur != LAST_T.swap(cur, Ordering::Relaxed) {
                relayout(hwnd); // 磁盘数量变化时需要重排
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
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

/// 右键菜单
unsafe fn show_menu(hwnd: HWND) {
    let rec = super::with_ctx(|ctx| {
        matches!(*ctx.shared.status.lock().unwrap(), Status::Recording { .. })
    })
    .unwrap_or(false);

    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let _ = AppendMenuW(menu, MF_STRING, super::CMD_SETTINGS as usize, PCWSTR(util::wide("设置(&S)").as_ptr()));
    // 按当前显隐状态给出正确菜单项，方便隐藏后重新显示
    let visible_now = super::with_ctx(|ctx| ctx.config.overlay_visible).unwrap_or(true);
    let toggle_text = if visible_now { "隐藏悬浮窗(&H)" } else { "显示悬浮窗(&H)" };
    let _ = AppendMenuW(menu, MF_STRING, super::CMD_TOGGLE_OVERLAY as usize, PCWSTR(util::wide(toggle_text).as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::w!(""));
    if rec {
        let _ = AppendMenuW(menu, MF_STRING, super::CMD_STOP_RECORD as usize, PCWSTR(util::wide("停止记录(&T)").as_ptr()));
    } else {
        let _ = AppendMenuW(menu, MF_STRING, super::CMD_START_RECORD as usize, PCWSTR(util::wide("开始记录(&R)").as_ptr()));
    }
    let _ = AppendMenuW(menu, MF_STRING, super::CMD_REPORT as usize, PCWSTR(util::wide("生成分析报告(&G)").as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, super::CMD_OPEN_DIR as usize, PCWSTR(util::wide("打开日志目录(&O)").as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::w!(""));
    let _ = AppendMenuW(menu, MF_STRING, super::CMD_EXIT as usize, PCWSTR(util::wide("退出(&X)").as_ptr()));

    let mut pt = windows::Win32::Foundation::POINT::default();
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

/// 绘制悬浮窗内容（内存双缓冲：先画到缓存的内存位图，再一次性贴到屏幕，消除刷新闪烁；
/// 内存 DC/位图/画刷/字体全部缓存复用，见文件顶部 static）
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    if w <= 0 || h <= 0 {
        let _ = EndPaint(hwnd, &mut ps);
        return;
    }

    let mem = mem_dc(hdc, w, h);

    // 1. 用透明色打底（配合 LWA_COLORKEY）
    FillRect(mem, &rc, HBRUSH(brush(&BRUSH_KEY, color(255, 0, 255)).0));

    // 2. 圆角深色面板
    let panel = brush(&BRUSH_PANEL, color(28, 32, 40));
    let old = SelectObject(mem, HGDIOBJ(panel.0));
    let _ = RoundRect(mem, 0, 0, rc.right, rc.bottom, 14, 14);
    SelectObject(mem, old);
    let border = brush(&BRUSH_BORDER, color(72, 82, 98));
    let inner = RECT {
        left: 0,
        top: 0,
        right: rc.right,
        bottom: rc.bottom,
    };
    FrameRect(mem, &inner, HBRUSH(border.0));

    SetBkMode(mem, TRANSPARENT);

    // 3. 标题行
    SelectObject(mem, HGDIOBJ(font_bold().0));
    SetTextColor(mem, color(126, 176, 255));
    let _ = TextOutW(mem, PAD, 7, util::wide("WinMon").as_slice());

    let status = super::with_ctx(|ctx| match &*ctx.shared.status.lock().unwrap() {
        Status::Recording { rows, .. } => format!("● 记录中 {}", rows),
        Status::Error(msg) => format!("● 记录错误：{}", msg),
        Status::Idle => "○ 未记录".to_string(),
    })
    .unwrap_or_default();
    SelectObject(mem, HGDIOBJ(font_normal().0));
    SetTextColor(mem, color(150, 158, 172));
    let mut trc = RECT {
        left: 0,
        top: 6,
        right: rc.right - PAD,
        bottom: 24,
    };
    let mut stext: Vec<u16> = util::wide(&status);
    DrawTextW(mem, &mut stext, &mut trc, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);

    // 4. 指标行
    let rows = super::with_ctx(|ctx| {
        let s = ctx.shared.latest();
        Row::build(&ctx.config.display_metrics, s.as_ref())
    })
    .unwrap_or_default();

    let mut y = HEADER_H;
    for row in rows {
        SelectObject(mem, HGDIOBJ(font_normal().0));
        SetTextColor(mem, color(158, 166, 180));
        let _ = TextOutW(mem, PAD, y + 4, util::wide(&row.label).as_slice());

        let vc = match row.pct {
            Some(p) if p >= 85.0 => color(255, 120, 100),
            Some(p) if p >= 60.0 => color(255, 196, 110),
            _ => color(235, 238, 244),
        };
        SetTextColor(mem, vc);
        let mut vrc = RECT {
            left: 0,
            top: y + 1,
            right: rc.right - PAD,
            bottom: y + ROW_H - 1,
        };
        let mut vtext: Vec<u16> = util::wide(&row.value);
        DrawTextW(mem, &mut vtext, &mut vrc, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
        y += ROW_H;
    }

    // 一次性贴到屏幕；内存 DC 与位图保留在缓存里复用，不删除
    let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);

    let _ = EndPaint(hwnd, &mut ps);
}
