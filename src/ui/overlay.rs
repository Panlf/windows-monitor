//! 桌面悬浮窗：毛玻璃半透明置顶面板，按类别分层显示指标，可拖动、右键菜单

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint,
    FillRect, FrameRect, InvalidateRect, RoundRect, SelectObject, SetBkMode, SetTextColor,
    SetWindowRgn, TextOutW, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_LEFT,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, FW_NORMAL, HBITMAP, HBRUSH, HDC,
    HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_SELECTED};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetClientRect,
    GetCursorPos, GetWindowRect, LoadCursorW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetLayeredWindowAttributes, SetMenuInfo, SetTimer, SetWindowPos,
    ShowWindow, TrackPopupMenu, HTCAPTION, HMENU, IDC_ARROW, LWA_ALPHA, LWA_COLORKEY, MENUINFO,
    MIM_BACKGROUND, MF_OWNERDRAW, SW_HIDE, SW_SHOW, SWP_NOMOVE, SWP_NOACTIVATE, SWP_NOZORDER,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_DRAWITEM, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MEASUREITEM,
    WM_NCLBUTTONDOWN, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCSTR, PCWSTR};

use crate::metrics::{Metric, Sample, ALL_METRICS, CATEGORY_ORDER};
use crate::sample_row::Row;
use crate::sampler::Status;
use crate::util;

const WIDTH: i32 = 280;
const PAD: i32 = 14;
const HEADER_H: i32 = 38; // 顶部标题行（含状态点）
const SECTION_TITLE_H: i32 = 18;
const ROW_H: i32 = 20;
const LINE_GAP: i32 = 12; // 组间间距（含分隔线）
const ROUND_R: i32 = 16; // 圆角半径
const DOT: i32 = 8; // 状态点直径
const SEP_ID: u32 = 0xFFFF; // owner-draw 菜单分隔线 ID

// 背景模式：亚克力毛玻璃 / colorkey 圆角透明 / 深色矩形兜底
const BD_ACRYLIC: u8 = 1;
const BD_COLORKEY: u8 = 2;
const BD_OPAQUE: u8 = 3;
static BACKDROP: AtomicU8 = AtomicU8::new(BD_COLORKEY);

// 状态点取值
const ST_IDLE: u8 = 0;
const ST_REC: u8 = 1;
const ST_ERR: u8 = 2;

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

static FONT_TITLE: OnceLock<FontHandle> = OnceLock::new();
static FONT_SECTION: OnceLock<FontHandle> = OnceLock::new();
static FONT_LABEL: OnceLock<FontHandle> = OnceLock::new();
static FONT_VALUE: OnceLock<FontHandle> = OnceLock::new();
static BRUSH_KEY: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_PANEL: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_BORDER: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_MENU_BG: OnceLock<BrushHandle> = OnceLock::new();
static MEM_DC: Mutex<Option<MemDc>> = Mutex::new(None);

fn make_font(size: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            size, 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY,
            FF_DONTCARE.0 as u32, PCWSTR(util::wide("Segoe UI").as_ptr()),
        )
    }
}

fn font_title() -> HFONT {
    FONT_TITLE
        .get_or_init(|| FontHandle(make_font(-14, FW_BOLD.0 as i32)))
        .0
}
fn font_section() -> HFONT {
    FONT_SECTION
        .get_or_init(|| FontHandle(make_font(-11, FW_NORMAL.0 as i32)))
        .0
}
fn font_label() -> HFONT {
    FONT_LABEL
        .get_or_init(|| FontHandle(make_font(-12, FW_NORMAL.0 as i32)))
        .0
}
fn font_value() -> HFONT {
    FONT_VALUE
        .get_or_init(|| FontHandle(make_font(-12, FW_BOLD.0 as i32)))
        .0
}

fn brush(get: &OnceLock<BrushHandle>, c: COLORREF) -> HBRUSH {
    get.get_or_init(|| BrushHandle(unsafe { CreateSolidBrush(c) })).0
}

fn key_brush() -> HBRUSH {
    brush(&BRUSH_KEY, color(255, 0, 255))
}
fn panel_brush() -> HBRUSH {
    brush(&BRUSH_PANEL, color(28, 32, 40))
}
fn border_brush() -> HBRUSH {
    brush(&BRUSH_BORDER, color(72, 82, 98))
}
fn menu_bg_brush() -> HBRUSH {
    brush(&BRUSH_MENU_BG, color(30, 35, 46))
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

/// 亚克力毛玻璃背景（SetWindowCompositionAttribute，Win10 1803+ / Win11）
#[repr(C)]
struct AccentPolicy {
    state: i32,
    flags: i32,
    gradient_color: u32, // ABGR
    animation_id: i32,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: i32, // WCA_ACCENT_POLICY = 19
    data: *mut core::ffi::c_void,
    size: usize,
}

// 原始调用 GetProcAddress：windows crate 的封装在部分环境返回 None（见 adl.rs 同类做法）
#[link(name = "kernel32")]
extern "system" {
    fn GetProcAddress(h_module: windows::Win32::Foundation::HMODULE, name: PCSTR)
        -> *mut core::ffi::c_void;
}

/// 取 SetWindowCompositionAttribute 的函数地址（0 表示系统不支持）
fn acrylic_fn_addr() -> usize {
    static FN: OnceLock<usize> = OnceLock::new();
    *FN.get_or_init(|| unsafe {
        let name = windows::core::w!("user32.dll");
        let h = match GetModuleHandleW(name) {
            Ok(h) => h,
            Err(_) => return 0usize,
        };
        GetProcAddress(h, PCSTR(b"SetWindowCompositionAttribute\0".as_ptr())) as usize
    })
}

fn acrylic_api_available() -> bool {
    acrylic_fn_addr() != 0
}

/// 启用亚克力毛玻璃，成功返回 true
unsafe fn apply_acrylic(hwnd: HWND, opacity: u8) -> bool {
    let addr = acrylic_fn_addr();
    if addr == 0 {
        return false;
    }
    type Fn = unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> i32;
    let f: Fn = std::mem::transmute(addr);
    // tint：深蓝灰玻璃，alpha 由配置 opacity 控制（越大越实）
    let gradient = ((opacity as u32) << 24) | 0x2A1E1A; // ABGR: A, B=0x2A, G=0x1E, R=0x1A
    let mut policy = AccentPolicy {
        state: 4, // ACCENT_ENABLE_ACRYLICBLURBEHIND
        flags: 2,
        gradient_color: gradient,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: 19,
        data: &mut policy as *mut AccentPolicy as *mut core::ffi::c_void,
        size: std::mem::size_of::<AccentPolicy>(),
    };
    f(hwnd, &mut data) != 0
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
        // 亚克力需要非 layered 窗口才生效，因此创建前先探测 API 决定窗口扩展样式
        let acrylic_avail = acrylic_api_available();
        let exstyle = if acrylic_avail {
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST
        } else {
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST
        };
        let hwnd = CreateWindowExW(
            exstyle,
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

        if acrylic_avail && apply_acrylic(hwnd, opacity) {
            BACKDROP.store(BD_ACRYLIC, Ordering::Relaxed);
        } else if acrylic_avail {
            // API 存在但应用失败：退化为非 layered 深色矩形面板（无毛玻璃但可用）
            BACKDROP.store(BD_OPAQUE, Ordering::Relaxed);
        } else {
            let _ = SetLayeredWindowAttributes(hwnd, color(255, 0, 255), opacity, LWA_ALPHA | LWA_COLORKEY);
            BACKDROP.store(BD_COLORKEY, Ordering::Relaxed);
        }

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
    let mut h = HEADER_H;
    for (i, s) in sections.iter().enumerate() {
        if i > 0 {
            h += LINE_GAP;
        }
        h += SECTION_TITLE_H + s.rows.len() as i32 * ROW_H;
    }
    h + PAD
}

/// 指标数量变化后调整窗口高度（高度没变时不动窗口也不重绘）
pub fn relayout(hwnd: HWND) {
    let h = super::with_ctx(|ctx| {
        let s = ctx.shared.latest();
        layout_height(&build_sections(&ctx.config.display_metrics, s.as_ref()))
    })
    .unwrap_or(HEADER_H + PAD);
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
            // 非 layered 模式（亚克力/深色矩形）用 region 裁圆角
            if BACKDROP.load(Ordering::Relaxed) != BD_COLORKEY {
                let rgn = CreateRoundRectRgn(0, 0, WIDTH, h, ROUND_R, ROUND_R);
                let _ = SetWindowRgn(hwnd, Some(rgn), true);
            }
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
        WM_ERASEBKGND => {
            // 亚克力模式跳过背景擦除（由 DWM 提供背景），避免闪烁
            if BACKDROP.load(Ordering::Relaxed) == BD_ACRYLIC {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_TIMER => {
            // 数据没变化就不重绘：空闲时跳过整帧绘制，降低常驻 CPU/GDI 开销
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

    // 背景（选中高亮，否则深色）
    let bg = if selected {
        color(58, 68, 88)
    } else {
        color(30, 35, 46)
    };
    let b = CreateSolidBrush(bg);
    FillRect(hdc, rc, b);
    let _ = DeleteObject(HGDIOBJ(b.0));

    // 分隔线
    if di.itemID == SEP_ID {
        let line = CreateSolidBrush(color(58, 68, 88));
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

    if di.itemData == 0 {
        return;
    }
    let text = read_wide_ptr(di.itemData as *const u16);

    let tc = if disabled {
        color(110, 120, 135)
    } else {
        color(232, 237, 245)
    };
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, tc);
    SelectObject(hdc, HGDIOBJ(font_label().0));

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

/// 绘制悬浮窗内容（分组分层 + 状态点；不含背景，背景由亚克力或面板画刷提供）
unsafe fn draw_content(hdc: HDC, w: i32, sections: &[Section], status: u8) {
    SetBkMode(hdc, TRANSPARENT);

    // 标题
    SelectObject(hdc, HGDIOBJ(font_title().0));
    SetTextColor(hdc, color(150, 178, 220));
    let _ = TextOutW(hdc, PAD, 12, util::wide("WinMon").as_slice());

    // 状态点（右上角）：空闲空白、记录中绿点、错误红点
    match status {
        ST_REC => draw_dot(hdc, w - PAD - DOT, (HEADER_H - DOT) / 2, DOT, color(82, 208, 138)),
        ST_ERR => draw_dot(hdc, w - PAD - DOT, (HEADER_H - DOT) / 2, DOT, color(255, 107, 98)),
        _ => {}
    }

    let mut y = HEADER_H;
    for (i, s) in sections.iter().enumerate() {
        if i > 0 {
            // 组间细分隔线
            let line = CreateSolidBrush(color(52, 62, 80));
            let lr = RECT {
                left: PAD,
                top: y + LINE_GAP / 2 - 1,
                right: w - PAD,
                bottom: y + LINE_GAP / 2,
            };
            FillRect(hdc, &lr, line);
            let _ = DeleteObject(HGDIOBJ(line.0));
            y += LINE_GAP;
        }
        // 组标题（弱化小字）
        SelectObject(hdc, HGDIOBJ(font_section().0));
        SetTextColor(hdc, color(120, 134, 156));
        let _ = TextOutW(hdc, PAD, y + 1, util::wide(s.title).as_slice());
        y += SECTION_TITLE_H;

        // 指标行：左标签、右数值
        for row in &s.rows {
            SelectObject(hdc, HGDIOBJ(font_label().0));
            SetTextColor(hdc, color(172, 182, 198));
            let _ = TextOutW(hdc, PAD, y + 2, util::wide(&row.label).as_slice());

            let vc = match row.pct {
                Some(p) if p >= 85.0 => color(255, 143, 135),
                Some(p) if p >= 60.0 => color(255, 201, 138),
                _ => color(232, 237, 245),
            };
            SelectObject(hdc, HGDIOBJ(font_value().0));
            SetTextColor(hdc, vc);
            let mut vrc = RECT {
                left: 0,
                top: y,
                right: w - PAD,
                bottom: y + ROW_H,
            };
            let mut vtext: Vec<u16> = util::wide(&row.value);
            DrawTextW(hdc, &mut vtext, &mut vrc, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
            y += ROW_H;
        }
    }
}

/// 绘制悬浮窗：亚克力模式直接画前景，回退模式用内存双缓冲画面板
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

    let (sections, status) = super::with_ctx(|ctx| {
        let s = ctx.shared.latest();
        let sections = build_sections(&ctx.config.display_metrics, s.as_ref());
        let status = match &*ctx.shared.status.lock().unwrap() {
            Status::Recording { .. } => ST_REC,
            Status::Error(_) => ST_ERR,
            Status::Idle => ST_IDLE,
        };
        (sections, status)
    })
    .unwrap_or_default();

    match BACKDROP.load(Ordering::Relaxed) {
        BD_ACRYLIC => {
            // 背景由 DWM 亚克力提供，直接画前景
            draw_content(hdc, w, &sections, status);
        }
        BD_COLORKEY => {
            // 内存双缓冲 + colorkey 透明 + 圆角深色面板
            let mem = mem_dc(hdc, w, h);
            FillRect(mem, &rc, key_brush());
            let panel = panel_brush();
            let old = SelectObject(mem, HGDIOBJ(panel.0));
            let _ = RoundRect(mem, 0, 0, w, h, 14, 14);
            SelectObject(mem, old);
            FrameRect(mem, &rc, border_brush());
            draw_content(mem, w, &sections, status);
            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        }
        _ => {
            // 深色矩形面板兜底
            let mem = mem_dc(hdc, w, h);
            FillRect(mem, &rc, panel_brush());
            draw_content(mem, w, &sections, status);
            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        }
    }

    let _ = EndPaint(hwnd, &mut ps);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::UpdateWindow;
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

    /// 回归测试：创建悬浮窗（含亚克力/回退背景 + region 圆角）、重排、绘制一轮不崩溃。
    /// 覆盖分组渲染、状态点、菜单外的主绘制路径。
    #[test]
    fn overlay_create_relayout_paint_does_not_crash() {
        unsafe {
            let hmodule = GetModuleHandleW(None).unwrap_or_default();
            let hinstance: windows::Win32::Foundation::HINSTANCE = hmodule.into();
            register(hinstance);
            *crate::ui::CTX.lock().unwrap() = Some(crate::ui::UiCtx {
                shared: crate::sampler::Shared::new(1),
                config: crate::config::Config::default(),
                overlay_hwnd: HWND::default(),
                settings_hwnd: None,
            });

            let hwnd = create(hinstance, 50, 50, 200, true);
            relayout(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            let _ = UpdateWindow(hwnd);
            let _ = DestroyWindow(hwnd);

            *crate::ui::CTX.lock().unwrap() = None;
        }
    }
}
