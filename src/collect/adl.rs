//! AMD 显卡 ADL（AMD Display Library）动态加载：GPU 温度/频率
//!
//! 通过 atiadlxx.dll（随 AMD 显卡驱动安装）读取 AMD 显卡的温度与当前引擎频率。
//! 温度读取链：ADL2_Overdrive5_Temperature_Get（独显热区）→
//! ADL2_New_QueryPMLogData_Get（PMLog，核显/APU 走这里，传感器 GFX/EDGE/SOC）。
//! 实现参考 LibreHardwareMonitor 的 AtiAdlxx / AmdGpu。

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, LoadLibraryW};
use windows::core::{PCSTR, PCWSTR};

use crate::util;

// 直接原始调用 kernel32.GetProcAddress：windows crate 的封装在本机返回 None（P/Invoke 实测能找到导出）
#[link(name = "kernel32")]
extern "system" {
    fn GetProcAddress(hModule: HMODULE, lpProcName: PCSTR) -> *mut core::ffi::c_void;
}

/// ADL_Main_Memory_Alloc：`void* (*)(int iSize)`，返回分配的内存指针，失败返回 NULL
///
/// 必须用系统 ucrtbase.dll 的 malloc：atiadlxx.dll 内部用它自己加载的
/// ucrtbase 的 free 释放回调分配的内存。exe 自身 CRT 即使静态化（crt-static）
/// 也是独立堆，跨堆 free 会崩溃，所以这里运行时从 ucrtbase 动态取 malloc。
extern "C" fn adl_malloc(size: i32) -> *mut core::ffi::c_void {
    if size <= 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        type MallocFn = unsafe extern "C" fn(usize) -> *mut core::ffi::c_void;
        static MALLOC_FN: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let addr = *MALLOC_FN.get_or_init(|| {
            let name = windows::core::w!("ucrtbase.dll");
            // Win10/11 上 ucrtbase 是系统组件；atiadlxx.dll 已加载时它必然在进程内
            let h = match GetModuleHandleW(name) {
                Ok(h) => h,
                Err(_) => LoadLibraryW(name).unwrap_or_default(),
            };
            if h.is_invalid() {
                return 0usize;
            }
            GetProcAddress(h, PCSTR(b"malloc\0".as_ptr())) as usize
        });
        if addr == 0 {
            return core::ptr::null_mut();
        }
        let f: MallocFn = std::mem::transmute(addr);
        f(size as usize)
    }
}

/// ADL2_Overdrive5_Temperature_Get 的参数
#[repr(C)]
#[allow(non_snake_case)]
struct ADLTemperature {
    iSize: i32,
    /// 温度，单位毫摄氏度（LHM：值 * 0.001）
    iTemperature: i32,
}

/// ADL2_Overdrive5_CurrentActivity_Get 的参数
#[repr(C)]
#[allow(non_snake_case)]
struct ADLPMActivity {
    iSize: i32,
    iEngineClock: i32,
    iMemoryClock: i32,
    iVddc: i32,
    iActivityPercent: i32,
    iCurrentPerformanceLevel: i32,
    iCurrentBusSpeed: i32,
    iBusLanes: i32,
    iBusLaneWidth: i32,
}

/// ADLPMLogDataOutput 内的单个传感器：supported!=0 表示驱动支持该项
#[repr(C)]
struct AdlSingleSensorData {
    supported: i32,
    value: i32,
}

/// ADL2_New_QueryPMLogData_Get 的输出：size + 256 个传感器槽（下标即传感器 ID）
#[repr(C)]
struct AdlPmLogDataOutput {
    size: i32,
    sensors: [AdlSingleSensorData; 256],
}

impl AdlPmLogDataOutput {
    fn zeroed() -> AdlPmLogDataOutput {
        AdlPmLogDataOutput {
            size: std::mem::size_of::<AdlPmLogDataOutput>() as i32,
            sensors: unsafe { std::mem::zeroed() },
        }
    }
}

/// PMLog 传感器 ID（LibreHardwareMonitor AtiAdlxx.cs ADLPMLogSensors）
const PMLOG_TEMPERATURE_EDGE: usize = 8;
const PMLOG_TEMPERATURE_GFX: usize = 28;
const PMLOG_TEMPERATURE_SOC: usize = 29;

type MallocCb = extern "C" fn(i32) -> *mut core::ffi::c_void;
type FnMainCreate = unsafe extern "C" fn(MallocCb, i32, *mut usize) -> i32;
type FnMainDestroy = unsafe extern "C" fn(usize) -> i32;
type FnNumAdapters = unsafe extern "C" fn(usize, *mut i32) -> i32;
type FnTempGet = unsafe extern "C" fn(usize, i32, i32, *mut ADLTemperature) -> i32;
type FnActivityGet = unsafe extern "C" fn(usize, i32, *mut ADLPMActivity) -> i32;
type FnPmLogQuery = unsafe extern "C" fn(usize, i32, *mut AdlPmLogDataOutput) -> i32;

pub struct AdlSample {
    pub temp_c: Option<f32>,
    pub engine_mhz: Option<u32>,
    pub activity_pct: Option<f32>,
}

pub struct Adl {
    module: HMODULE,
    context: usize,
    destroy: FnMainDestroy,
    temp_get: FnTempGet,
    activity_get: FnActivityGet,
    pmlog_query: Option<FnPmLogQuery>,
    /// 选定的物理适配器索引
    adapter_index: i32,
    /// PMLog 优先使用的温度传感器 ID（探测阶段确定）
    pmlog_sensor: Option<usize>,
    /// 是否已记录首次采样诊断（只记一次，避免每轮刷屏）
    logged_sample: std::sync::atomic::AtomicBool,
}

impl Adl {
    /// 尝试加载 atiadlxx.dll 并初始化 ADL；失败返回 None（非 AMD 显卡属正常情况）
    pub fn try_init() -> Option<Adl> {
        unsafe {
            let module = match LoadLibraryW(PCWSTR(util::wide("atiadlxx.dll").as_ptr())) {
                Ok(m) => m,
                Err(e) => {
                    util::debug_log(&format!("ADL: atiadlxx.dll 加载失败：{:?}", e));
                    return None;
                }
            };
            // 记录实际加载的是哪个 DLL（不同路径的 ADL 行为可能不同，便于定位）
            let mut buf = [0u16; 520];
            let n = GetModuleFileNameW(Some(module), &mut buf);
            if n > 0 {
                util::debug_log(&format!(
                    "ADL: 实际加载模块 = {}",
                    util::wide_to_string(&buf)
                ));
            }
            let get = |name: &str| -> Option<usize> {
                // 关键：GetProcAddress 需要以 \0 结尾的 C 字符串；&str 不保证结尾有 \0
                let mut nb = name.as_bytes().to_vec();
                nb.push(0);
                let p = GetProcAddress(module, PCSTR(nb.as_ptr()));
                if p.is_null() {
                    util::debug_log(&format!("ADL: GetProcAddress({}) 返回空", name));
                    None
                } else {
                    Some(p as usize)
                }
            };
            let create: FnMainCreate = match get("ADL2_Main_Control_Create") {
                Some(f) => std::mem::transmute(f),
                None => {
                    util::debug_log("ADL: ADL2_Main_Control_Create 导出缺失");
                    let _ = FreeLibrary(module);
                    return None;
                }
            };
            let mut context: usize = 0;
            // iEnumConnectedAdapters = 1：只枚举已连接的显示器适配器
            let ret = create(adl_malloc, 1, &mut context);
            if ret != 0 || context == 0 {
                util::debug_log(&format!(
                    "ADL: ADL2_Main_Control_Create 返回 {}，context={:#x}",
                    ret, context
                ));
                let _ = FreeLibrary(module);
                return None;
            }
            let destroy: FnMainDestroy = std::mem::transmute(get("ADL2_Main_Control_Destroy")?);
            let num_adapters: FnNumAdapters = match get("ADL2_Adapter_NumberOfAdapters_Get") {
                Some(f) => std::mem::transmute(f),
                None => {
                    util::debug_log("ADL: ADL2_Adapter_NumberOfAdapters_Get 缺失");
                    let _ = destroy(context);
                    let _ = FreeLibrary(module);
                    return None;
                }
            };
            let mut count: i32 = 0;
            let arc_ret = num_adapters(context, &mut count);
            if arc_ret != 0 || count <= 0 {
                util::debug_log(&format!(
                    "ADL: 适配器计数 返回 {}，数量 {}",
                    arc_ret, count
                ));
                let _ = destroy(context);
                let _ = FreeLibrary(module);
                return None;
            }
            util::debug_log(&format!("ADL: 检测到 {} 个适配器", count));

            let temp_get: FnTempGet = match get("ADL2_Overdrive5_Temperature_Get") {
                Some(f) => std::mem::transmute(f),
                None => {
                    util::debug_log("ADL: ADL2_Overdrive5_Temperature_Get 导出缺失");
                    let _ = destroy(context);
                    let _ = FreeLibrary(module);
                    return None;
                }
            };
            let activity_get: FnActivityGet = match get("ADL2_Overdrive5_CurrentActivity_Get") {
                Some(f) => std::mem::transmute(f),
                None => {
                    util::debug_log("ADL: ADL2_Overdrive5_CurrentActivity_Get 导出缺失");
                    let _ = destroy(context);
                    let _ = FreeLibrary(module);
                    return None;
                }
            };
            // PMLog 查询：核显/APU 温度的来源（老驱动可能没有该导出）
            let pmlog_query: Option<FnPmLogQuery> =
                get("ADL2_New_QueryPMLogData_Get").map(|f| std::mem::transmute(f));
            if pmlog_query.is_none() {
                util::debug_log("ADL: ADL2_New_QueryPMLogData_Get 缺失，核显温度不可用");
            }

            // 逐个探测适配器：优先挑能读到温度的（OD5 或 PMLog），否则退而求其次挑活动可读的
            let mut adapter_index: Option<i32> = None;
            let mut pmlog_sensor: Option<usize> = None;
            for idx in 0..count {
                // OD5 温度探测
                let mut t = ADLTemperature {
                    iSize: std::mem::size_of::<ADLTemperature>() as i32,
                    iTemperature: 0,
                };
                let tr = (temp_get)(context, idx, 0, &mut t);
                // PMLog 温度探测：优先 GFX(28)，其次 EDGE(8)、SOC(29)
                let mut pm_sensor: Option<(usize, i32, i32)> = None; // (sensor, supported, value)
                if let Some(q) = pmlog_query {
                    let mut log = AdlPmLogDataOutput::zeroed();
                    let qr = q(context, idx, &mut log);
                    if qr == 0 {
                        for id in [
                            PMLOG_TEMPERATURE_GFX,
                            PMLOG_TEMPERATURE_EDGE,
                            PMLOG_TEMPERATURE_SOC,
                        ] {
                            let s = &log.sensors[id];
                            if s.supported != 0 {
                                pm_sensor = Some((id, s.supported, s.value));
                                break;
                            }
                        }
                    } else if idx == 0 {
                        util::debug_log(&format!("ADL: PMLog 查询返回 {}", qr));
                    }
                }
                let mut act = ADLPMActivity {
                    iSize: std::mem::size_of::<ADLPMActivity>() as i32,
                    iEngineClock: 0,
                    iMemoryClock: 0,
                    iVddc: 0,
                    iActivityPercent: 0,
                    iCurrentPerformanceLevel: 0,
                    iCurrentBusSpeed: 0,
                    iBusLanes: 0,
                    iBusLaneWidth: 0,
                };
                let ar = (activity_get)(context, idx, &mut act);
                util::debug_log(&format!(
                    "ADL: 探测 idx={} OD5温度接口={}（原值 {}） PMLog温度={:?} 活动接口={}（时钟 {} 使用率 {}）",
                    idx, tr, t.iTemperature, pm_sensor, ar, act.iEngineClock, act.iActivityPercent
                ));
                let od5_ok = tr == 0 && (1..=150_000).contains(&t.iTemperature);
                let pm_ok = pm_sensor
                    .as_ref()
                    .map(|(_, _, v)| (0..=200_000).contains(v))
                    .unwrap_or(false);
                if od5_ok || pm_ok {
                    adapter_index = Some(idx);
                    if pm_ok && !od5_ok {
                        pmlog_sensor = pm_sensor.map(|(id, _, _)| id);
                    }
                    break;
                }
                if adapter_index.is_none() && ar == 0 {
                    adapter_index = Some(idx);
                }
            }
            let Some(adapter_index) = adapter_index else {
                util::debug_log("ADL: 所有适配器均无可读的温度/使用率");
                let _ = destroy(context);
                let _ = FreeLibrary(module);
                return None;
            };

            util::debug_log(&format!(
                "ADL: 初始化成功，适配器 {}，PMLog温度传感器={}",
                adapter_index,
                pmlog_sensor
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "无".to_string())
            ));
            Some(Adl {
                module,
                context,
                destroy,
                temp_get,
                activity_get,
                pmlog_query,
                adapter_index,
                pmlog_sensor,
                logged_sample: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    /// 从 PMLog 查询结果取温度（摄氏度）
    fn pmlog_temp(&self) -> Option<f32> {
        let q = self.pmlog_query?;
        let mut log = AdlPmLogDataOutput::zeroed();
        let qr = unsafe { q(self.context, self.adapter_index, &mut log) };
        if qr != 0 {
            return None;
        }
        // 探测阶段确定的传感器优先，其次按 GFX/EDGE/SOC 顺序
        let order: Vec<usize> = match self.pmlog_sensor {
            Some(s) => vec![s],
            None => vec![
                PMLOG_TEMPERATURE_GFX,
                PMLOG_TEMPERATURE_EDGE,
                PMLOG_TEMPERATURE_SOC,
            ],
        };
        for id in order {
            let s = &log.sensors[id];
            if s.supported == 0 {
                continue;
            }
            let raw = s.value as f32;
            // PMLog 温度应为整数摄氏度；>200 视为毫摄氏度做兼容
            let v = if raw > 200.0 { raw / 1000.0 } else { raw };
            if (0.0..=150.0).contains(&v) {
                return Some(v);
            }
        }
        None
    }

    pub fn sample(&self) -> AdlSample {
        let mut s = AdlSample {
            temp_c: None,
            engine_mhz: None,
            activity_pct: None,
        };
        unsafe {
            // 首次采样时输出一次诊断（swap 拿旧值，只有 false→true 的那次会记录）
            use std::sync::atomic::Ordering;
            let t_logged = self.logged_sample.swap(true, Ordering::SeqCst);
            // 温度：先 OD5（独显），失败再 PMLog（核显/APU）
            let mut t = ADLTemperature {
                iSize: std::mem::size_of::<ADLTemperature>() as i32,
                iTemperature: 0,
            };
            let temp_ret = (self.temp_get)(self.context, self.adapter_index, 0, &mut t);
            if temp_ret == 0 && (1..=150_000).contains(&t.iTemperature) {
                s.temp_c = Some(t.iTemperature as f32 * 0.001);
            } else {
                s.temp_c = self.pmlog_temp();
            }

            // 频率/使用率
            let mut act = ADLPMActivity {
                iSize: std::mem::size_of::<ADLPMActivity>() as i32,
                iEngineClock: 0,
                iMemoryClock: 0,
                iVddc: 0,
                iActivityPercent: 0,
                iCurrentPerformanceLevel: 0,
                iCurrentBusSpeed: 0,
                iBusLanes: 0,
                iBusLaneWidth: 0,
            };
            let act_ret = (self.activity_get)(self.context, self.adapter_index, &mut act);
            if act_ret == 0 {
                // 引擎频率：兼容两种单位（MHz 或 MHz*100）
                let mhz = act.iEngineClock as u32;
                let mhz = if mhz >= 10000 { mhz / 100 } else { mhz };
                // 125MHz(空闲) 到 10GHz 之间视为有效
                if (125..=10000).contains(&mhz) {
                    s.engine_mhz = Some(mhz);
                }
                if (0..=100).contains(&act.iActivityPercent) {
                    s.activity_pct = Some(act.iActivityPercent as f32);
                }
            }
            if !t_logged {
                util::debug_log(&format!(
                    "ADL: 首次采样 温度接口返回 {}（原值 {}，解析 {}），活动接口返回 {}（时钟原值 {}）",
                    temp_ret,
                    t.iTemperature,
                    s.temp_c
                        .map(|v| format!("{:.1}°C", v))
                        .unwrap_or_else(|| "N/A".to_string()),
                    act_ret,
                    act.iEngineClock
                ));
            }
            s
        }
    }
}

impl Drop for Adl {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.destroy)(self.context);
            let _ = FreeLibrary(self.module);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机实测 ADL 初始化链路：用于在 AMD 机器上定位温度/频率读不到的原因
    #[test]
    fn try_init_probe() {
        let a = Adl::try_init();
        crate::util::debug_log(&format!(
            "PROBE: Adl::try_init => {}",
            if a.is_some() { "OK" } else { "None" }
        ));
        if let Some(x) = a {
            let s = x.sample();
            crate::util::debug_log(&format!(
                "PROBE: temp={:?} freq_mhz={:?} activity={:?}",
                s.temp_c, s.engine_mhz, s.activity_pct
            ));
        }
    }
}
