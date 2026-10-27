//! 设置窗口：桌面显示指标、记录指标、采集间隔、日志目录

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL, CLIP_DEFAULT_PRECIS, HFONT,
    OUT_DEFAULT_PRECIS, ANTIALIASED_QUALITY, COLOR_BTNFACE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, BST_CHECKED,
};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDlgItem, GetWindowTextLengthW,
    LoadCursorW, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowTextW, ShowWindow,
    BM_GETCHECK, BM_SETCHECK, CW_USEDEFAULT, HMENU, IDC_ARROW, SW_SHOWNORMAL,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_GETTEXT, WM_SETFONT,
    WNDCLASSW, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_GROUPBOX, WINDOW_EX_STYLE, WINDOW_STYLE,
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

static mut FONT: HFONT = HFONT(std::ptr::null_mut());

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
            585,
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
        FONT = CreateFontW(
            -13, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY,
            FF_DONTCARE.0 as u32, PCWSTR(util::wide("Microsoft YaHei UI").as_ptr()),
        );
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
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                (COLOR_BTNFACE.0 + 1) as *mut core::ffi::c_void,
            ),
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
    SendMessageW(h, WM_SETFONT, Some(WPARAM(FONT.0 as usize)), Some(LPARAM(1)));
    h
}

unsafe fn build_controls(parent: HWND) {
    // ---- 桌面显示指标 ----
    // 14 个指标用 3 列 × 5 行排列（列距 138、宽 130，刚好放进 430 宽的分组框）
    child(parent, "BUTTON", "桌面显示指标（显示在悬浮窗上，可多选）", BS_GROUPBOX as u32, 12, 10, 430, 132, 1);
    for (i, m) in ALL_METRICS.iter().enumerate() {
        let col = (i / 5) as i32;
        let row = (i % 5) as i32;
        let h = child(
            parent,
            "BUTTON",
            m.label(),
            BS_AUTOCHECKBOX as u32,
            26 + col * 138,
            34 + row * 18,
            130,
            18,
            IDC_DISP_BASE + i as u32,
        );
        set_check(h, false);
    }

    // ---- 记录指标 ----
    child(parent, "BUTTON", "记录指标（写入 CSV 的列，可多选）", BS_GROUPBOX as u32, 12, 150, 430, 132, 2);
    for (i, m) in ALL_METRICS.iter().enumerate() {
        let col = (i / 5) as i32;
        let row = (i % 5) as i32;
        let h = child(
            parent,
            "BUTTON",
            m.label(),
            BS_AUTOCHECKBOX as u32,
            26 + col * 138,
            174 + row * 18,
            130,
            18,
            IDC_REC_BASE + i as u32,
        );
        set_check(h, true);
    }

    // ---- 采集设置 ----
    child(parent, "BUTTON", "采集与记录设置", BS_GROUPBOX as u32, 12, 290, 430, 118, 3);
    child(parent, "STATIC", "桌面刷新间隔(秒)", 0, 26, 314, 130, 18, 4);
    child(parent, "EDIT", "", WS_TABSTOP.0, 156, 311, 70, 22, IDC_EDIT_REFRESH);
    child(parent, "STATIC", "记录间隔(秒)", 0, 250, 314, 100, 18, 5);
    child(parent, "EDIT", "", WS_TABSTOP.0, 350, 311, 70, 22, IDC_EDIT_REC_INTERVAL);
    child(parent, "STATIC", "日志目录（采集数据 CSV 与分析报告的存放位置）", 0, 26, 342, 400, 18, 6);
    child(parent, "EDIT", "", WS_TABSTOP.0, 26, 362, 318, 22, IDC_EDIT_DIR);
    child(parent, "BUTTON", "浏览...", WS_TABSTOP.0, 352, 361, 72, 24, IDC_BTN_BROWSE);
    child(parent, "BUTTON", "启动时自动开始记录", BS_AUTOCHECKBOX as u32, 26, 388, 200, 18, IDC_CHK_AUTOSTART);

    // ---- 动作按钮 ----
    child(parent, "BUTTON", "开始记录", WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32, 12, 424, 102, 28, super::CMD_START_RECORD);
    child(parent, "BUTTON", "停止记录", WS_TABSTOP.0, 120, 424, 102, 28, super::CMD_STOP_RECORD);
    child(parent, "BUTTON", "生成分析报告", WS_TABSTOP.0, 228, 424, 102, 28, super::CMD_REPORT);
    child(parent, "BUTTON", "打开日志目录", WS_TABSTOP.0, 336, 424, 106, 28, super::CMD_OPEN_DIR);
    child(parent, "BUTTON", "保存设置", WS_TABSTOP.0, 12, 462, 102, 28, 2300);
    child(parent, "BUTTON", "退出程序", WS_TABSTOP.0, 120, 462, 102, 28, 2301);
    child(parent, "STATIC", "", 0, 12, 502, 430, 20, IDC_STATUS);
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
            LRESULT(0)
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
