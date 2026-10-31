//! 设置窗口：桌面显示指标、记录指标、采集间隔、日志目录

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawFocusRect, DrawTextW, RoundRect,
    SelectObject, SetBkColor, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT,
    HGDIOBJ, OUT_DEFAULT_PRECIS, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, BST_CHECKED, DRAWITEMSTRUCT,
    ODS_DISABLED, ODS_FOCUS, ODS_SELECTED, SetWindowTheme,
};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDlgItem, GetWindowLongPtrW, GetWindowTextLengthW,
    LoadCursorW, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowTextW, ShowWindow,
    BM_GETCHECK, BM_SETCHECK, BS_AUTO3STATE, BS_AUTOCHECKBOX, BS_CHECKBOX, BS_OWNERDRAW,
    BS_TYPEMASK, CW_USEDEFAULT, GWL_ID, GWL_STYLE, HMENU, IDC_ARROW, SW_SHOWNORMAL, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DRAWITEM, WM_GETTEXT,
    WM_SETFONT, WNDCLASSW, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE, BS_GROUPBOX, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{PCWSTR, PWSTR};

use crate::config::Config;
use crate::metrics::{Metric, ALL_METRICS};
use crate::util;

const CLASS_NAME: &str = "WinMonSettings";

const IDC_DISP_BASE: u32 = 2000;
const IDC_REC_BASE: u32 = 2100;
const IDC_EDIT_REFRESH: u32 = 2200;
const IDC_EDIT_REC_INTERVAL: u32 = 2201;
const IDC_EDIT_DIR: u32 = 2202;
const IDC_BTN_BROWSE: u32 = 2203;
const IDC_CHK_AUTOSTART: u32 = 2204;
const IDC_STATUS: u32 = 2205;

// GDI 字体缓存：创建一次复用（仅 UI 线程访问，Send/Sync 手工声明只为放进静态变量）
struct FontHandle(HFONT);
unsafe impl Send for FontHandle {}
unsafe impl Sync for FontHandle {}

struct BrushHandle(HBRUSH);
unsafe impl Send for BrushHandle {}
unsafe impl Sync for BrushHandle {}

static FONT: OnceLock<FontHandle> = OnceLock::new();
static BRUSH_BG: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_CARD: OnceLock<BrushHandle> = OnceLock::new();
static BRUSH_EDIT: OnceLock<BrushHandle> = OnceLock::new();

fn color(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// 让设置窗口的标题栏（非客户区）也变深色，与悬浮窗深暖灰主体一致
fn enable_dark_titlebar(hwnd: HWND) {
    unsafe {
        // 深色标题栏 + 白字（Win10 1903+，Build 19045 支持）
        let enable: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &enable as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        // 标题栏底色设成与主体一致的深暖灰，避免默认纯黑太生硬
        let c = color(44, 42, 41);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &c as *const COLORREF as *const core::ffi::c_void,
            std::mem::size_of::<COLORREF>() as u32,
        );
    }
}

fn font() -> HFONT {
    FONT.get_or_init(|| {
        FontHandle(unsafe {
            CreateFontW(
                -13, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY,
                FF_DONTCARE.0 as u32, PCWSTR(util::wide("Microsoft YaHei UI").as_ptr()),
            )
        })
    }).0
}

/// 深暖灰背景（与悬浮窗深玻璃主题一致），窗口与静态文本/分组框/复选按钮共用
fn bg_brush() -> HBRUSH {
    BRUSH_BG
        .get_or_init(|| {
            BrushHandle(unsafe {
                CreateSolidBrush(COLORREF(0x2C_2A_29)) // RGB(44,42,41)
            })
        })
        .0
}

/// 判断按钮控件是否为复选类（复选/自动复选/三态），决定文字用白色还是深色
unsafe fn is_checkbox(h: HWND) -> bool {
    let style = GetWindowLongPtrW(h, GWL_STYLE) as i32;
    let kind = style & BS_TYPEMASK;
    kind == BS_CHECKBOX || kind == BS_AUTOCHECKBOX || kind == BS_AUTO3STATE
}

/// 分组卡片底色：比窗口底亮一档，让三个分组区域从深底里"浮"出一层（层次感的来源）
fn card_brush() -> HBRUSH {
    BRUSH_CARD
        .get_or_init(|| {
            BrushHandle(unsafe {
                CreateSolidBrush(COLORREF(0x2F_31_34)) // RGB(52,49,47)，比窗口底 (44,42,41) 亮一档
            })
        })
        .0
}

/// 输入框底色：比卡片再亮一档（RGB(60,58,56)），突出输入区；
/// 与 WM_CTLCOLOREDIT 里 SetBkColor 保持一致，否则文字底色块
fn edit_brush() -> HBRUSH {
    BRUSH_EDIT
        .get_or_init(|| {
            BrushHandle(unsafe {
                CreateSolidBrush(COLORREF(0x38_3A_3C)) // RGB(60,58,56)
            })
        })
        .0
}

// 窗口创建期间使用的配置快照：
// CreateWindowExW 会**同步**派发 WM_CREATE（其中的 fill_from_config 要读配置），
// 而 open() 此时若持有全局上下文锁就会被自己锁死，因此改成创建前先取快照。
thread_local! {
    static PENDING_CONFIG: std::cell::RefCell<Option<Config>> = const {
        std::cell::RefCell::new(None)
    };
}

/// 打开设置窗口（已打开则前置）
pub fn open() {
    // 已打开则前置（窗口操作一律在锁外做）
    let existing = super::with_ctx(|ctx| ctx.settings_hwnd).flatten();
    if let Some(h) = existing {
        unsafe {
            let _ = SetForegroundWindow(h);
            let _ = ShowWindow(h, SW_SHOWNORMAL);
        }
        return;
    }

    let hinstance = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    // 配置快照必须在建窗前取好：WM_CREATE 里不能再回头拿上下文锁
    let snapshot = super::with_ctx(|ctx| ctx.config.clone());
    PENDING_CONFIG.with(|p| *p.borrow_mut() = snapshot);

    let cls = util::wide(CLASS_NAME);
    let title = util::wide("WinMon 设置");
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            470,
            660,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    };
    match hwnd {
        Ok(h) => {
            super::with_ctx(|ctx| ctx.settings_hwnd = Some(h));
            unsafe {
                let _ = ShowWindow(h, SW_SHOWNORMAL);
                let _ = SetForegroundWindow(h);
            }
        }
        // 提示框会起模态消息循环，必须放在锁外
        Err(e) => super::msg_error(&format!("创建设置窗口失败：{}", e)),
    }
}

/// 注册设置窗口类
pub fn register(hinstance: windows::Win32::Foundation::HINSTANCE) {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let cls = util::wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: super::app_icon(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: bg_brush(),
            lpszMenuName: windows::core::w!(""),
            lpszClassName: PCWSTR(cls.as_ptr()),
        };
        RegisterClassW(&wc);
    }
}

/// 处理设置窗口自身的命令；返回是否已处理
pub fn handle_command(cmd: u32) -> bool {
    match cmd {
        IDC_BTN_BROWSE => {
            browse_dir();
            true
        }
        _ => false,
    }
}

unsafe fn child(parent: HWND, class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: u32) -> HWND {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();
    let cls = util::wide(class);
    let txt = util::wide(text);
    let ex = if class == "EDIT" {
        WS_EX_CLIENTEDGE
    } else {
        WINDOW_EX_STYLE(0)
    };
    let h = CreateWindowExW(
        ex,
        PCWSTR(cls.as_ptr()),
        PCWSTR(txt.as_ptr()),
        WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as *mut core::ffi::c_void)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(h, WM_SETFONT, Some(WPARAM(font().0 as usize)), Some(LPARAM(1)));
    // 禁用视觉主题：带主题时按钮/编辑框会忽略 WM_CTLCOLOR 返回的深色画刷，
    // 关掉后经典样式 + 深底画刷才能统一接管，整窗配色不再"只换背景"
    if class == "BUTTON" || class == "EDIT" {
        let _ = SetWindowTheme(h, windows::core::w!(""), windows::core::w!(""));
    }
    h
}

unsafe fn build_controls(parent: HWND) {
    // ---- 桌面显示指标 ----
    // 14 个指标 3 列 × 5 行（列距 138、行距 22，组内上下留白 ≥12px）
    child(parent, "BUTTON", "桌面显示指标（显示在悬浮窗上，可多选）", BS_GROUPBOX as u32, 12, 10, 430, 148, 1);
    for (i, m) in ALL_METRICS.iter().enumerate() {
        let col = (i / 5) as i32;
        let row = (i % 5) as i32;
        let h = child(
            parent,
            "BUTTON",
            m.label(),
            BS_AUTOCHECKBOX as u32,
            26 + col * 138,
            40 + row * 22,
            130,
            18,
            IDC_DISP_BASE + i as u32,
        );
        set_check(h, false);
    }

    // ---- 记录指标 ----
    child(parent, "BUTTON", "记录指标（写入 CSV 的列，可多选）", BS_GROUPBOX as u32, 12, 170, 430, 148, 2);
    for (i, m) in ALL_METRICS.iter().enumerate() {
        let col = (i / 5) as i32;
        let row = (i % 5) as i32;
        let h = child(
            parent,
            "BUTTON",
            m.label(),
            BS_AUTOCHECKBOX as u32,
            26 + col * 138,
            200 + row * 22,
            130,
            18,
            IDC_REC_BASE + i as u32,
        );
        set_check(h, true);
    }

    // ---- 采集设置（行距放宽：标题区 32px、目录区 22px/22px、自动记录 30px） ----
    child(parent, "BUTTON", "采集与记录设置", BS_GROUPBOX as u32, 12, 330, 430, 150, 3);
    child(parent, "STATIC", "桌面刷新间隔(秒)", 0, 26, 362, 130, 18, 4);
    child(parent, "EDIT", "", WS_TABSTOP.0, 158, 359, 70, 22, IDC_EDIT_REFRESH);
    child(parent, "STATIC", "记录间隔(秒)", 0, 250, 362, 90, 18, 5);
    child(parent, "EDIT", "", WS_TABSTOP.0, 344, 359, 72, 22, IDC_EDIT_REC_INTERVAL);
    child(parent, "STATIC", "日志目录（采集数据 CSV 与分析报告的存放位置）", 0, 26, 394, 400, 18, 6);
    child(parent, "EDIT", "", WS_TABSTOP.0, 26, 416, 320, 22, IDC_EDIT_DIR);
    // 浏览按钮与左侧目录输入框垂直居中（两者中线同为 y=427）
    child(parent, "BUTTON", "浏览...", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 354, 415, 76, 24, IDC_BTN_BROWSE);
    child(parent, "BUTTON", "启动时自动开始记录", BS_AUTOCHECKBOX as u32, 26, 448, 200, 18, IDC_CHK_AUTOSTART);

    // ---- 动作按钮（2 行 × 3 个均匀网格：x=12/158/304，宽 138，列完全对齐） ----
    child(parent, "BUTTON", "开始记录", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 12, 496, 138, 28, super::CMD_START_RECORD);
    child(parent, "BUTTON", "停止记录", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 158, 496, 138, 28, super::CMD_STOP_RECORD);
    child(parent, "BUTTON", "生成分析报告", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 304, 496, 138, 28, super::CMD_REPORT);
    child(parent, "BUTTON", "打开日志目录", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 12, 534, 138, 28, super::CMD_OPEN_DIR);
    child(parent, "BUTTON", "保存设置", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 158, 534, 138, 28, 2300);
    child(parent, "BUTTON", "退出程序", BS_OWNERDRAW as u32 | WS_TABSTOP.0, 304, 534, 138, 28, 2301);
    child(parent, "STATIC", "", 0, 12, 574, 430, 20, IDC_STATUS);
}

unsafe fn set_check(h: HWND, checked: bool) {
    SendMessageW(
        h,
        BM_SETCHECK,
        Some(WPARAM(if checked { BST_CHECKED.0 as usize } else { 0 })),
        Some(LPARAM(0)),
    );
}

unsafe fn get_check(id: u32, parent: HWND) -> bool {
    let h = GetDlgItem(Some(parent), id as i32).unwrap_or_default();
    let r = SendMessageW(h, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0)));
    r.0 == BST_CHECKED.0 as isize
}

unsafe fn get_text(parent: HWND, id: u32) -> String {
    let h = GetDlgItem(Some(parent), id as i32).unwrap_or_default();
    let len = GetWindowTextLengthW(h) as usize;
    let mut buf = vec![0u16; len + 2];
    let n = SendMessageW(
        h,
        WM_GETTEXT,
        Some(WPARAM(buf.len())),
        Some(LPARAM(buf.as_mut_ptr() as isize)),
    );
    buf.truncate(n.0 as usize);
    util::wide_to_string(&buf)
}

unsafe fn set_text(parent: HWND, id: u32, text: &str) {
    let h = GetDlgItem(Some(parent), id as i32).unwrap_or_default();
    let _ = SetWindowTextW(h, PCWSTR(util::wide(text).as_ptr()));
}

/// 从界面读取全部设置并应用
unsafe fn apply(parent: HWND) {
    // 1. 界面取值（纯窗口操作，不持锁）
    let disp = selected(parent, IDC_DISP_BASE);
    let rec = selected(parent, IDC_REC_BASE);
    let refresh = get_text(parent, IDC_EDIT_REFRESH);
    let interval = get_text(parent, IDC_EDIT_REC_INTERVAL);
    let dir = get_text(parent, IDC_EDIT_DIR).trim().to_string();
    let autostart = get_check(IDC_CHK_AUTOSTART, parent);

    // 2. 一次性写回配置，并取出后续需要的句柄（不在此处做窗口操作）
    let taken = super::with_ctx(|ctx| {
        let cfg = &mut ctx.config;
        cfg.display_metrics = disp;
        cfg.record_metrics = rec;
        cfg.refresh_secs = refresh.trim().parse::<u32>().unwrap_or(cfg.refresh_secs).clamp(1, 3600);
        cfg.record_interval_secs = interval
            .trim()
            .parse::<u32>()
            .unwrap_or(cfg.record_interval_secs)
            .clamp(1, 86400);
        if !dir.is_empty() {
            cfg.record_dir = dir;
        }
        cfg.autostart_record = autostart;
        (ctx.shared.clone(), ctx.overlay_hwnd, cfg.refresh_secs)
    });

    // 3. 锁外：刷新采样间隔、重排悬浮窗（两者内部都要读上下文）
    if let Some((shared, hwnd, refresh_secs)) = taken {
        shared
            .refresh_secs
            .store(refresh_secs, std::sync::atomic::Ordering::Relaxed);
        super::overlay::relayout(hwnd);
        windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(hwnd),
            super::TIMER_REFRESH,
            (refresh_secs * 1000).max(500),
            None,
        );
    }
    super::save_config();
    set_status(parent, "设置已保存。");
}

unsafe fn selected(parent: HWND, base: u32) -> Vec<Metric> {
    ALL_METRICS
        .iter()
        .enumerate()
        .filter(|(i, _)| get_check(base + *i as u32, parent))
        .map(|(_, m)| *m)
        .collect()
}

unsafe fn set_status(parent: HWND, text: &str) {
    set_text(parent, IDC_STATUS, text);
}

/// 读按钮窗口文本（null 结尾 UTF-16，供自绘按钮 DrawText 用）
unsafe fn read_button_text(h: HWND) -> Vec<u16> {
    let len = GetWindowTextLengthW(h) as usize;
    let mut buf = vec![0u16; len + 2];
    let n = SendMessageW(
        h,
        WM_GETTEXT,
        Some(WPARAM(buf.len())),
        Some(LPARAM(buf.as_mut_ptr() as isize)),
    );
    // 截断到"实际文本 + 1 个 null"：尾随多余的 0 会让 DrawTextW 的 DT_CENTER
    // 测宽偏移，文字看起来偏左
    buf.truncate(n.0 as usize + 1);
    buf
}

/// owner-draw 按钮：深色圆角底 + 白字，按下/禁用有反馈，主/次/危险用语义色区分
unsafe fn draw_button(di: &DRAWITEMSTRUCT) {
    let hdc = di.hDC;
    let rc = &di.rcItem;
    let state = di.itemState.0;
    let pressed = state & ODS_SELECTED.0 != 0;
    let disabled = state & ODS_DISABLED.0 != 0;
    let focused = state & ODS_FOCUS.0 != 0;

    // 默认：深灰底白字（比卡片/输入框更亮一档，提示"可点"）
    let mut bg = if pressed { color(88, 84, 80) } else { color(70, 67, 64) };
    let mut border = color(104, 100, 96);
    let mut fg = color(242, 240, 238);
    if disabled {
        bg = color(50, 48, 47);
        border = color(70, 68, 66);
        fg = color(122, 120, 118);
    }
    // 语义色只留一个：退出程序 = 破坏性操作用暗红（iOS 惯例）；
    // 其余全部统一中性深灰，避免多色混杂（2绿+1红+3灰显得杂乱）
    match di.CtlID {
        2301 => {
            // 退出程序：暗红
            bg = if pressed { color(118, 50, 46) } else { color(146, 62, 56) };
            border = color(170, 82, 74);
            fg = color(255, 255, 255);
        }
        _ => {}
    }

    // 圆角底 + 描边
    let br = CreateSolidBrush(bg);
    let pen = CreatePen(PS_SOLID, 1, border);
    let old_br = SelectObject(hdc, HGDIOBJ(br.0));
    let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, 8, 8);
    SelectObject(hdc, old_br);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(HGDIOBJ(br.0));
    let _ = DeleteObject(HGDIOBJ(pen.0));

    // 键盘焦点：内缩虚线框（深底上呈亮虚线）
    if focused {
        let fr = RECT {
            left: rc.left + 3,
            top: rc.top + 3,
            right: rc.right - 3,
            bottom: rc.bottom - 3,
        };
        let _ = DrawFocusRect(hdc, &fr);
    }

    // 文字居中（整体下移 1px：中文 YaHei 在 DT_VCENTER 下视觉重心偏上，做视觉居中修正）
    let text = read_button_text(di.hwndItem);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, fg);
    SelectObject(hdc, HGDIOBJ(font().0));
    let mut tr = RECT {
        left: rc.left,
        top: rc.top + 1,
        right: rc.right,
        bottom: rc.bottom + 1,
    };
    let mut t = text;
    let _ = DrawTextW(hdc, &mut t, &mut tr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
}

unsafe fn fill_from_config(parent: HWND) {
    // 不访问全局上下文：本函数由 WM_CREATE 同步触发，外层可能正持有该锁（重入会死锁）
    let Some(cfg) = PENDING_CONFIG.with(|p| p.borrow().clone()) else {
        return;
    };
    for (i, m) in ALL_METRICS.iter().enumerate() {
        set_check(
            GetDlgItem(Some(parent), (IDC_DISP_BASE + i as u32) as i32).unwrap_or_default(),
            cfg.display_metrics.contains(m),
        );
        set_check(
            GetDlgItem(Some(parent), (IDC_REC_BASE + i as u32) as i32).unwrap_or_default(),
            cfg.record_metrics.contains(m),
        );
    }
    set_text(parent, IDC_EDIT_REFRESH, &format!("{}", cfg.refresh_secs));
    set_text(parent, IDC_EDIT_REC_INTERVAL, &format!("{}", cfg.record_interval_secs));
    set_text(parent, IDC_EDIT_DIR, &cfg.record_dir);
    set_check(
        GetDlgItem(Some(parent), IDC_CHK_AUTOSTART as i32).unwrap_or_default(),
        cfg.autostart_record,
    );
}

/// 选择日志目录
fn browse_dir() {
    let mut display = [0u16; 260];
    let title = util::wide("选择日志目录");
    let mut info = BROWSEINFOW::default();
    info.pszDisplayName = PWSTR(display.as_mut_ptr());
    info.lpszTitle = PCWSTR(title.as_ptr());
    info.ulFlags = BIF_RETURNONLYFSDIRS;
    unsafe {
        let pidl = SHBrowseForFolderW(&info);
        if pidl.is_null() {
            return;
        }
        let mut path = [0u16; 260];
        if SHGetPathFromIDListW(pidl, &mut path).as_bool() {
            let s = util::wide_to_string(&path);
            super::with_ctx(|ctx| {
                if let Some(h) = ctx.settings_hwnd {
                    set_text(h, IDC_EDIT_DIR, &s);
                }
            });
        }
    }
}

pub(super) unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            build_controls(hwnd);
            fill_from_config(hwnd);
            enable_dark_titlebar(hwnd);
            LRESULT(0)
        }
        // 静态文本/分组框标题白字；分组区域内用卡片底，底部状态栏用窗口底
        WM_CTLCOLORSTATIC => {
            let h = HWND(lparam.0 as *mut core::ffi::c_void);
            let id = GetWindowLongPtrW(h, GWL_ID) as u32;
            unsafe {
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut core::ffi::c_void);
                SetTextColor(hdc, COLORREF(0xEC_EA_E8)); // RGB(232,234,236)
                SetBkMode(hdc, TRANSPARENT);
            }
            if id == IDC_STATUS {
                LRESULT(bg_brush().0 as isize)
            } else {
                LRESULT(card_brush().0 as isize)
            }
        }
        // 复选按钮白字 + 卡片底（复选都在分组区域内；owner-draw 普通按钮不走这条）
        WM_CTLCOLORBTN => {
            unsafe {
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut core::ffi::c_void);
                SetBkMode(hdc, TRANSPARENT);
                if is_checkbox(HWND(lparam.0 as *mut core::ffi::c_void)) {
                    SetTextColor(hdc, COLORREF(0xEC_EA_E8));
                } else {
                    SetTextColor(hdc, COLORREF(0x22_24_26));
                }
            }
            LRESULT(card_brush().0 as isize)
        }
        // 输入框：深底白字（底色与 edit_brush 一致）
        WM_CTLCOLOREDIT => {
            unsafe {
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut core::ffi::c_void);
                SetTextColor(hdc, COLORREF(0xF0_F0_F0));
                SetBkColor(hdc, COLORREF(0x38_3A_3C)); // RGB(60,58,56)
            }
            LRESULT(edit_brush().0 as isize)
        }
        // owner-draw 按钮：深色圆角自绘
        WM_DRAWITEM => {
            unsafe {
                let di = lparam.0 as *mut DRAWITEMSTRUCT;
                draw_button(&*di);
            }
            LRESULT(1)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            match id {
                IDC_BTN_BROWSE => {
                    browse_dir();
                }
                2300 => {
                    apply(hwnd);
                }
                2301 => {
                    super::exit_app();
                }
                _ => {
                    // 记录/报告等命令：先应用设置再执行
                    if id == super::CMD_START_RECORD || id == super::CMD_STOP_RECORD {
                        apply(hwnd);
                    }
                    super::handle_command(id, wparam);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            DestroyWindow(hwnd).ok();
            super::with_ctx(|ctx| ctx.settings_hwnd = None);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
