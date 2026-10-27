//! NVIDIA 管理库（NVML）动态加载：GPU 使用率/温度/频率/显存
//! 仅在机器上存在 nvml.dll（NVIDIA 驱动）时可用；A 卡/I 卡走 PDH 兜底。

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, PCWSTR};

use crate::util;

#[repr(C)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

type FnInit = unsafe extern "C" fn() -> i32;
type FnShutdown = unsafe extern "C" fn() -> i32;
type FnGetCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnGetHandle = unsafe extern "C" fn(u32, *mut isize) -> i32;
type FnGetUtil = unsafe extern "C" fn(isize, *mut NvmlUtilization) -> i32;
type FnGetTemp = unsafe extern "C" fn(isize, u32, *mut u32) -> i32;
type FnGetClock = unsafe extern "C" fn(isize, u32, *mut u32) -> i32;
type FnGetMem = unsafe extern "C" fn(isize, *mut NvmlMemory) -> i32;

pub struct NvmlSample {
    pub gpu_pct: Option<f32>,
    pub temp_c: Option<f32>,
    pub clock_mhz: Option<u32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
}

pub struct Nvml {
    module: HMODULE,
    shutdown: FnShutdown,
    get_util: FnGetUtil,
    get_temp: FnGetTemp,
    get_clock: FnGetClock,
    get_mem: FnGetMem,
    device: isize,
}

impl Nvml {
    /// 尝试加载 nvml.dll 并初始化；失败返回 None（非 NVIDIA 显卡属正常情况）
    pub fn try_init() -> Option<Nvml> {
        unsafe {
            let module = LoadLibraryW(PCWSTR(util::wide("nvml.dll").as_ptr())).ok()?;
            let get = |name: &str| -> Option<usize> {
                // GetProcAddress 需要 \0 结尾的 C 字符串；&str 不保证结尾有 \0
                let mut nb = name.as_bytes().to_vec();
                nb.push(0);
                GetProcAddress(module, PCSTR(nb.as_ptr())).map(|f| f as usize)
            };
            let init: FnInit = std::mem::transmute(get("nvmlInit_v2")?);
            if init() != 0 {
                let _ = FreeLibrary(module);
                return None;
            }
            let get_count: FnGetCount = std::mem::transmute(get("nvmlDeviceGetCount_v2")?);
            let get_handle: FnGetHandle = std::mem::transmute(get("nvmlDeviceGetHandleByIndex_v2")?);
            let mut count: u32 = 0;
            if get_count(&mut count) != 0 || count == 0 {
                return None;
            }
            let mut device: isize = 0;
            if get_handle(0, &mut device) != 0 {
                return None;
            }
            Some(Nvml {
                module,
                shutdown: std::mem::transmute(get("nvmlShutdown")?),
                get_util: std::mem::transmute(get("nvmlDeviceGetUtilizationRates")?),
                get_temp: std::mem::transmute(get("nvmlDeviceGetTemperature")?),
                get_clock: std::mem::transmute(get("nvmlDeviceGetClockInfo")?),
                get_mem: std::mem::transmute(get("nvmlDeviceGetMemoryInfo")?),
                device,
            })
        }
    }

    pub fn sample(&self) -> NvmlSample {
        let mut s = NvmlSample {
            gpu_pct: None,
            temp_c: None,
            clock_mhz: None,
            mem_used: None,
            mem_total: None,
        };
        unsafe {
            let mut u = NvmlUtilization { gpu: 0, memory: 0 };
            if (self.get_util)(self.device, &mut u) == 0 {
                s.gpu_pct = Some(u.gpu as f32);
            }
            let mut t: u32 = 0;
            // 0 = NVML_TEMPERATURE_GPU
            if (self.get_temp)(self.device, 0, &mut t) == 0 && (1..=150).contains(&t) {
                s.temp_c = Some(t as f32);
            }
            let mut c: u32 = 0;
            // 0 = NVML_CLOCK_GRAPHICS
            if (self.get_clock)(self.device, 0, &mut c) == 0 && c > 0 {
                s.clock_mhz = Some(c);
            }
            let mut m = NvmlMemory { total: 0, free: 0, used: 0 };
            if (self.get_mem)(self.device, &mut m) == 0 && m.total > 0 {
                s.mem_used = Some(m.used);
                s.mem_total = Some(m.total);
            }
        }
        s
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)();
            let _ = FreeLibrary(self.module);
        }
    }
}
