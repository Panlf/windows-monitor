//! 指标采集：所有底层数据源的统一入口

pub mod adl;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod mem;
pub mod net;
pub mod nvml;
pub mod pdh;
pub mod ring0;

use crate::metrics::Sample;
use crate::util;

/// 一次性诊断标志：每轮只写一次数据源状态日志
static DIAG_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct Collector {
    cpu: cpu::CpuUsage,
    net: net::NetState,
    disk: disk::DiskState,
    pdh: Option<gpu::PdhSensors>,
    nvml: Option<nvml::Nvml>,
    adl: Option<adl::Adl>,
    r0: Option<ring0::Ring0>,
}

impl Collector {
    pub fn new() -> Collector {
        // 内核驱动：管理员权限下可打开，用于直读 CPU 温度（AMD SMN / Intel MSR）
        let (r0, r0_err) = match ring0::Ring0::open() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        };
        let col = Collector {
            cpu: cpu::CpuUsage::new(),
            net: net::NetState::new(),
            disk: disk::DiskState::new(),
            pdh: gpu::PdhSensors::new(),
            nvml: nvml::Nvml::try_init(),
            adl: adl::Adl::try_init(),
            r0,
        };
        if let Some(e) = r0_err {
            util::debug_log(&format!("WinRing0: 不可用（{e}）；CPU 温度将退回 PDH 热区"));
        }
        col
    }

    /// 采集一轮全部指标；无数据的项为 None/0（CPU 使用率与网络速率首拍为 0）
    pub fn sample(&mut self) -> Sample {
        // PDH 先收集，稍后统一读取
        if let Some(p) = &self.pdh {
            p.collect();
        }

        let m = mem::mem();
        let mut s = Sample::empty(util::now_unix());
        s.cpu_pct = self.cpu.usage();
        s.cpu_freq_mhz = cpu::cpu_freq_mhz().unwrap_or(0);
        s.mem_pct = m.used_pct;
        s.mem_used_gb = m.used_gb;
        s.mem_total_gb = m.total_gb;

        // CPU 温度：内核驱动直读（AMD SMN / Intel MSR）；失败再退回 PDH ACPI 热区
        s.cpu_temp_c = self.cpu_temp_c();

        // 数据源状态诊断（只写一次）
        if !DIAG_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            util::debug_log(&format!(
                "数据源: WinRing0可用={}  PDH计数器入口可用={}  NVML可用={}  ADL可用={}  CPU温度={}",
                self.r0.is_some(),
                self.pdh.is_some(),
                self.nvml.is_some(),
                self.adl.is_some(),
                s.cpu_temp_c
                    .map(|v| format!("{:.1}°C", v))
                    .unwrap_or_else(|| "N/A".to_string())
            ));
        }

        // GPU：优先 NVML（NVIDIA，数据最全）；其次 ADL（AMD，温度/频率/使用率）；
        // 最后 PDH 兜底（I 卡等）。温度/频率仅独有数据源的厂商可读。
        if let Some(n) = &self.nvml {
            let g = n.sample();
            s.gpu_pct = g.gpu_pct;
            s.gpu_temp_c = g.temp_c;
            s.gpu_freq_mhz = g.clock_mhz;
            s.gpu_mem_used_gb = g.mem_used.map(|u| u as f32 / 1073741824.0);
            if let (Some(u), Some(t)) = (g.mem_used, g.mem_total) {
                s.gpu_mem_pct = Some((u as f64 / t as f64 * 100.0) as f32);
            }
        } else {
            // 非 NVIDIA：PDH 提供使用率/显存，ADL 提供 AMD 的温度/频率
            if let Some(p) = &self.pdh {
                s.gpu_pct = p.gpu_usage_pct();
                s.gpu_mem_used_gb = p.gpu_mem_used().map(|u| u as f32 / 1073741824.0);
            }
            if let Some(a) = &self.adl {
                let g = a.sample();
                s.gpu_temp_c = g.temp_c;
                s.gpu_freq_mhz = g.engine_mhz;
                if s.gpu_pct.is_none() {
                    s.gpu_pct = g.activity_pct;
                }
            }
        }

        let (down, up) = self.net.rates();
        s.net_down_bps = down;
        s.net_up_bps = up;

        let (disks, disk_read, disk_write) = self.disk.sample();
        s.disks = disks;
        s.disk_read_bps = disk_read;
        s.disk_write_bps = disk_write;
        s
    }

    /// CPU 温度：优先内核驱动直读传感器，失败退回 PDH ACPI 热区（多数机器没有）
    fn cpu_temp_c(&self) -> Option<f32> {
        if let Some(r0) = &self.r0 {
            if let Some(t) = cpu::temp_via_ring0(r0) {
                return Some(t);
            }
        }
        self.pdh.as_ref().and_then(|p| p.cpu_temp_c())
    }
}
