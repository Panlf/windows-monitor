//! 网络上下行速率（全部活动网卡计数差值）

use std::collections::HashMap;
use std::time::Instant;

use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;

pub struct NetState {
    prev: HashMap<u32, (u64, u64)>, // ifIndex -> (inOctets, outOctets)
    prev_t: Instant,
}

impl NetState {
    pub fn new() -> NetState {
        NetState {
            prev: HashMap::new(),
            prev_t: Instant::now(),
        }
    }

    /// 返回 (下行 B/s, 上行 B/s)；首次调用返回 (0, 0)
    pub fn rates(&mut self) -> (u64, u64) {
        let mut cur: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let st = unsafe { GetIfTable2(&mut table) };
        if st.0 != 0 || table.is_null() {
            return (0, 0);
        }
        unsafe {
            let n = (*table).NumEntries as usize;
            for i in 0..n {
                let row = &*(*table).Table.as_ptr().add(i);
                if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                    continue;
                }
                if row.OperStatus != IfOperStatusUp {
                    continue;
                }
                cur.insert(row.InterfaceIndex, (row.InOctets, row.OutOctets));
            }
            FreeMibTable(table as *const core::ffi::c_void);
        }

        let now = Instant::now();
        let dt = now.duration_since(self.prev_t).as_secs_f64();
        let prev = std::mem::replace(&mut self.prev, cur);
        self.prev_t = now;
        if prev.is_empty() || dt <= 0.0 {
            return (0, 0);
        }

        let mut down: u64 = 0;
        let mut up: u64 = 0;
        for (idx, (inn, out)) in &self.prev {
            if let Some((pin, pout)) = prev.get(idx) {
                down += inn.saturating_sub(*pin);
                up += out.saturating_sub(*pout);
            }
        }
        ((down as f64 / dt) as u64, (up as f64 / dt) as u64)
    }
}
