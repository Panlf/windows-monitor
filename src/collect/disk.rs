//! 各磁盘占用率与温度
//!
//! 温度获取顺序：
//! 1) IOCTL_STORAGE_QUERY_PROPERTY / StorageDeviceTemperatureProperty（NVMe 与多数新盘，摄氏度）
//! 2) ATA PASS THROUGH / SMART READ DATA（SATA 盘，温度属性 0xC2/0xBF/0xBB/0x194）
//! 两者都需要管理员权限，失败时温度为 None（界面显示 N/A）。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, FILE_ATTRIBUTE_NORMAL,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Performance::PDH_HCOUNTER;
use windows::core::PCWSTR;

/// GetDriveTypeW 返回值：固定磁盘
const DRIVE_FIXED: u32 = 3;

use super::pdh::PdhQuery;
use crate::metrics::DiskSample;
use crate::util;

// ---- 手工定义的存储 IOCTL 常量与结构（Windows SDK ntddstor.h / ntddscsi.h）----

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x2D_1400;
const IOCTL_ATA_PASS_THROUGH: u32 = 0x4D_02C;
/// STORAGE_PROPERTY_ID::StorageDeviceTemperatureProperty = 52
const STORAGE_DEVICE_TEMPERATURE_PROPERTY: u32 = 52;
/// STORAGE_QUERY_TYPE::PropertyStandardQuery = 0
const PROPERTY_STANDARD_QUERY: u32 = 0;
/// 温度未上报的哨兵值（0x8000 为 SHORT 最小值）
const TEMPERATURE_VALUE_NOT_REPORTED: i16 = -32768;
/// ATA_FLAGS_DATA_IN
const ATA_FLAGS_DATA_IN: u16 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StorageTemperatureInfo {
    index: u16,
    temperature: i16,
    over_threshold: i16,
    under_threshold: i16,
    over_changable: u8,
    under_changable: u8,
    event_generated: u8,
    reserved0: u8,
    reserved1: u32,
}

#[repr(C)]
struct StorageTemperatureDataDescriptor {
    version: u32,
    size: u32,
    critical_temperature: i16,
    warning_temperature: i16,
    info_count: u16,
    reserved0: [u8; 2],
    reserved1: [u32; 2],
    temperature_info: [StorageTemperatureInfo; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AtaPassThroughEx {
    length: u16,
    ata_flags: u16,
    path_id: u8,
    target_id: u8,
    lun: u8,
    reserved_uchar: u8,
    data_transfer_length: u32,
    time_out_value: u32,
    reserved_ulong: u32,
    data_buffer_offset: usize,
    previous_task_file: [u8; 8],
    current_task_file: [u8; 8],
}

/// 温度刷新周期（秒）：SMART/存储属性查询有一定开销，不必每秒都查
const TEMP_REFRESH_SECS: u64 = 10;

/// 全机磁盘读/写速率（PDH 物理磁盘字节速率计数器，所有物理盘合计）
struct IoCounters {
    q: PdhQuery,
    read: PDH_HCOUNTER,
    write: PDH_HCOUNTER,
}

impl IoCounters {
    fn open() -> Option<IoCounters> {
        let q = PdhQuery::new()?;
        let read = q.add_english(r"\PhysicalDisk(*)\Disk Read Bytes/sec")?;
        let write = q.add_english(r"\PhysicalDisk(*)\Disk Write Bytes/sec")?;
        q.collect(); // 速率计数器需要两次采集才有有效差值
        Some(IoCounters { q, read, write })
    }

    /// 返回 (读速率, 写速率)，字节/秒
    fn sample(&self) -> (u64, u64) {
        self.q.collect();
        let sum = |c: &PDH_HCOUNTER| -> u64 {
            self.q
                .array(c)
                .into_iter()
                // 通配符实例里含 "_Total"，合计时应排除以免翻倍
                .filter(|(name, _)| name != "_Total")
                .map(|(_, v)| v.max(0.0) as u64)
                .sum()
        };
        (sum(&self.read), sum(&self.write))
    }
}

pub struct DiskState {
    drives: Vec<u8>,                    // 盘符字母，如 C、D
    temps: Vec<Option<f32>>,
    last_temp: Option<Instant>,
    io: Option<IoCounters>,
}

impl DiskState {
    pub fn new() -> DiskState {
        let io = match IoCounters::open() {
            Some(c) => {
                util::debug_log("磁盘IO: PDH 物理磁盘读写计数器可用");
                Some(c)
            }
            None => {
                util::debug_log("磁盘IO: PDH 计数器打开失败，读写速率将显示 0");
                None
            }
        };
        DiskState {
            drives: enumerate_fixed_drives(),
            temps: Vec::new(),
            last_temp: None,
            io,
        }
    }

    /// 返回 (各磁盘状态, 全机读速率 bps, 全机写速率 bps)
    pub fn sample(&mut self) -> (Vec<DiskSample>, u64, u64) {
        // 插拔磁盘后重新枚举（最多每 60 秒一次的成本很低）
        let drives = enumerate_fixed_drives();
        if drives != self.drives {
            self.drives = drives;
            self.temps.clear();
            self.last_temp = None;
        }

        let need_temp = match self.last_temp {
            Some(t) => t.elapsed() >= Duration::from_secs(TEMP_REFRESH_SECS),
            None => true,
        };
        if need_temp {
            self.temps = self.drives.iter().map(|&d| read_disk_temp(d)).collect();
            self.last_temp = Some(Instant::now());
        }

        let (read_bps, write_bps) = self.io.as_ref().map(|io| io.sample()).unwrap_or((0, 0));

        let mut out = Vec::with_capacity(self.drives.len());
        for (i, &d) in self.drives.iter().enumerate() {
            let (used_pct, total_gb, free_gb) = disk_usage(d);
            out.push(DiskSample {
                name: format!("{}", d as char),
                used_pct,
                total_gb,
                free_gb,
                temp_c: self.temps.get(i).copied().flatten(),
            });
        }
        (out, read_bps, write_bps)
    }
}

fn enumerate_fixed_drives() -> Vec<u8> {
    let mut buf = vec![0u16; 256];
    let n = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return Vec::new();
    }
    let mut drives = Vec::new();
    let mut start = 0;
    for i in 0..n {
        if buf[i] == 0 {
            let s = util::wide_to_string(&buf[start..i]);
            start = i + 1;
            if s.len() >= 2 {
                let w = util::wide(&s);
                let typ = unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) };
                if typ == DRIVE_FIXED {
                    drives.push(s.as_bytes()[0].to_ascii_uppercase());
                }
            }
        }
    }
    drives
}

fn disk_usage(letter: u8) -> (f32, f32, f32) {
    let root = util::wide(&format!("{}:\\", letter as char));
    let mut avail: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(root.as_ptr()),
            Some(&mut avail),
            Some(&mut total),
            Some(&mut free),
        )
    }
    .is_ok();
    if !ok || total == 0 {
        return (0.0, 0.0, 0.0);
    }
    let used = total.saturating_sub(free) as f64;
    (
        (used / total as f64 * 100.0) as f32,
        (total as f64 / 1073741824.0) as f32,
        (free as f64 / 1073741824.0) as f32,
    )
}

/// 打开卷句柄（如 `\\.\C:`）
fn open_volume(letter: u8) -> Option<HANDLE> {
    let path = util::wide(&format!("\\\\.\\{}:", letter as char));
    let h = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0x80000000u32, // GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
            None,
        )
    };
    match h {
        Ok(h) if !h.is_invalid() => Some(h),
        _ => None,
    }
}

fn close_handle(h: HANDLE) {
    unsafe {
        let _ = CloseHandle(h);
    }
}

/// 读取磁盘温度（摄氏度）；读不到返回 None
fn read_disk_temp(letter: u8) -> Option<f32> {
    let h = open_volume(letter)?;
    let t = storage_property_temp(h).or_else(|| ata_smart_temp(h));
    close_handle(h);
    t
}

/// 1) 存储设备温度属性（Win10 1809+，NVMe/SATA 均可）
fn storage_property_temp(h: HANDLE) -> Option<f32> {
    let query = StoragePropertyQuery {
        property_id: STORAGE_DEVICE_TEMPERATURE_PROPERTY,
        query_type: PROPERTY_STANDARD_QUERY,
        additional_parameters: [0],
    };
    let mut out = vec![0u8; 4096];
    let mut returned: u32 = 0;
    let ok = unsafe {
        windows::Win32::System::IO::DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const core::ffi::c_void),
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            Some(out.as_mut_ptr() as *mut core::ffi::c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .is_ok();
    if !ok || returned < 24 {
        return None;
    }
    let desc = unsafe { &*(out.as_ptr() as *const StorageTemperatureDataDescriptor) };
    if desc.info_count == 0 {
        return None;
    }
    // 取所有传感器中有效的最高温度（Index 0 多为综合温度）。
    // 注意：不直接访问 desc.temperature_info[i]——结构里该数组长度固定为 1，
    //       运行时下标越界会 panic 崩掉整个采样线程；改为在缓冲区上按偏移读取。
    let info_size = std::mem::size_of::<StorageTemperatureInfo>();
    let info_field_off = std::mem::offset_of!(StorageTemperatureDataDescriptor, temperature_info);
    let base = out.as_ptr() as usize + info_field_off;
    // 可用条目数受缓冲区内剩余空间限制，也受驱动上报的 info_count 限制
    let avail = (out.len() - (info_field_off + info_size)) / info_size + 1;
    let count = (desc.info_count as usize).min(avail);

    let mut best: Option<f32> = None;
    for i in 0..count {
        let p = base + i * info_size;
        let info = unsafe { &*(p as *const StorageTemperatureInfo) };
        if info.temperature == TEMPERATURE_VALUE_NOT_REPORTED {
            continue;
        }
        let v = info.temperature as f32;
        if (-40.0..=150.0).contains(&v) {
            best = Some(best.map_or(v, |b: f32| b.max(v)));
        }
    }
    best
}

/// 2) ATA SMART 属性温度（SATA 盘）
fn ata_smart_temp(h: HANDLE) -> Option<f32> {
    const ATA_STRUCT_LEN: usize = std::mem::size_of::<AtaPassThroughEx>();
    const DATA_LEN: usize = 512;
    let mut buf = vec![0u8; ATA_STRUCT_LEN + DATA_LEN];
    {
        let apt = unsafe { &mut *(buf.as_mut_ptr() as *mut AtaPassThroughEx) };
        apt.length = ATA_STRUCT_LEN as u16;
        apt.ata_flags = ATA_FLAGS_DATA_IN;
        apt.data_transfer_length = DATA_LEN as u32;
        apt.time_out_value = 5;
        apt.data_buffer_offset = ATA_STRUCT_LEN;
        // SMART READ DATA：feature=0xD0, count=1, LBA mid=0x4F, LBA high=0xC2, command=0xB0
        apt.current_task_file = [0xD0, 0x01, 0x00, 0x4F, 0xC2, 0xA0, 0xB0, 0x00];
    }
    let mut returned: u32 = 0;
    let ok = unsafe {
        windows::Win32::System::IO::DeviceIoControl(
            h,
            IOCTL_ATA_PASS_THROUGH,
            Some(buf.as_ptr() as *const core::ffi::c_void),
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .is_ok();
    if !ok {
        return None;
    }
    // 512 字节 SMART 数据从 ATA 结构后开始，第 2 字节起为 30 个属性（每条 12 字节）
    let data = &buf[ATA_STRUCT_LEN..ATA_STRUCT_LEN + DATA_LEN];
    // 温度相关 SMART 属性：0xC2(194 温度)、0xBE(190 气流温度)、0xBF(191)、0xBB(187)
    const TEMP_ATTRS: [u8; 4] = [0xC2, 0xBE, 0xBF, 0xBB];
    for i in 0..30usize {
        let off = 2 + i * 12;
        if off + 11 >= data.len() {
            break;
        }
        let id = data[off];
        if id == 0 || !TEMP_ATTRS.contains(&id) {
            continue;
        }
        let raw = (data[off + 5] as u16) | ((data[off + 6] as u16) << 8);
        let v = raw as f32;
        if (1.0..=150.0).contains(&v) {
            return Some(v);
        }
    }
    None
}
