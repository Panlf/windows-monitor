//! 指标定义与采样数据结构

use crate::util;

/// 可监控指标枚举
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    CpuUsage,
    CpuFreq,
    CpuTemp,
    MemUsage,
    GpuUsage,
    GpuFreq,
    GpuTemp,
    GpuMem,
    NetDown,
    NetUp,
    DiskRead,
    DiskWrite,
    DiskUsage,
    DiskTemp,
}

/// 全部指标（顺序即设置界面与 CSV 列的顺序）
pub const ALL_METRICS: [Metric; 14] = [
    Metric::CpuUsage,
    Metric::CpuFreq,
    Metric::CpuTemp,
    Metric::MemUsage,
    Metric::GpuUsage,
    Metric::GpuFreq,
    Metric::GpuTemp,
    Metric::GpuMem,
    Metric::NetDown,
    Metric::NetUp,
    Metric::DiskRead,
    Metric::DiskWrite,
    Metric::DiskUsage,
    Metric::DiskTemp,
];

impl Metric {
    /// 配置/CSV 中使用的键名
    pub fn key(self) -> &'static str {
        match self {
            Metric::CpuUsage => "cpu_usage",
            Metric::CpuFreq => "cpu_freq",
            Metric::CpuTemp => "cpu_temp",
            Metric::MemUsage => "mem_usage",
            Metric::GpuUsage => "gpu_usage",
            Metric::GpuFreq => "gpu_freq",
            Metric::GpuTemp => "gpu_temp",
            Metric::GpuMem => "gpu_mem",
            Metric::NetDown => "net_down",
            Metric::NetUp => "net_up",
            Metric::DiskRead => "disk_read_bps",
            Metric::DiskWrite => "disk_write_bps",
            Metric::DiskUsage => "disk_usage",
            Metric::DiskTemp => "disk_temp",
        }
    }

    /// 悬浮窗/设置界面显示名
    pub fn label(self) -> &'static str {
        match self {
            Metric::CpuUsage => "CPU 使用率",
            Metric::CpuFreq => "CPU 频率",
            Metric::CpuTemp => "CPU 温度",
            Metric::MemUsage => "内存使用率",
            Metric::GpuUsage => "GPU 使用率",
            Metric::GpuFreq => "GPU 频率",
            Metric::GpuTemp => "GPU 温度",
            Metric::GpuMem => "显存占用",
            Metric::NetDown => "网络下行",
            Metric::NetUp => "网络上行",
            Metric::DiskRead => "磁盘读取",
            Metric::DiskWrite => "磁盘写入",
            Metric::DiskUsage => "磁盘占用率",
            Metric::DiskTemp => "磁盘温度",
        }
    }

    pub fn from_key(key: &str) -> Option<Metric> {
        ALL_METRICS.iter().copied().find(|m| m.key() == key)
    }

    /// 是否为磁盘类指标（在 CSV 中按磁盘展开为多列）
    pub fn is_disk(self) -> bool {
        matches!(self, Metric::DiskUsage | Metric::DiskTemp)
    }

    /// 悬浮窗分组标题（不同类别分层显示）
    pub fn category(self) -> &'static str {
        match self {
            Metric::CpuUsage | Metric::CpuFreq | Metric::CpuTemp => "CPU",
            Metric::MemUsage => "内存",
            Metric::GpuUsage | Metric::GpuFreq | Metric::GpuTemp | Metric::GpuMem => "GPU",
            Metric::NetDown | Metric::NetUp => "网络",
            Metric::DiskRead | Metric::DiskWrite | Metric::DiskUsage | Metric::DiskTemp => "磁盘",
        }
    }
}

/// 悬浮窗分组的固定顺序（与用户勾选顺序无关，保证类别层次稳定）
pub const CATEGORY_ORDER: [&str; 5] = ["CPU", "内存", "GPU", "网络", "磁盘"];

/// 单个磁盘的一次采样
#[derive(Clone, Debug)]
pub struct DiskSample {
    /// 盘符，如 "C"
    pub name: String,
    pub used_pct: f32,
    pub total_gb: f32,
    pub free_gb: f32,
    pub temp_c: Option<f32>,
}

/// 一次完整采样；无数据的项为 None（不能当成 0）
#[derive(Clone, Debug)]
pub struct Sample {
    pub t: i64,
    pub cpu_pct: f32,
    pub cpu_freq_mhz: u32,
    pub cpu_temp_c: Option<f32>,
    pub mem_pct: f32,
    pub mem_used_gb: f32,
    pub mem_total_gb: f32,
    pub gpu_pct: Option<f32>,
    pub gpu_freq_mhz: Option<u32>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_mem_pct: Option<f32>,
    pub gpu_mem_used_gb: Option<f32>,
    pub net_down_bps: u64,
    pub net_up_bps: u64,
    /// 全机磁盘读/写速率（所有物理盘合计，字节/秒）
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub disks: Vec<DiskSample>,
}

impl Sample {
    pub fn empty(t: i64) -> Sample {
        Sample {
            t,
            cpu_pct: 0.0,
            cpu_freq_mhz: 0,
            cpu_temp_c: None,
            mem_pct: 0.0,
            mem_used_gb: 0.0,
            mem_total_gb: 0.0,
            gpu_pct: None,
            gpu_freq_mhz: None,
            gpu_temp_c: None,
            gpu_mem_pct: None,
            gpu_mem_used_gb: None,
            net_down_bps: 0,
            net_up_bps: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            disks: Vec::new(),
        }
    }

    /// 悬浮窗里单个指标的取值文本（磁盘类指标用 disk_value）
    pub fn display_value(&self, m: Metric) -> String {
        match m {
            Metric::CpuUsage => format!("{:.0}%", self.cpu_pct),
            Metric::CpuFreq => util::format_mhz(self.cpu_freq_mhz),
            Metric::CpuTemp => match self.cpu_temp_c {
                Some(v) => format!("{:.0}°C", v),
                None => "N/A".to_string(),
            },
            Metric::MemUsage => format!(
                "{:.0}%  {:.1}/{:.0}GB",
                self.mem_pct, self.mem_used_gb, self.mem_total_gb
            ),
            Metric::GpuUsage => match self.gpu_pct {
                Some(v) => format!("{:.0}%", v),
                None => "N/A".to_string(),
            },
            Metric::GpuFreq => match self.gpu_freq_mhz {
                Some(v) => util::format_mhz(v),
                None => "N/A".to_string(),
            },
            Metric::GpuTemp => match self.gpu_temp_c {
                Some(v) => format!("{:.0}°C", v),
                None => "N/A".to_string(),
            },
            Metric::GpuMem => match (self.gpu_mem_pct, self.gpu_mem_used_gb) {
                (Some(p), Some(g)) => format!("{:.0}%  {:.1}GB", p, g),
                (Some(p), None) => format!("{:.0}%", p),
                (None, Some(g)) => format!("{:.1}GB", g),
                _ => "N/A".to_string(),
            },
            Metric::NetDown => util::format_bps(self.net_down_bps),
            Metric::NetUp => util::format_bps(self.net_up_bps),
            Metric::DiskRead => util::format_bps(self.disk_read_bps),
            Metric::DiskWrite => util::format_bps(self.disk_write_bps),
            Metric::DiskUsage | Metric::DiskTemp => String::new(),
        }
    }

    /// 磁盘类指标按磁盘的取值文本
    pub fn disk_value(&self, m: Metric, d: &DiskSample) -> String {
        match m {
            Metric::DiskUsage => format!("{:.0}%  {:.0}/{:.0}GB", d.used_pct, d.total_gb - d.free_gb, d.total_gb),
            Metric::DiskTemp => match d.temp_c {
                Some(v) => format!("{:.0}°C", v),
                None => "N/A".to_string(),
            },
            _ => String::new(),
        }
    }
}
