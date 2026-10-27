//! CPU 使用率与当前频率

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Power::{CallNtPowerInformation, PROCESSOR_POWER_INFORMATION};
use windows::Win32::System::SystemInformation::{GetNativeSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::GetSystemTimes;

const PROCESSOR_INFORMATION: windows::Win32::System::Power::POWER_INFORMATION_LEVEL =
    windows::Win32::System::Power::POWER_INFORMATION_LEVEL(11); // ProcessorInformation

/// CPU 总体使用率（依赖前后两次快照做差值）
pub struct CpuUsage {
    last: Option<(u64, u64, u64)>, // (idle, kernel, user)，单位 100ns
}

impl CpuUsage {
    pub fn new() -> CpuUsage {
        CpuUsage { last: None }
    }

    /// 返回 0.0-100.0 的 CPU 总使用率；首次调用返回 0.0
    pub fn usage(&mut self) -> f32 {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_err() {
                return 0.0;
            }
        }
        let cur = (ft_to_u64(idle), ft_to_u64(kernel), ft_to_u64(user));
        let prev = self.last.replace(cur);
        match prev {
            Some((pi, pk, pu)) => {
                let total = (cur.1 + cur.2).saturating_sub(pk + pu);
                if total == 0 {
                    return 0.0;
                }
                let busy = total.saturating_sub(cur.0.saturating_sub(pi));
                (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
            }
            None => 0.0,
        }
    }
}

fn ft_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// CPU 当前平均频率（MHz），逐核 CurrentMhz 取平均
pub fn cpu_freq_mhz() -> Option<u32> {
    let mut sys = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut sys);
    }
    let n = sys.dwNumberOfProcessors.max(1) as usize;
    let mut buf: Vec<PROCESSOR_POWER_INFORMATION> =
        vec![PROCESSOR_POWER_INFORMATION::default(); n];
    let size = (n * std::mem::size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
    let st = unsafe {
        CallNtPowerInformation(
            PROCESSOR_INFORMATION,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            size,
        )
    };
    if st.0 != 0 {
        return None;
    }
    let sum: u64 = buf.iter().map(|p| p.CurrentMhz as u64).sum();
    let avg = (sum / n as u64) as u32;
    // 0 或明显异常的值视为无数据
    if (300..=20000).contains(&avg) {
        Some(avg)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// CPU 温度：内核驱动直读传感器（参考 LibreHardwareMonitor / Linux k10temp）
// ---------------------------------------------------------------------------

/// CPU 厂商字符串（"AuthenticAMD" / "GenuineIntel" / 其他）
pub fn cpu_vendor() -> &'static str {
    static VENDOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VENDOR.get_or_init(|| {
        let r = std::arch::x86_64::__cpuid(0);
        let bytes: Vec<u8> = r
            .ebx
            .to_le_bytes()
            .into_iter()
            .chain(r.edx.to_le_bytes())
            .chain(r.ecx.to_le_bytes())
            .collect();
        String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string()
    })
}

/// AMD Family 17h+（Zen 全系）：SMN 通道读 Tctl 温度
///
/// SMN 通过 PCI 0:0:0 的索引/数据寄存器（0x60/0x64）访问；
/// THM_TCON_CUR_TMP（SMN 0x59800）的 bit[31:21] 为当前温度，单位 0.125°C；
/// bit19 置位时读数含 49°C 的偏移修正（仅 Zen1 部分型号，减去即可）。
/// 算法与 LibreHardwareMonitor Amd17Cpu / Linux k10temp 一致。
const AMD_SMN_TCTL: u32 = 0x0005_9800;
const AMD_PCI_SMN_INDEX: u32 = 0x60; // PCI bus 0, dev 0, fn 0
const AMD_PCI_SMN_DATA: u32 = 0x64;

pub fn amd_temp_c(r0: &super::ring0::Ring0) -> Option<f32> {
    if !r0.write_pci_config(0, AMD_PCI_SMN_INDEX, AMD_SMN_TCTL) {
        return None;
    }
    let raw = r0.read_pci_config(0, AMD_PCI_SMN_DATA)?;
    let mut t = ((raw >> 21) as f32) * 0.125;
    if raw & 0x0008_0000 != 0 {
        t -= 49.0;
    }
    (0.0..=125.0).contains(&t).then_some(t)
}

/// Intel：TjMax（MSR 0x1A2）- Package 温度读数（MSR 0x1B1）
const IA32_TEMPERATURE_TARGET: u32 = 0x1A2;
const IA32_PACKAGE_THERM_STATUS: u32 = 0x1B1;

pub fn intel_temp_c(r0: &super::ring0::Ring0) -> Option<f32> {
    let tjmax_raw = r0.read_msr(IA32_TEMPERATURE_TARGET)?;
    let tjmax = ((tjmax_raw >> 16) & 0xFF) as f32;
    if !(50.0..=125.0).contains(&tjmax) {
        return None;
    }
    let pkg = r0.read_msr(IA32_PACKAGE_THERM_STATUS)?;
    let reading = ((pkg >> 16) & 0x7F) as f32;
    let t = tjmax - reading;
    (0.0..=125.0).contains(&t).then_some(t)
}

/// 按厂商分派温度读取（需已打开内核驱动）
pub fn temp_via_ring0(r0: &super::ring0::Ring0) -> Option<f32> {
    let v = cpu_vendor();
    if v == "AuthenticAMD" {
        amd_temp_c(r0)
    } else if v == "GenuineIntel" {
        intel_temp_c(r0)
    } else {
        None
    }
}
