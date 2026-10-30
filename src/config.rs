//! 配置文件读写（INI 风格，随程序同目录，便于拷贝到新机器使用）

use std::fs;
use std::path::{Path, PathBuf};

use crate::metrics::{Metric, ALL_METRICS};

#[derive(Clone)]
pub struct Config {
    pub overlay_visible: bool,
    pub overlay_x: i32,
    pub overlay_y: i32,
    pub overlay_opacity: u8,
    /// 悬浮窗刷新间隔（秒），同时也是采样线程的最小采样周期
    pub refresh_secs: u32,
    /// 桌面悬浮窗显示的指标
    pub display_metrics: Vec<Metric>,
    /// 记录（写 CSV）的指标
    pub record_metrics: Vec<Metric>,
    /// 采集数据日志目录
    pub record_dir: String,
    /// 记录采样间隔（秒）
    pub record_interval_secs: u32,
    /// 启动时自动开始记录
    pub autostart_record: bool,
    /// 是否已提示过“未提权时温度可能读不到”（避免每次启动都弹窗）
    pub warned_no_admin: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            overlay_visible: true,
            overlay_x: 24,
            overlay_y: 24,
            overlay_opacity: 225,
            refresh_secs: 1,
            display_metrics: ALL_METRICS.to_vec(),
            record_metrics: ALL_METRICS.to_vec(),
            record_dir: crate::util::exe_dir()
                .join("logs")
                .to_string_lossy()
                .into_owned(),
            record_interval_secs: 1,
            autostart_record: false,
            warned_no_admin: false,
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        crate::util::exe_dir().join("winmon.ini")
    }

    /// 读取配置；文件不存在或损坏时用默认值（缺失字段同样回退默认）
    pub fn load(path: &Path) -> Config {
        let mut cfg = Config::default();
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return cfg,
        };
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') || line.starts_with('[') {
                continue;
            }
            let (k, v) = match line.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
                None => continue,
            };
            match k.as_str() {
                "visible" => cfg.overlay_visible = parse_bool(&v),
                "x" => cfg.overlay_x = parse_int(&v, cfg.overlay_x),
                "y" => cfg.overlay_y = parse_int(&v, cfg.overlay_y),
                "opacity" => {
                    cfg.overlay_opacity = parse_int(&v, cfg.overlay_opacity as i32).clamp(60, 255) as u8
                }
                "refresh_secs" => {
                    cfg.refresh_secs = parse_int(&v, cfg.refresh_secs as i32).clamp(1, 3600) as u32
                }
                "metrics" => cfg.display_metrics = parse_metrics(&v),
                "record_dir" => {
                    if !v.is_empty() {
                        cfg.record_dir = v;
                    }
                }
                "record_interval_secs" => {
                    cfg.record_interval_secs = parse_int(&v, cfg.record_interval_secs as i32).clamp(1, 86400) as u32
                }
                "record_metrics" => cfg.record_metrics = parse_metrics(&v),
                "autostart_record" => cfg.autostart_record = parse_bool(&v),
                "warned_no_admin" => cfg.warned_no_admin = parse_bool(&v),
                _ => {}
            }
        }
        if cfg.display_metrics.is_empty() {
            cfg.display_metrics = ALL_METRICS.to_vec();
        }
        if cfg.record_metrics.is_empty() {
            cfg.record_metrics = ALL_METRICS.to_vec();
        }
        cfg
    }

    /// 保存配置；中文注释方便手工调整
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let s = format!(
            "# WinMon 配置文件（可手工编辑，保存设置时会整体重写）\n\
             [ui]\n\
             visible={}\n\
             x={}\n\
             y={}\n\
             opacity={}\n\
             refresh_secs={}\n\
             # 桌面显示指标：{}\n\
             metrics={}\n\
             \n\
             [record]\n\
             # 采集数据日志目录\n\
             record_dir={}\n\
             # 采集时间间隔（秒）\n\
             record_interval_secs={}\n\
             # 记录的采集指标：{}\n\
             record_metrics={}\n\
             # 启动时自动开始记录（1/0）\n\
             autostart_record={}\n\
             # 是否已提示过未提权（1/0，内部用）\n\
             warned_no_admin={}\n",
            self.overlay_visible as i32,
            self.overlay_x,
            self.overlay_y,
            self.overlay_opacity,
            self.refresh_secs,
            ALL_METRICS.iter().map(|m| m.key()).collect::<Vec<_>>().join(","),
            join_metrics(&self.display_metrics),
            self.record_dir,
            self.record_interval_secs,
            ALL_METRICS.iter().map(|m| m.key()).collect::<Vec<_>>().join(","),
            join_metrics(&self.record_metrics),
            self.autostart_record as i32,
            self.warned_no_admin as i32,
        );
        // 先写临时文件再替换（同目录 rename 原子），写入中途被杀也不会留下截断的配置
        let tmp = path.with_extension("ini.tmp");
        let write_direct = |body: &str| fs::write(path, body);
        let write_atomic = |body: &str| -> std::io::Result<()> {
            fs::write(&tmp, body)?;
            match fs::rename(&tmp, path) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(e)
                }
            }
        };
        write_atomic(&s)
            .or_else(|_| write_direct(&s))
            .map_err(|e| format!("写入配置文件失败：{}", e))
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(v.trim(), "1" | "true" | "yes" | "on")
}

fn parse_int(v: &str, default: i32) -> i32 {
    v.trim().parse::<i32>().unwrap_or(default)
}

fn parse_metrics(v: &str) -> Vec<Metric> {
    v.split(',')
        .filter_map(|s| Metric::from_key(s.trim()))
        .collect()
}

fn join_metrics(list: &[Metric]) -> String {
    list.iter().map(|m| m.key()).collect::<Vec<_>>().join(",")
}
