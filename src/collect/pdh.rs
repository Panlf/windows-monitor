//! PDH 性能计数器小封装（英文计数器路径，任何系统语言都可用）

use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

use crate::util;

const PDH_MORE_DATA: u32 = 0x8000_07D2;

pub struct PdhQuery {
    query: PDH_HQUERY,
}

impl PdhQuery {
    pub fn new() -> Option<PdhQuery> {
        let mut query = PDH_HQUERY::default();
        let st = unsafe { PdhOpenQueryW(None, 0, &mut query) };
        if st != 0 || query.is_invalid() {
            return None;
        }
        Some(PdhQuery { query })
    }

    /// 按英文计数器路径添加计数器（路径形如 `\GPU Engine(*)\Utilization Percentage`）
    pub fn add_english(&self, path: &str) -> Option<PDH_HCOUNTER> {
        let mut c = PDH_HCOUNTER::default();
        let st = unsafe {
            PdhAddEnglishCounterW(
                self.query,
                windows::core::PCWSTR(util::wide(path).as_ptr()),
                0,
                &mut c,
            )
        };
        if st != 0 || c.is_invalid() {
            return None;
        }
        Some(c)
    }

    /// 每个采样周期调用一次，刷新全部计数器
    pub fn collect(&self) {
        unsafe {
            PdhCollectQueryData(self.query);
        }
    }

    /// 通配符计数器 -> (实例名, 数值) 列表
    pub fn array(&self, c: &PDH_HCOUNTER) -> Vec<(String, f64)> {
        let mut size: u32 = 0;
        let mut count: u32 = 0;
        let st = unsafe { PdhGetFormattedCounterArrayW(*c, PDH_FMT_DOUBLE, &mut size, &mut count, None) };
        if st != 0 && st != PDH_MORE_DATA {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        let st = unsafe {
            PdhGetFormattedCounterArrayW(
                *c,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
            )
        };
        if st != 0 {
            return Vec::new();
        }
        let item_size = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let off = i * item_size;
            if off + item_size > buf.len() {
                break;
            }
            let item: &PDH_FMT_COUNTERVALUE_ITEM_W =
                unsafe { &*(buf.as_ptr().add(off) as *const PDH_FMT_COUNTERVALUE_ITEM_W) };
            // szName 指向 buf 内部：按缓冲区末尾为界扫描到 \0，避免硬编码 256 越界读
            let name_start: *const u16 = item.szName.0;
            let buf_end: *const u16 = unsafe { buf.as_ptr().add(buf.len()) as *const u16 };
            let mut len = 0usize;
            unsafe {
                let mut p = name_start;
                while p < buf_end && *p != 0 {
                    p = p.add(1);
                    len += 1;
                }
            }
            let name = unsafe { util::wide_to_string(std::slice::from_raw_parts(name_start, len)) };
            let val = unsafe { item.FmtValue.Anonymous.doubleValue };
            if val.is_finite() {
                out.push((name, val));
            }
        }
        out
    }
}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}
