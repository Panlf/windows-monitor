//! WinMon —— 轻量 Windows 10/11 机器监控
//!
//! 功能：CPU/内存/GPU/磁盘占用率、温度与频率、网络速率的桌面悬浮窗显示；
//! 按配置的指标与时间间隔持续采集到 CSV，并可生成本地 HTML 深度分析报告。
//!
//! 设计目标：体积小、常驻内存低、采集不打扰前台应用（采样线程低优先级）。

// GUI 子系统：启动时不带控制台黑框。
// `--cpu-test` 自检模式会在运行时附加父进程控制台，命令行输出不受影响。
#![windows_subsystem = "windows"]

mod collect;
mod config;
mod metrics;
mod recorder;
mod report;
mod sample_row;
mod sampler;
mod ui;
mod util;

use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

use config::Config;
use ui::UiCtx;

/// 单实例互斥体名
const MUTEX_NAME: &str = "WinMon_SingleInstance_Mutex";

fn main() {
    // 自检模式：winmon --cpu-test 输出一轮 CPU 采样后退出（命令行验证数据源用）
    // GUI 子系统下没有控制台：附加到父进程的控制台让 println 可见
    if std::env::args().any(|a| a == "--cpu-test") {
        unsafe {
            use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
        selftest_cpu();
        return;
    }

    unsafe {
        // 高 DPI 下按显示器缩放绘制
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 温度采集需要管理员权限：未提权时自动请求，失败则继续运行（温度显示 N/A）
    // 注意：必须先提权再创建单实例互斥体，否则未提权进程持有的互斥体句柄未释放时，
    //      提权后的新实例会误判“已在运行”而提前退出。
    if !is_elevated() {
        try_elevate();
    }

    // 单实例
    let mutex = match unsafe {
        CreateMutexW(None, false, windows::core::PCWSTR(util::wide(MUTEX_NAME).as_ptr()))
    } {
        Ok(h) => h,
        Err(e) => {
            ui::msg_error(&format!("初始化失败：{}", e));
            return;
        }
    };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        ui::msg_info("WinMon 已经在运行了（可在托盘图标中找到它）。");
        unsafe {
            let _ = CloseHandle(mutex);
        }
        return;
    }

    let cfg = Config::load(&Config::config_path());

    // 启动低优先级采样线程
    let shared = sampler::Shared::new(cfg.refresh_secs);
    let sampler_handle = sampler::spawn(shared.clone());

    // 界面
    let hinstance: windows::Win32::Foundation::HINSTANCE =
        unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
            .unwrap_or_default()
            .into();
    ui::overlay::register(hinstance);
    ui::settings::register(hinstance);

    let overlay_hwnd = ui::overlay::create(
        hinstance,
        cfg.overlay_x,
        cfg.overlay_y,
        cfg.overlay_opacity,
        cfg.overlay_visible,
    );

    *ui::CTX.lock().unwrap() = Some(UiCtx {
        shared,
        config: cfg,
        overlay_hwnd,
        settings_hwnd: None,
    });

    ui::tray::add(overlay_hwnd);
    ui::overlay::relayout(overlay_hwnd);

    // 未提权时温度常读不到：仅首次弹一次提示，并记录到配置避免每次启动都打扰
    if !is_elevated() {
        let warned = ui::with_ctx(|ctx| ctx.config.warned_no_admin).unwrap_or(false);
        if !warned {
            ui::msg_info(
                "未获得管理员权限，CPU/磁盘温度可能无法读取（显示为 N/A）。\n\
                 \n\
                 如需查看 CPU、磁盘温度，请以管理员身份重新启动程序并允许 UAC 提示。\n\
                 GPU 温度/频率需要 NVIDIA 显卡才可读取。",
            );
            ui::with_ctx(|ctx| ctx.config.warned_no_admin = true);
            ui::save_config();
        }
    }

    // 启动时自动开始记录
    let autostart = ui::with_ctx(|ctx| ctx.config.autostart_record).unwrap_or(false);
    if autostart {
        ui::start_record();
    }

    // 主消息循环
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    ui::tray::remove(overlay_hwnd);
    ui::save_config();
    sampler_handle.join().ok();
    unsafe {
        let _ = CloseHandle(mutex);
    }
}

/// 自检模式：采两轮（使用率需要差值），打印 CPU 相关指标
fn selftest_cpu() {
    let mut c = collect::Collector::new();
    let _ = c.sample();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let s = c.sample();
    println!("CPU 型号: {}", cpu_name());
    println!("CPU 使用率: {:.0}%", s.cpu_pct);
    println!(
        "CPU 频率: {}",
        if s.cpu_freq_mhz > 0 {
            util::format_mhz(s.cpu_freq_mhz)
        } else {
            "N/A".to_string()
        }
    );
    println!(
        "CPU 温度: {}",
        s.cpu_temp_c
            .map(|v| format!("{:.1}°C", v))
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!(
        "GPU 使用率: {}",
        s.gpu_pct
            .map(|v| format!("{:.0}%", v))
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!(
        "GPU 频率: {}",
        s.gpu_freq_mhz
            .map(util::format_mhz)
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!(
        "GPU 温度: {}",
        s.gpu_temp_c
            .map(|v| format!("{:.1}°C", v))
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!("磁盘读取: {}", util::format_bps(s.disk_read_bps));
    println!("磁盘写入: {}", util::format_bps(s.disk_write_bps));
    println!("（诊断日志见 exe 同目录 winmon_debug.log）");
}

fn cpu_name() -> String {
    // CPUID brand string：leaf 0x80000002..0x80000004
    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004u32 {
        let r = std::arch::x86_64::__cpuid(leaf);
        bytes.extend(r.eax.to_le_bytes());
        bytes.extend(r.ebx.to_le_bytes());
        bytes.extend(r.ecx.to_le_bytes());
        bytes.extend(r.edx.to_le_bytes());
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// 是否已获得管理员权限
fn is_elevated() -> bool {
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    let mut token = HANDLE::default();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elev.TokenIsElevated != 0
    }
}

/// 未提权时以管理员身份重启自身
fn try_elevate() {
    use windows::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => return,
    };
    let verb = util::wide("runas");
    let file = util::wide(&exe);
    let mut info = SHELLEXECUTEINFOW::default();
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = windows::core::PCWSTR(verb.as_ptr());
    info.lpFile = windows::core::PCWSTR(file.as_ptr());
    info.nShow = SW_SHOWNORMAL.0 as i32;
    if unsafe { ShellExecuteExW(&mut info) }.is_ok() {
        // 新实例已提权启动，本实例退出
        std::process::exit(0);
    }
}
