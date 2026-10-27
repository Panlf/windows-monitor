//! 内存使用情况

use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

pub struct MemSample {
    pub used_pct: f32,
    pub used_gb: f32,
    pub total_gb: f32,
}

pub fn mem() -> MemSample {
    let mut ms = MEMORYSTATUSEX::default();
    ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut ms) }.is_err() {
        return MemSample {
            used_pct: 0.0,
            used_gb: 0.0,
            total_gb: 0.0,
        };
    }
    let total = ms.ullTotalPhys as f64;
    let used = total - ms.ullAvailPhys as f64;
    MemSample {
        used_pct: (used / total * 100.0) as f32,
        used_gb: (used / 1073741824.0) as f32,
        total_gb: (total / 1073741824.0) as f32,
    }
}
