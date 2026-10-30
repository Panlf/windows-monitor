//! CSV 采集数据 → 自包含 HTML 深度分析报告
//!
//! 输出完全离线可打开：样式与图表全部内联（SVG），不引用任何外部资源。
//! CSV 列格式见 recorder.rs：time + 固定指标列 + 动态 disk_<盘符>_* 列；空单元格表示无数据。

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

/// 本地时区偏移（分钟），在 generate 时由调用方（util::tz_offset_minutes）注入，
/// 避免在非东八区机器上时间错位 8 小时。默认 +8 仅为无注入时的兜底。
static TZ_OFFSET_MINUTES: AtomicI64 = AtomicI64::new(480);

const CHART_W: i32 = 920;
const CHART_H: i32 = 210;
const PAD_L: f64 = 64.0;
const PAD_R: f64 = 12.0;
const PAD_T: f64 = 14.0;
const PAD_B: f64 = 28.0;
/// 图表最多绘制的点数（超过则降采样，取桶内最大值以保留尖峰）
const MAX_POINTS: usize = 600;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Pct,
    TempC,
    FreqMhz,
    MemGb,
    NetBps,
    Num,
}

struct Series {
    label: String,
    kind: Kind,
    vals: Vec<Option<f64>>,
}

struct Anomaly {
    label: String,
    kind: Kind,
    start: i64,
    end: i64,
    peak: f64,
    samples: usize,
}

/// 生成 HTML 报告，返回给用户看的摘要文本
///
/// `tz_offset_min` 为本地时区相对 UTC 的偏移（分钟，东八区 +480），用于把 CSV 中的
/// Unix 时间戳转为本地时间显示。
pub fn generate(
    csv_path: &Path,
    out_html: &Path,
    tz_offset_min: i64,
) -> Result<String, String> {
    TZ_OFFSET_MINUTES.store(tz_offset_min, Ordering::Relaxed);
    let text = std::fs::read_to_string(csv_path).map_err(|e| format!("读取 CSV 失败：{}", e))?;
    let (times, series) = parse_csv(&text)?;
    if times.is_empty() {
        return Err("采集数据为空，无法分析".to_string());
    }
    let anomalies = find_anomalies(&times, &series);
    let html = render(&times, &series, &anomalies, csv_path);
    std::fs::write(out_html, html).map_err(|e| format!("写入报告失败：{}", e))?;

    let first = fmt_time(times[0]);
    let last = fmt_time(times[times.len() - 1]);
    Ok(format!(
        "分析完成：{} 条记录（{} ~ {}），{} 个指标，{} 个异常时段",
        times.len(),
        first,
        last,
        series.len(),
        anomalies.len()
    ))
}

// ---------------------------------------------------------------- CSV 解析

fn parse_csv(text: &str) -> Result<(Vec<i64>, Vec<Series>), String> {
    let mut lines = text.lines();
    let header = lines.next().ok_or("CSV 为空")?;
    let keys: Vec<&str> = header.split(',').map(|s| s.trim()).collect();
    let t_idx = keys
        .iter()
        .position(|&k| k == "time")
        .ok_or("CSV 缺少 time 列")?;

    let mut times: Vec<i64> = Vec::new();
    let mut cols: Vec<Vec<Option<f64>>> = vec![Vec::new(); keys.len()];

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let cells: Vec<&str> = line.split(',').collect();
        let t: f64 = match cells.get(t_idx).and_then(|c| c.trim().parse::<f64>().ok()) {
            Some(v) if v.is_finite() => v,
            _ => continue,
        };
        times.push(t as i64);
        for (i, col) in cols.iter_mut().enumerate() {
            let v = cells
                .get(i)
                .and_then(|c| c.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite());
            col.push(v);
        }
    }

    let mut series = Vec::new();
    for (i, &key) in keys.iter().enumerate() {
        if i == t_idx {
            continue;
        }
        if cols[i].iter().all(|v| v.is_none()) {
            continue; // 全空列（未勾选的指标）不参与分析
        }
        let (label, kind) = key_info(key);
        series.push(Series {
            label,
            kind,
            vals: std::mem::take(&mut cols[i]),
        });
    }
    Ok((times, series))
}

/// 由列名推导中文名与数值类型
fn key_info(key: &str) -> (String, Kind) {
    let kind = if key.ends_with("_pct") {
        Kind::Pct
    } else if key.ends_with("_temp_c") {
        Kind::TempC
    } else if key.ends_with("_freq_mhz") {
        Kind::FreqMhz
    } else if key.ends_with("_used_gb") {
        Kind::MemGb
    } else if key.starts_with("net_") || key.ends_with("_bps") {
        Kind::NetBps
    } else {
        Kind::Num
    };

    if key.starts_with("disk_") && key.ends_with("_used_pct") {
        return (format!("磁盘 {} 占用率", &key[5..key.len() - 9]), kind);
    }
    if key.starts_with("disk_") && key.ends_with("_temp_c") {
        return (format!("磁盘 {} 温度", &key[5..key.len() - 7]), kind);
    }
    let label = match key {
        "cpu_pct" => "CPU 占用率",
        "cpu_freq_mhz" => "CPU 频率",
        "cpu_temp_c" => "CPU 温度",
        "mem_pct" => "内存占用率",
        "mem_used_gb" => "内存已用",
        "gpu_pct" => "GPU 占用率",
        "gpu_freq_mhz" => "GPU 频率",
        "gpu_temp_c" => "GPU 温度",
        "gpu_mem_pct" => "GPU 显存占用",
        "net_down_bps" => "网络下行速率",
        "net_up_bps" => "网络上行速率",
        "disk_read_bps" => "磁盘读取速率",
        "disk_write_bps" => "磁盘写入速率",
        _ => key,
    };
    (label.to_string(), kind)
}

// ---------------------------------------------------------------- 统计与异常

/// (均值, P95, 最大值, 有效点数)
fn stats(vals: &[Option<f64>]) -> Option<(f64, f64, f64, usize)> {
    let mut v: Vec<f64> = vals.iter().flatten().copied().collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    let idx = (((n as f64) * 0.95).ceil() as usize).clamp(1, n) - 1;
    Some((mean, v[idx], v[n - 1], n))
}

fn threshold(s: &Series) -> Option<f64> {
    match s.kind {
        Kind::Pct => Some(85.0),
        Kind::TempC => Some(if s.label.starts_with("磁盘") { 50.0 } else { 85.0 }),
        _ => None,
    }
}

fn find_anomalies(times: &[i64], series: &[Series]) -> Vec<Anomaly> {
    let mut out = Vec::new();
    for s in series {
        let thr = match threshold(s) {
            Some(t) => t,
            None => continue,
        };
        let mut i = 0;
        while i < s.vals.len() {
            match s.vals[i] {
                Some(v) if v >= thr => {
                    let start_i = i;
                    let mut peak = v;
                    while i < s.vals.len() {
                        match s.vals[i] {
                            Some(v2) if v2 >= thr => {
                                peak = peak.max(v2);
                                i += 1;
                            }
                            _ => break,
                        }
                    }
                    if i - start_i >= 2 {
                        out.push(Anomaly {
                            label: s.label.clone(),
                            kind: s.kind,
                            start: times[start_i],
                            end: times[i - 1],
                            peak,
                            samples: i - start_i,
                        });
                    }
                }
                _ => i += 1,
            }
        }
    }
    out.sort_by(|a, b| {
        b.peak
            .partial_cmp(&a.peak)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(20);
    out
}

// ---------------------------------------------------------------- HTML 渲染

fn render(times: &[i64], series: &[Series], anomalies: &[Anomaly], csv_path: &Path) -> String {
    let t0 = times[0];
    let t1 = times[times.len() - 1];
    let dur = (t1 - t0).max(0);
    let interval = est_interval(times);
    let file_name = csv_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut h = String::with_capacity(256 * 1024);
    h.push_str("<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    h.push_str("<title>WinMon 深度分析报告</title><style>");
    h.push_str(CSS);
    h.push_str("</style></head><body><div class=\"wrap\">");

    // 头部
    h.push_str("<header><h1>WinMon 机器状态深度分析</h1><div class=\"meta\">");
    h.push_str(&format!("<span>数据文件：{}</span>", esc(&file_name)));
    h.push_str(&format!(
        "<span>时段：{} ~ {}（{}）</span>",
        fmt_time(t0),
        fmt_time(t1),
        fmt_dur(dur)
    ));
    h.push_str(&format!(
        "<span>样本 {} 条 · 采样间隔约 {}</span>",
        times.len(),
        fmt_dur(interval)
    ));
    h.push_str("</div></header>");

    // 概览卡片
    h.push_str("<h2>指标概览</h2><div class=\"cards\">");
    for s in series {
        if let Some((mean, p95, max, n)) = stats(&s.vals) {
            let color = kind_color(s.kind);
            h.push_str(&format!(
                "<div class=\"card\" style=\"border-left-color:{}\"><div class=\"ct\">{}</div>\
                 <div class=\"cv\" style=\"color:{}\">{}</div>\
                 <div class=\"cs\">P95 {} · 最大 {} · 有效 {:.0}%</div></div>",
                color,
                esc(&s.label),
                color,
                fmt_val(s.kind, mean),
                fmt_val(s.kind, p95),
                fmt_val(s.kind, max),
                n as f64 / times.len() as f64 * 100.0
            ));
        }
    }
    h.push_str("</div>");

    // 时间序列图
    h.push_str("<h2>时间序列</h2>");
    for s in series {
        h.push_str("<section class=\"chart\">");
        h.push_str(&format!("<h3>{}</h3>", esc(&s.label)));
        h.push_str(&chart_svg(s, times));
        h.push_str("</section>");
    }

    // 异常时段
    h.push_str("<h2>异常时段（越限采样段）</h2>");
    if anomalies.is_empty() {
        h.push_str("<p class=\"ok\">未检测到持续越限时段：占用率 ≥ 85%、CPU/GPU 温度 ≥ 85°C、磁盘温度 ≥ 50°C 的连续片段会列在这里。</p>");
    } else {
        h.push_str("<table><thead><tr><th>指标</th><th>开始</th><th>结束</th><th>持续</th><th>峰值</th><th>采样点</th></tr></thead><tbody>");
        for a in anomalies {
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"peak\">{}</td><td>{}</td></tr>",
                esc(&a.label),
                fmt_time(a.start),
                fmt_time(a.end),
                fmt_dur((a.end - a.start).max(0)),
                fmt_val(a.kind, a.peak),
                a.samples
            ));
        }
        h.push_str("</tbody></table>");
    }

    // 结论与建议
    h.push_str("<h2>结论与建议</h2><ul class=\"concl\">");
    for c in conclusions(times, series, anomalies) {
        h.push_str(&format!("<li>{}</li>", esc(&c)));
    }
    h.push_str("</ul>");

    h.push_str("<footer>本报告由 WinMon 在本机离线生成，数据未做任何上传。</footer>");
    h.push_str("</div></body></html>");
    h
}

fn conclusions(times: &[i64], series: &[Series], anomalies: &[Anomaly]) -> Vec<String> {
    let mut out = Vec::new();
    let find = |name: &str| series.iter().find(|s| s.label == name);

    if let Some(s) = find("CPU 占用率") {
        if let Some((mean, p95, max, _)) = stats(&s.vals) {
            if mean >= 70.0 {
                out.push(format!(
                    "CPU 整体负载偏高（均值 {:.0}%，峰值 {:.0}%），建议排查常驻高占用进程。",
                    mean, max
                ));
            } else if p95 >= 90.0 {
                out.push(format!(
                    "CPU 平时负载不高，但存在突发高负载（P95 {:.0}%），可能与间歇性任务有关。",
                    p95
                ));
            } else {
                out.push(format!("CPU 负载整体平稳（均值 {:.0}%，最大 {:.0}%）。", mean, max));
            }
        }
    }
    if let Some(s) = find("CPU 温度") {
        if let Some((_, _, max, _)) = stats(&s.vals) {
            if max >= 90.0 {
                out.push(format!("CPU 温度峰值达 {:.0}°C，已接近过热降频区间，建议检查散热。", max));
            } else if max >= 80.0 {
                out.push(format!("CPU 温度峰值 {:.0}°C，处于偏高但可接受范围。", max));
            }
        }
    }
    if let Some(s) = find("内存占用率") {
        if let Some((mean, _, max, _)) = stats(&s.vals) {
            if mean >= 80.0 {
                out.push(format!(
                    "内存长期紧张（均值 {:.0}%，峰值 {:.0}%），建议增加内存或减少常驻程序。",
                    mean, max
                ));
            } else if max >= 95.0 {
                out.push("内存曾接近耗尽，可能出现过卡顿或频繁换页。".to_string());
            }
        }
    }
    if let Some(s) = find("GPU 温度") {
        if let Some((_, _, max, _)) = stats(&s.vals) {
            if max >= 85.0 {
                out.push(format!("GPU 温度峰值 {:.0}°C，高负载下散热压力较大。", max));
            }
        }
    }
    if let Some(s) = find("GPU 占用率") {
        if let Some((mean, p95, _, _)) = stats(&s.vals) {
            if p95 >= 95.0 {
                out.push(format!(
                    "GPU 存在持续满载（P95 {:.0}%，均值 {:.0}%），游戏/渲染/计算任务是主要负载来源。",
                    p95, mean
                ));
            }
        }
    }
    for s in series {
        if s.label.starts_with("磁盘 ") && s.label.ends_with("占用率") {
            if let Some((_, _, max, _)) = stats(&s.vals) {
                if max >= 90.0 {
                    out.push(format!("{} 峰值 {:.0}%，磁盘空间将满，建议清理或扩容。", s.label, max));
                }
            }
        }
        if s.label.starts_with("磁盘 ") && s.label.ends_with("温度") {
            if let Some((_, _, max, _)) = stats(&s.vals) {
                if max >= 55.0 {
                    out.push(format!("{} 峰值 {:.0}°C，偏高，注意硬盘散热。", s.label, max));
                }
            }
        }
    }
    if let (Some(d), Some(u)) = (find("网络下行速率"), find("网络上行速率")) {
        if let (Some((_, _, dmax, _)), Some((_, _, umax, _))) = (stats(&d.vals), stats(&u.vals)) {
            out.push(format!(
                "网络峰值：下行 {} / 上行 {}。",
                fmt_val(Kind::NetBps, dmax),
                fmt_val(Kind::NetBps, umax)
            ));
        }
    }

    if !anomalies.is_empty() {
        out.push(format!(
            "共检测到 {} 个持续越限时段，详见上表；可结合时间点回忆当时的使用场景。",
            anomalies.len()
        ));
    } else {
        out.push("未检测到持续越限时段，机器状态整体健康。".to_string());
    }
    let dur = times[times.len() - 1] - times[0];
    out.push(format!(
        "本次分析覆盖 {}，如需更完整结论可延长记录时长后再次生成报告。",
        fmt_dur(dur.max(0))
    ));
    out
}

// ---------------------------------------------------------------- SVG 图表

fn chart_svg(s: &Series, times: &[i64]) -> String {
    let n = times.len();
    // 降采样：每桶取最大值保留尖峰；空桶断线
    let bucket = (n + MAX_POINTS - 1) / MAX_POINTS;
    let mut pts: Vec<(f64, bool)> = Vec::new(); // (值, 是否新起点)
    let mut seg_t: Vec<i64> = Vec::new();
    let mut i = 0;
    let mut gap = true;
    while i < n {
        let end = (i + bucket).min(n);
        let mut best: Option<f64> = None;
        for j in i..end {
            if let Some(v) = s.vals[j] {
                best = Some(best.map_or(v, |b: f64| b.max(v)));
            }
        }
        if let Some(v) = best {
            pts.push((v, gap));
            seg_t.push(times[i]);
            gap = false;
        } else {
            gap = true;
        }
        i = end;
    }
    if pts.is_empty() {
        return "<p class=\"ok\">该指标没有有效数据。</p>".to_string();
    }

    // Y 轴范围
    let mut vmin = f64::MAX;
    let mut vmax = f64::MIN;
    for (v, _) in &pts {
        vmin = vmin.min(*v);
        vmax = vmax.max(*v);
    }
    let (ymin, ymax) = match s.kind {
        Kind::Pct => (0.0, 100.0),
        Kind::FreqMhz => ((vmin * 0.9).floor(), (vmax * 1.05).ceil().max(vmin + 1.0)),
        _ => (0.0, (vmax * 1.08).ceil().max(vmin + 1.0)),
    };
    let plot_w = CHART_W as f64 - PAD_L - PAD_R;
    let plot_h = CHART_H as f64 - PAD_T - PAD_B;
    let t0 = seg_t[0] as f64;
    let t1 = seg_t[seg_t.len() - 1] as f64;
    let span = (t1 - t0).max(1.0);
    let mapx = |t: i64| PAD_L + (t as f64 - t0) / span * plot_w;
    let mapy = |v: f64| PAD_T + (1.0 - (v - ymin) / (ymax - ymin).max(1e-9)) * plot_h;

    let color = kind_color(s.kind);

    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" xmlns=\"http://www.w3.org/2000/svg\">",
        CHART_W, CHART_H
    );
    // 网格与刻度
    for k in 0..=3 {
        let y = PAD_T + plot_h * k as f64 / 3.0;
        let val = ymax - (ymax - ymin) * k as f64 / 3.0;
        svg.push_str(&format!(
            "<line x1=\"{}\" y1=\"{:.1}\" x2=\"{}\" y2=\"{:.1}\" class=\"grid\"/>",
            PAD_L,
            y,
            CHART_W as f64 - PAD_R,
            y
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{:.1}\" class=\"tick\">{}</text>",
            PAD_L as i32 - 6,
            y + 4.0,
            esc(&fmt_val(s.kind, val))
        ));
    }
    // 折线 + 面积
    let mut line = String::new();
    let mut area = String::new();
    let mut prev_x = PAD_L;
    for (idx, (v, new_seg)) in pts.iter().enumerate() {
        let x = mapx(seg_t[idx]);
        let y = mapy(*v);
        let cmd = if *new_seg || idx == 0 { "M" } else { "L" };
        line.push_str(&format!("{}{:.1} {:.1} ", cmd, x, y));
        if idx == 0 || *new_seg {
            if idx > 0 {
                area.push_str(&format!("L{:.1} {:.1} Z ", prev_x, mapy(ymin)));
            }
            area.push_str(&format!("M{:.1} {:.1} L{:.1} {:.1} ", x, mapy(ymin), x, y));
        } else {
            area.push_str(&format!("L{:.1} {:.1} ", x, y));
        }
        prev_x = x;
    }
    area.push_str(&format!("L{:.1} {:.1} Z", prev_x, mapy(ymin)));
    svg.push_str(&format!("<path d=\"{}\" class=\"area\" fill=\"{}\" fill-opacity=\"0.16\"/>", area, color));
    svg.push_str(&format!("<path d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"1.6\"/>", line, color));
    // X 轴时间
    svg.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{}\" class=\"tick\">{}</text>",
        PAD_L,
        CHART_H - 8,
        esc(&fmt_time_short(seg_t[0]))
    ));
    svg.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{}\" class=\"tick\" text-anchor=\"end\">{}</text>",
        CHART_W as f64 - PAD_R,
        CHART_H - 8,
        esc(&fmt_time_short(seg_t[seg_t.len() - 1]))
    ));
    svg.push_str("</svg>");
    svg
}

// ---------------------------------------------------------------- 工具

fn kind_color(kind: Kind) -> &'static str {
    match kind {
        Kind::Pct => "#4fc3f7",
        Kind::TempC => "#ef5350",
        Kind::FreqMhz => "#ffb74d",
        Kind::MemGb => "#7e57c2",
        Kind::NetBps => "#26a69a",
        Kind::Num => "#90a4ae",
    }
}

fn fmt_val(kind: Kind, v: f64) -> String {
    match kind {
        Kind::Pct => format!("{:.1}%", v),
        Kind::TempC => format!("{:.1}°C", v),
        Kind::FreqMhz => format!("{:.0} MHz", v),
        Kind::MemGb => format!("{:.2} GB", v),
        Kind::NetBps => {
            let bytes = v / 8.0;
            if bytes >= 1024.0 * 1024.0 {
                format!("{:.2} MB/s", bytes / 1024.0 / 1024.0)
            } else if bytes >= 1024.0 {
                format!("{:.1} KB/s", bytes / 1024.0)
            } else {
                format!("{:.0} B/s", bytes)
            }
        }
        Kind::Num => format!("{:.1}", v),
    }
}

fn est_interval(times: &[i64]) -> i64 {
    if times.len() < 2 {
        return 0;
    }
    let mut diffs: Vec<i64> = times.windows(2).map(|w| (w[1] - w[0]).max(0)).collect();
    diffs.sort_unstable();
    diffs[diffs.len() / 2]
}

fn fmt_dur(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{} 小时 {} 分 {} 秒", h, m, s)
    } else if m > 0 {
        format!("{} 分 {} 秒", m, s)
    } else {
        format!("{} 秒", s)
    }
}

/// Unix 时间戳 → 本地 "YYYY-MM-DD HH:MM:SS"
fn fmt_time(t: i64) -> String {
    // 饱和运算：CSV 的 time 列可被手工编辑成任意大数，防止溢出 panic
    let offset = TZ_OFFSET_MINUTES.load(Ordering::Relaxed).saturating_mul(60);
    let secs = t.saturating_add(offset);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        mo,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn fmt_time_short(t: i64) -> String {
    let s = fmt_time(t);
    // 年份超 4 位（异常大时间戳）时按原样返回，避免切片越界 panic
    if s.len() >= 19 {
        s[5..16].to_string() // MM-DD HH:MM
    } else {
        s
    }
}

/// 1970 起的天数 → 公历年月日（Howard Hinnant 算法）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const CSS: &str = r#"
* { box-sizing: border-box; }
body { margin: 0; background: #0e1420; color: #d6dce8;
  font: 14px/1.7 "Segoe UI", "Microsoft YaHei UI", sans-serif; }
.wrap { max-width: 1020px; margin: 0 auto; padding: 28px 20px 60px; }
header h1 { margin: 0 0 6px; font-size: 24px; color: #f2f6fc; }
.meta { display: flex; flex-wrap: wrap; gap: 8px 22px; color: #8b96a8; font-size: 13px; }
h2 { margin: 34px 0 14px; font-size: 18px; color: #9ecbff;
  border-left: 3px solid #2f6fd0; padding-left: 10px; }
.cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 12px; }
.card { background: #17202e; border-left: 3px solid #4fc3f7; border-radius: 8px;
  padding: 12px 14px; }
.ct { color: #93a0b4; font-size: 12px; }
.cv { font-size: 22px; font-weight: 600; margin: 2px 0; }
.cs { color: #77839a; font-size: 11.5px; }
.chart { background: #131b28; border: 1px solid #223047; border-radius: 8px;
  padding: 12px 14px 6px; margin-bottom: 14px; }
.chart h3 { margin: 0 0 6px; font-size: 14px; color: #b8c4d8; font-weight: 600; }
svg { width: 100%; height: auto; display: block; }
.grid { stroke: #223047; stroke-width: 1; }
.tick { fill: #6b7789; font-size: 11px; }
table { width: 100%; border-collapse: collapse; background: #131b28;
  border: 1px solid #223047; border-radius: 8px; overflow: hidden; }
th, td { padding: 8px 12px; text-align: left; border-bottom: 1px solid #1d2940; font-size: 13px; }
th { background: #1a2434; color: #93a0b4; font-weight: 600; }
tr:last-child td { border-bottom: none; }
.peak { color: #ff8a80; }
.ok { background: #131b28; border: 1px solid #223047; border-radius: 8px;
  padding: 12px 16px; color: #9ad29a; }
.concl li { margin: 6px 0; }
footer { margin-top: 40px; color: #5b6675; font-size: 12px; text-align: center; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟测试：构造 CSV → 生成报告 → 校验关键内容 → 清理临时文件
    #[test]
    fn smoke_generate() {
        let dir = std::env::temp_dir().join("winmon_report_test");
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("t.csv");
        let html = dir.join("t.html");

        let mut text = String::from(
            "time,cpu_pct,cpu_freq_mhz,cpu_temp_c,mem_pct,mem_used_gb,gpu_pct,gpu_freq_mhz,\
             gpu_temp_c,gpu_mem_pct,net_down_bps,net_up_bps,disk_C_used_pct,disk_C_temp_c\n",
        );
        let base = 1_700_000_000i64;
        for i in 0..100 {
            let cpu = if i % 20 < 5 { 92.0 } else { 30.0 }; // 制造持续越限段
            text.push_str(&format!(
                "{},{},3800,65.0,50.0,8.0,,,,,1000,500,70.0,42.0\n",
                base + i * 5,
                cpu
            ));
        }
        std::fs::write(&csv, text).unwrap();

        let summary = generate(&csv, &html, 480).unwrap();
        let out = std::fs::read_to_string(&html).unwrap();
        assert!(summary.contains("条记录"));
        assert!(summary.contains("异常时段"));
        assert!(out.contains("CPU 占用率"));
        assert!(out.contains("磁盘 C 温度"));
        assert!(out.contains("<svg"));
        assert!(out.contains("结论与建议"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
