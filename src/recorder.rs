//! 采集数据记录：按配置的指标与时间间隔写 CSV
//!
//! CSV 列固定（未勾选的指标列留空，保证分析报告解析稳定）：
//! time,cpu_pct,cpu_freq_mhz,cpu_temp_c,mem_pct,mem_used_gb,gpu_pct,gpu_freq_mhz,gpu_temp_c,
//! gpu_mem_pct,net_down_bps,net_up_bps,disk_<盘符>_used_pct,disk_<盘符>_temp_c,...

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use crate::metrics::{Metric, Sample};
use crate::util;

pub struct Recorder {
    writer: std::io::BufWriter<File>,
    /// 记录指标集合（决定哪些列写值）
    metrics: Vec<Metric>,
    /// 采样时刻（秒），用于按 interval 节流
    last_write_t: Option<i64>,
    interval: u32,
    rows: u64,
    disk_names: Vec<String>,
}

impl Recorder {
    /// 开始记录：在 dir 下新建 winmon_<时间戳>.csv 并写表头
    pub fn start(
        dir: &str,
        interval: u32,
        metrics: &[Metric],
        disks: &[String],
    ) -> Result<Recorder, String> {
        fs::create_dir_all(dir).map_err(|e| format!("创建日志目录失败：{}", e))?;
        let name = format!("winmon_{}.csv", util::local_time_compact(util::now_unix()));
        let path = PathBuf::from(dir).join(name);
        let file = File::create(&path).map_err(|e| format!("创建数据文件失败：{}", e))?;
        let mut writer = std::io::BufWriter::new(file);

        let mut header = String::from(
            "time,cpu_pct,cpu_freq_mhz,cpu_temp_c,mem_pct,mem_used_gb,gpu_pct,gpu_freq_mhz,gpu_temp_c,gpu_mem_pct,net_down_bps,net_up_bps,disk_read_bps,disk_write_bps",
        );
        for d in disks {
            header.push_str(&format!(",disk_{}_used_pct,disk_{}_temp_c", d, d));
        }
        header.push('\n');
        writer
            .write_all(header.as_bytes())
            .map_err(|e| format!("写入表头失败：{}", e))?;

        Ok(Recorder {
            writer,
            metrics: metrics.to_vec(),
            last_write_t: None,
            interval: interval.max(1),
            rows: 0,
            disk_names: disks.to_vec(),
        })
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    fn on(&self, m: Metric) -> bool {
        self.metrics.contains(&m)
    }

    /// 按记录间隔节流写入一行
    pub fn write_if_due(&mut self, s: &Sample) {
        if let Some(last) = self.last_write_t {
            if s.t - last < self.interval as i64 {
                return;
            }
        }
        self.last_write_t = Some(s.t);

        let mut line = format!("{},", s.t);
        // 固定列（顺序必须与表头一致）
        line.push_str(&f(if self.on(Metric::CpuUsage) { Some(s.cpu_pct) } else { None }, 1));
        line.push(',');
        line.push_str(&i(if self.on(Metric::CpuFreq) { Some(s.cpu_freq_mhz as i64) } else { None }));
        line.push(',');
        line.push_str(&f(s.cpu_temp_c.filter(|_| self.on(Metric::CpuTemp)), 1));
        line.push(',');
        line.push_str(&f(if self.on(Metric::MemUsage) { Some(s.mem_pct) } else { None }, 1));
        line.push(',');
        line.push_str(&f(if self.on(Metric::MemUsage) { Some(s.mem_used_gb) } else { None }, 2));
        line.push(',');
        line.push_str(&f(s.gpu_pct.filter(|_| self.on(Metric::GpuUsage)), 1));
        line.push(',');
        line.push_str(&i(
            s.gpu_freq_mhz.filter(|_| self.on(Metric::GpuFreq)).map(|v| v as i64),
        ));
        line.push(',');
        line.push_str(&f(s.gpu_temp_c.filter(|_| self.on(Metric::GpuTemp)), 1));
        line.push(',');
        line.push_str(&f(s.gpu_mem_pct.filter(|_| self.on(Metric::GpuMem)), 1));
        line.push(',');
        line.push_str(&i(if self.on(Metric::NetDown) { Some(s.net_down_bps as i64) } else { None }));
        line.push(',');
        line.push_str(&i(if self.on(Metric::NetUp) { Some(s.net_up_bps as i64) } else { None }));
        line.push(',');
        line.push_str(&i(if self.on(Metric::DiskRead) { Some(s.disk_read_bps as i64) } else { None }));
        line.push(',');
        line.push_str(&i(if self.on(Metric::DiskWrite) { Some(s.disk_write_bps as i64) } else { None }));

        for d in &self.disk_names {
            let ds = s.disks.iter().find(|x| &x.name == d);
            line.push(',');
            line.push_str(&f(
                ds.and_then(|x| if self.on(Metric::DiskUsage) { Some(x.used_pct) } else { None }),
                1,
            ));
            line.push(',');
            line.push_str(&f(
                ds.and_then(|x| x.temp_c).filter(|_| self.on(Metric::DiskTemp)),
                1,
            ));
        }
        line.push('\n');
        if self.writer.write_all(line.as_bytes()).is_ok() {
            self.rows += 1;
            // 每行刷新一次开销可忽略（1Hz 级别），并保证随时能生成分析报告
            let _ = self.writer.flush();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}

/// 浮点单元格：None 或非有限值 -> 空
fn f(v: Option<f32>, digits: usize) -> String {
    match v {
        Some(v) if v.is_finite() => format!("{:.*}", digits, v),
        _ => String::new(),
    }
}

/// 整数单元格
fn i(v: Option<i64>) -> String {
    match v {
        Some(v) => format!("{}", v),
        None => String::new(),
    }
}

/// 列出目录下最近修改的 CSV（用于“生成分析报告”默认分析对象）
pub fn latest_csv(dir: &str) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x.eq_ignore_ascii_case("csv")).unwrap_or(false) {
            let t = e.metadata().and_then(|m| m.modified()).ok();
            if let Some(t) = t {
                if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}
