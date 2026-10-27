//! PDH 计数器采集：GPU 使用率/显存（无 NVML 时的兜底）与 CPU 温度（ACPI 热区）
//!
//! 使用英文计数器路径（PdhAddEnglishCounterW），与系统显示语言无关。
//! 说明：ACPI 热区温度是 CPU 附近热区的近似值，无传感器的台式机可能读不到。

use super::pdh::PdhQuery;
use windows::Win32::System::Performance::PDH_HCOUNTER;

pub struct PdhSensors {
    pdh: PdhQuery,
    c_engine: Option<PDH_HCOUNTER>,
    c_adapter_mem: Option<PDH_HCOUNTER>,
    c_thermal: Option<PDH_HCOUNTER>,
}

impl PdhSensors {
    /// 至少成功添加一个计数器才返回 Some
    pub fn new() -> Option<PdhSensors> {
        let pdh = PdhQuery::new()?;
        let c_engine = pdh.add_english("\\GPU Engine(*)\\Utilization Percentage");
        let c_adapter_mem = pdh.add_english("\\GPU Adapter Memory(*)\\Dedicated Usage");
        let c_thermal = pdh.add_english("\\Thermal Zone Information(*)\\Temperature");
        if c_engine.is_none() && c_adapter_mem.is_none() && c_thermal.is_none() {
            return None;
        }
        Some(PdhSensors {
            pdh,
            c_engine,
            c_adapter_mem,
            c_thermal,
        })
    }

    /// 每个采样周期调用一次（一次收集刷新全部计数器）
    pub fn collect(&self) {
        self.pdh.collect();
    }

    /// GPU 总使用率：按物理 GPU 汇总引擎实例，取最忙 GPU 的结果
    ///
    /// 实例名形如 `pid_1234_luid_..._phys_0_engtype_3D_eng_0`；
    /// 同一 GPU 同一引擎类型的所有进程之和，再取各引擎类型的最大值（3D/Compute/Copy 在时间上重叠，不能直接相加）。
    pub fn gpu_usage_pct(&self) -> Option<f32> {
        let c = self.c_engine?;
        // (phys, engtype) -> sum
        let mut sums: Vec<((u32, String), f64)> = Vec::new();
        for (name, val) in self.pdh.array(&c) {
            let phys = match parse_token_u32(&name, "_phys_") {
                Some(p) => p,
                None => continue,
            };
            let engtype = parse_token_str(&name, "engtype_").unwrap_or_else(|| "_".to_string());
            let key = (phys, engtype);
            match sums.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => *v += val,
                None => sums.push((key, val)),
            }
        }
        let mut per_gpu: Vec<(u32, f64)> = Vec::new();
        for ((phys, _), v) in sums {
            match per_gpu.iter_mut().find(|(p, _)| *p == phys) {
                Some((_, max)) => {
                    if v > *max {
                        *max = v;
                    }
                }
                None => per_gpu.push((phys, v)),
            }
        }
        per_gpu.into_iter().map(|(_, v)| v).fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a: f64| a.max(v)))
        }).map(|v| (v.min(100.0)) as f32)
    }

    /// 已用显存（字节）：按物理 GPU 汇总 Dedicated Usage 取最大值
    pub fn gpu_mem_used(&self) -> Option<u64> {
        let c = self.c_adapter_mem?;
        let mut per_gpu: Vec<(u32, f64)> = Vec::new();
        for (name, val) in self.pdh.array(&c) {
            let phys = match parse_token_u32(&name, "_phys_") {
                Some(p) => p,
                None => continue,
            };
            match per_gpu.iter_mut().find(|(p, _)| *p == phys) {
                Some((_, v)) => *v += val,
                None => per_gpu.push((phys, val)),
            }
        }
        per_gpu
            .into_iter()
            .map(|(_, v)| v)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a: f64| a.max(v)))
            })
            .map(|v| v as u64)
    }

    /// CPU 温度（摄氏度）：取各 ACPI 热区的最高温度；计数器单位为开尔文
    pub fn cpu_temp_c(&self) -> Option<f32> {
        let c = self.c_thermal?;
        let mut best: Option<f64> = None;
        for (_, kelvin) in self.pdh.array(&c) {
            if !(150.0..=500.0).contains(&kelvin) {
                continue;
            }
            best = Some(best.map_or(kelvin, |b: f64| b.max(kelvin)));
        }
        best.map(|k| (k - 273.15) as f32)
    }
}

/// 从实例名中提取形如 `_phys_12` 后面的数字
fn parse_token_u32(name: &str, token: &str) -> Option<u32> {
    let pos = name.find(token)? + token.len();
    let rest = &name[pos..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 从实例名中提取形如 `engtype_3D` 后面的引擎类型名（到下一个下划线为止）
fn parse_token_str(name: &str, token: &str) -> Option<String> {
    let pos = name.find(token)? + token.len();
    let rest = &name[pos..];
    let end = rest.find('_').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}
