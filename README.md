# WinMon —— 轻量 Windows 机器监控

WinMon 是一个用 Rust 编写的 Windows 桌面监控小工具：常驻托盘、桌面悬浮窗实时显示 CPU / 内存 / GPU / 磁盘 / 网络指标，支持按配置采集到 CSV 并一键生成本地 HTML 深度分析报告。

![license](https://img.shields.io/badge/license-MIT-green.svg)

## 功能特性

- **桌面悬浮窗**：半透明置顶、可拖动、右键菜单，内存双缓冲绘制无闪烁
- **CPU**：使用率、实时频率、温度（AMD Zen 全系 SMN 直读 / Intel MSR 温度）
- **GPU**：使用率、频率、温度（NVIDIA 走 NVML，AMD 走 ADL2 + PMLog，核显也能读到温度）
- **磁盘**：各分区占用率与温度（NVMe / SATA SMART）、全机读/写速率
- **内存**：使用率与用量
- **网络**：实时上下行速率
- **数据记录**：按所选指标与间隔写入 CSV（低优先级后台线程采集，不打扰前台）
- **分析报告**：一键生成单文件 HTML 报告，含统计卡片、趋势图、异常检测
- **温度内核通道**：内嵌 WinRing0 驱动，运行时自动加载（需管理员权限）

## 环境要求

- Windows 10 1809+ / Windows 11
- CPU 温度、磁盘温度需要**管理员权限**运行（启动时自动请求 UAC 提权）
- GPU 指标：NVIDIA 需装 NVML（随驱动自带）；AMD 需装官方显卡驱动

## 从源码构建

```bat
:: 需要 Rust 1.75+（MSVC 工具链）
cargo build --release
```

构建产物为 `target/release/winmon.exe`，单文件、无运行时依赖。`打包.bat` 可完成一键构建并复制到 dist 目录。

## 使用说明

| 操作 | 方式 |
|---|---|
| 显示/隐藏悬浮窗 | 托盘右键菜单或悬浮窗右键菜单 |
| 拖动悬浮窗 | 左键按住拖动，位置自动记忆 |
| 打开设置 | 双击托盘图标 |
| 开始/停止记录 | 右键菜单，或设置窗口按钮 |
| 生成分析报告 | 右键菜单「生成分析报告」 |
| 退出程序 | 右键菜单「退出」 |

- 配置保存在 exe 同目录 `winmon.ini`，悬浮窗位置、显示指标、刷新间隔等自动记忆
- 记录数据写入配置的日志目录（`winmon_<时间戳>.csv`），报告为同名 `.html`
- 命令行自检：`winmon.exe --cpu-test` 输出一轮 CPU/GPU/磁盘指标用于验证数据源

## 常见问题

- **温度显示 N/A**：未以管理员权限运行；或安全软件拦截了 WinRing0 驱动（放行即可）；开启内核隔离（HVCI）时需关闭才能加载驱动
- **GPU 温度 N/A**：无 NVIDIA 显卡时依赖 AMD 驱动（ADL2/PMLog），过老驱动可能不支持
- **生成报告提示没有数据**：先「开始记录」等待一段时间

## 技术实现

纯 Win32 API + GDI 手写界面（无第三方 UI 框架），体积 500KB 左右、常驻内存低。内核态温度读取参考 [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) 的实现（WinRing0 驱动来自其 v0.9.4 发行版）。

## 开源协议

本项目基于 [MIT License](LICENSE) 开源。
