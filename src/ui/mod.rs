//! 界面公共部分：全局上下文、菜单动作、辅助函数

pub mod overlay;
pub mod settings;
pub mod tray;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, WPARAM};
use windows::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, SW_SHOWNORMAL,
};

use crate::config::Config;
use crate::sampler::{Cmd, Shared};
use crate::util;

/// 托盘图标回调消息
pub const WM_APP_TRAY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;
/// 界面刷新定时器
pub const TIMER_REFRESH: usize = 1;

/// 菜单/按钮命令 ID
pub const CMD_SETTINGS: u32 = 1001;
pub const CMD_TOGGLE_OVERLAY: u32 = 1002;
pub const CMD_START_RECORD: u32 = 1003;
pub const CMD_STOP_RECORD: u32 = 1004;
pub const CMD_REPORT: u32 = 1005;
pub const CMD_OPEN_DIR: u32 = 1006;
pub const CMD_EXIT: u32 = 1007;

pub struct UiCtx {
    pub shared: Arc<Shared>,
    pub config: Config,
    pub overlay_hwnd: HWND,
    pub settings_hwnd: Option<HWND>,
}

// HWND 是裸指针包装，仅主线程访问该上下文，这里手工声明可跨线程存放
unsafe impl Send for UiCtx {}

/// 全局 UI 上下文（主线程访问；采样线程只用 shared）
pub static CTX: Mutex<Option<UiCtx>> = Mutex::new(None);

/// 从 exe 资源加载应用图标（资源 ID 1，见 winmon.rc）；失败回退系统默认图标
pub fn app_icon() -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadIconW, LoadImageW, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE,
    };
    unsafe {
        let exe: windows::Win32::Foundation::HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        match LoadImageW(
            Some(exe),
            windows::core::PCWSTR(1 as _),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE,
        ) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

/// 在上下文上执行一段逻辑
///
/// 约定：**持锁期间不要调用会派发窗口消息的 API**（CreateWindowExW / ShowWindow /
/// MessageBox 等）。这些 API 会同步回调到窗口过程，而窗口过程同样要读上下文，
/// 同一把锁重入会直接把界面线程锁死（表现为程序无响应）。
/// 这里用 try_lock：万一发生重入则跳过本次动作并记录日志，宁可功能降级也不卡死。
pub fn with_ctx<R>(f: impl FnOnce(&mut UiCtx) -> R) -> Option<R> {
    match CTX.try_lock() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => {
            // 只记一次，避免刷屏
            static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                util::debug_log("UI: with_ctx 发生重入，已跳过本次操作以避免死锁");
            }
            None
        }
    }
}

pub fn save_config() {
    with_ctx(|ctx| {
        let path = Config::config_path();
        if let Err(e) = ctx.config.save(&path) {
            msg_error(&e);
        }
    });
}

pub fn msg_info(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(util::wide(text).as_ptr()),
            windows::core::PCWSTR(util::wide("WinMon").as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

pub fn msg_error(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(util::wide(text).as_ptr()),
            windows::core::PCWSTR(util::wide("WinMon").as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 用系统默认程序打开路径（目录/文件）
pub fn shell_open(path: &str) {
    let verb = util::wide("open");
    let file = util::wide(path);
    let mut info = SHELLEXECUTEINFOW::default();
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = windows::core::PCWSTR(verb.as_ptr());
    info.lpFile = windows::core::PCWSTR(file.as_ptr());
    info.nShow = SW_SHOWNORMAL.0 as i32;
    unsafe {
        if ShellExecuteExW(&mut info).is_err() {
            msg_error(&format!("无法打开：{}", path));
        }
    }
}

/// 开始记录
pub fn start_record() {
    with_ctx(|ctx| {
        let cfg = &ctx.config;
        ctx.shared.push_cmd(Cmd::Start {
            dir: cfg.record_dir.clone(),
            interval: cfg.record_interval_secs,
            metrics: cfg.record_metrics.clone(),
        });
    });
}

pub fn stop_record() {
    with_ctx(|ctx| ctx.shared.push_cmd(Cmd::Stop));
}

/// 在途的报告生成线程句柄：退出前统一 join，防止进程退出时 HTML 被写一半
static REPORT_THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// 生成深度分析报告：分析日志目录中最新的 CSV，输出 HTML 并用浏览器打开
pub fn generate_report() {
    // 只取目录字符串，遍历目录（磁盘 IO）放到锁外做，避免阻塞其他 with_ctx 调用
    let Some(dir) = with_ctx(|ctx| ctx.config.record_dir.clone()) else {
        return;
    };
    let Some(csv) = crate::recorder::latest_csv(&dir) else {
        msg_info("日志目录中没有采集数据文件，请先开始记录。");
        return;
    };
    let handle = std::thread::spawn(move || {
        let out = report_path(&dir);
        // 用当前本地时区偏移渲染报告时间，避免硬编码 +8 在其它时区错位
        let offset = util::tz_offset_minutes(util::now_unix());
        match crate::report::generate(&csv, &out, offset) {
            Ok(summary) => {
                shell_open(&out.to_string_lossy());
                msg_info(&format!("{}\n\n报告文件：{}", summary, out.to_string_lossy()));
            }
            Err(e) => msg_error(&format!("生成分析报告失败：{}", e)),
        }
    });
    if let Ok(mut list) = REPORT_THREADS.lock() {
        list.retain(|t| !t.is_finished());
        list.push(handle);
    }
}

/// 退出前等待在途报告线程结束（正常情况下报告很快生成完，不会阻塞退出）
pub fn join_report_threads() {
    let Ok(mut list) = REPORT_THREADS.lock() else { return };
    for h in list.drain(..) {
        let _ = h.join();
    }
}

fn report_path(dir: &str) -> PathBuf {
    PathBuf::from(dir).join(format!(
        "winmon_report_{}.html",
        util::local_time_compact(util::now_unix())
    ))
}

/// 退出程序（保存配置、停止采样线程）
pub fn exit_app() {
    save_config();
    with_ctx(|ctx| {
        ctx.shared.running.store(false, std::sync::atomic::Ordering::Relaxed);
    });
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
    }
}

/// 供子窗口命令转发：处理菜单/按钮命令，返回是否已处理
pub fn handle_command(cmd: u32, wparam: WPARAM) -> bool {
    let _ = wparam;
    match cmd {
        CMD_SETTINGS => {
            settings::open();
            true
        }
        CMD_TOGGLE_OVERLAY => {
            // 只改配置，ShowWindow 放到锁外：它会同步触发重绘，而绘制过程要读上下文
            let (hwnd, show) = with_ctx(|ctx| {
                ctx.config.overlay_visible = !ctx.config.overlay_visible;
                (ctx.overlay_hwnd, ctx.config.overlay_visible)
            })
            .unwrap_or((HWND::default(), false));
            if !hwnd.is_invalid() {
                overlay::set_visible(hwnd, show);
            }
            save_config();
            true
        }
        CMD_START_RECORD => {
            start_record();
            true
        }
        CMD_STOP_RECORD => {
            stop_record();
            true
        }
        CMD_REPORT => {
            generate_report();
            true
        }
        CMD_OPEN_DIR => {
            // 只取目录，实际打开动作放到锁外（Shell 调用失败会弹模态框）
            let dir = with_ctx(|ctx| {
                let dir = ctx.config.record_dir.clone();
                std::fs::create_dir_all(&dir).ok();
                dir
            });
            if let Some(dir) = dir {
                shell_open(&dir);
            }
            true
        }
        CMD_EXIT => {
            exit_app();
            true
        }
        _ => settings::handle_command(cmd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：打开设置窗口不得死锁
    ///
    /// 建窗过程中 WM_CREATE 会同步回调 fill_from_config，若此时外层还握着上下文锁，
    /// 界面线程会把自己锁死（表现为程序无响应）。本用例若回归会直接挂住而非失败。
    #[test]
    fn settings_open_does_not_deadlock() {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap_or_default();
            settings::register(hinstance.into());
        }
        *CTX.lock().unwrap() = Some(UiCtx {
            shared: Shared::new(1),
            config: Config::default(),
            overlay_hwnd: HWND::default(),
            settings_hwnd: None,
        });

        settings::open();
        let opened = with_ctx(|ctx| ctx.settings_hwnd).flatten();
        assert!(opened.is_some(), "设置窗口未创建成功");
    }

    /// 上下文锁重入时必须降级而不是死锁（防御：即便将来有人写错也不会卡死界面）
    #[test]
    fn with_ctx_reentrant_degrades() {
        *CTX.lock().unwrap() = Some(UiCtx {
            shared: Shared::new(1),
            config: Config::default(),
            overlay_hwnd: HWND::default(),
            settings_hwnd: None,
        });
        let outer = with_ctx(|_ctx| {
            // 内层重入：应返回 None 并继续，而不是把自己锁死
            with_ctx(|_c| 42)
        });
        assert!(outer.is_some(), "外层调用应成功");
        assert_eq!(outer.flatten(), None, "重入调用应降级返回 None");
    }
}

