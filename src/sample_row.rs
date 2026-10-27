//! 悬浮窗一行指标的展示文本生成（纯逻辑，便于单独测试）

use crate::metrics::{Metric, Sample};

pub struct Row {
    pub label: String,
    pub value: String,
    /// 用于着色的百分比（使用率类指标）
    pub pct: Option<f32>,
}

impl Row {
    /// 按配置的指标顺序生成展示行；磁盘类指标每个磁盘一行
    pub fn build(metrics: &[Metric], sample: Option<&Sample>) -> Vec<Row> {
        let mut rows = Vec::new();
        for &m in metrics {
            match sample {
                None => rows.push(Row {
                    label: m.label().to_string(),
                    value: "--".to_string(),
                    pct: None,
                }),
                Some(s) => {
                    if m.is_disk() {
                        if s.disks.is_empty() {
                            rows.push(Row {
                                label: m.label().to_string(),
                                value: "N/A".to_string(),
                                pct: None,
                            });
                        }
                        for d in &s.disks {
                            let label = match m {
                                Metric::DiskUsage => format!("{}: 占用", d.name),
                                Metric::DiskTemp => format!("{}: 温度", d.name),
                                _ => d.name.clone(),
                            };
                            rows.push(Row {
                                label,
                                value: s.disk_value(m, d),
                                pct: if m == Metric::DiskUsage { Some(d.used_pct) } else { None },
                            });
                        }
                    } else {
                        rows.push(Row {
                            label: m.label().to_string(),
                            value: s.display_value(m),
                            pct: match m {
                                Metric::CpuUsage => Some(s.cpu_pct),
                                Metric::MemUsage => Some(s.mem_pct),
                                Metric::GpuUsage => s.gpu_pct,
                                _ => None,
                            },
                        });
                    }
                }
            }
        }
        rows
    }
}
