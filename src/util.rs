//! 通用小工具：时间格式化、宽字符串转换等

use std::path::PathBuf;

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
};

/// 当前 Unix 秒级时间戳
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix 时间戳 -> 本地时间 SYSTEMTIME
fn unix_to_local_systemtime(ts: i64) -> Option<SYSTEMTIME> {
    // FILETIME 自 1601-01-01 起算，单位 100ns；与 Unix 纪元相差 11644473600 秒
    let ticks = ((ts + 11644473600) * 10_000_000) as u64;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err() {
            return None;
        }
        let mut local = SYSTEMTIME::default();
        if SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return None;
        }
        Some(local)
    }
}

/// Unix 时间戳 -> "YYYYMMDD_HHMMSS"（本地时区，用于文件名）
pub fn local_time_compact(ts: i64) -> String {
    match unix_to_local_systemtime(ts) {
        Some(st) => format!(
            "{:04}{:02}{:02}_{:02}{:02}{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        ),
        None => format!("{}", ts),
    }
}

/// 指定时刻的本地时区相对 UTC 的偏移（分钟，东八区为 +480）
pub fn tz_offset_minutes(at_ts: i64) -> i64 {
    let ticks = ((at_ts + 11644473600) * 10_000_000) as u64;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    unsafe {
        // 先把该 UTC 时刻转成“系统当前时区的墙钟时间”
        let mut utc = SYSTEMTIME::default();
        if FileTimeToSystemTime(&ft, &mut utc).is_err() {
            return 0;
        }
        let mut local = SYSTEMTIME::default();
        if SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return 0;
        }
        // 把“墙钟时间当作 UTC 序列化”，与真正的 UTC 序列化相减即得偏移
        let mut local_ft = FILETIME::default();
        if SystemTimeToFileTime(&local, &mut local_ft).is_err() {
            return 0;
        }
        let local_ticks = ((local_ft.dwHighDateTime as u64) << 32) | local_ft.dwLowDateTime as u64;
        let diff_secs = (local_ticks as i64 - ticks as i64) / 10_000_000;
        diff_secs / 60
    }
}

/// String -> 以 \0 结尾的 UTF-16 缓冲
pub fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// UTF-16 切片 -> String（截断到第一个 \0）
pub fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 网络速率格式化（入参 B/s）
pub fn format_bps(bps: u64) -> String {
    let b = bps as f64;
    if b >= 1_073_741_824.0 {
        format!("{:.2} GB/s", b / 1_073_741_824.0)
    } else if b >= 1_048_576.0 {
        format!("{:.2} MB/s", b / 1_048_576.0)
    } else if b >= 1024.0 {
        format!("{:.1} KB/s", b / 1024.0)
    } else {
        format!("{} B/s", bps)
    }
}

/// 频率格式化（入参 MHz）
pub fn format_mhz(mhz: u32) -> String {
    format!("{:.2} GHz", mhz as f32 / 1000.0)
}

/// 可执行文件所在目录（配置文件、日志的默认位置）
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 临时诊断日志：数据源状态写入可执行文件同目录的 winmon_debug.log，定位温度/频率读不到的原因
pub fn debug_log(msg: &str) {
    use std::io::Write;
    let path = exe_dir().join("winmon_debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {}", local_time_compact(now_unix()), msg);
    }
}
