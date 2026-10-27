//! WinRing0 内核驱动封装（实现参考 LibreHardwareMonitor 的 Ring0/KernelDriver，MPL-2.0）
//!
//! 用途：用户态进程通过内核驱动读 MSR / PCI 配置空间。
//! AMD Zen 系列的 CPU 温度（Tctl）只能通过 SMN 寄存器读取，而 SMN 通道
//! （PCI 0:0:0 偏移 0x60/0x64）必须内核权限，WMI/PDH 用户态接口都拿不到。
//!
//! 权限要求：管理员。驱动文件内嵌于本模块（include_bytes!），
//! 首次使用时释放到 exe 同目录，注册为内核驱动服务后打开设备。
//! 设备名固定为 WinRing0_1_2_0（驱动二进制内部创建的名字，与 LHM 一致）。

use windows::Win32::Foundation::{CloseHandle, HANDLE, ERROR_SERVICE_ALREADY_RUNNING};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, StartServiceW, SERVICE_ALL_ACCESS, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
    SERVICE_ERROR_NORMAL, SERVICE_KERNEL_DRIVER, SERVICE_STATUS, SC_MANAGER_CREATE_SERVICE,
};

/// 内嵌驱动（来自 LibreHardwareMonitor v0.9.4 资源，SHA256
/// 11bd2c9f9e2397c9a16e0990e4ed2cf0679498fe0fd418a3dfdac60b5c160ee5）
const DRIVER_SYS: &[u8] = include_bytes!("WinRing0x64.sys");

/// 驱动设备名（WinRing0 驱动内部写死的符号链接名）
const DEVICE_PATH: &str = "\\\\.\\WinRing0_1_2_0";

// OpenLibSys OLS 驱动的 IOCTL 码：CTL_CODE(40000, func, access)
// = (40000 << 16) | (access << 14) | (func << 2) | METHOD_BUFFERED
const IOCTL_OLS_READ_MSR: u32 = (40000 << 16) | (0x821 << 2); // 0x9C402084
const IOCTL_OLS_READ_PCI_CONFIG: u32 =
    (40000 << 16) | (1 << 14) | (0x851 << 2); // 0x9C406144
const IOCTL_OLS_WRITE_PCI_CONFIG: u32 =
    (40000 << 16) | (2 << 14) | (0x852 << 2); // 0x9C40A148

#[repr(C)]
struct ReadPciIn {
    pci_address: u32,
    reg_address: u32,
}

#[repr(C)]
struct WritePciIn {
    pci_address: u32,
    reg_address: u32,
    value: u32,
}

pub struct Ring0 {
    device: HANDLE,
}

impl Ring0 {
    /// 打开驱动；未加载则安装并启动。失败返回原因描述（写入诊断日志用）。
    pub fn open() -> Result<Ring0, String> {
        if let Some(r) = Ring0::try_open() {
            return Ok(r);
        }
        Ring0::install()?;
        match Ring0::try_open() {
            Some(r) => Ok(r),
            None => Err("驱动已安装但打开设备失败".to_string()),
        }
    }

    fn try_open() -> Option<Ring0> {
        unsafe {
            let h = CreateFileW(
                windows::core::PCWSTR(util::wide(DEVICE_PATH).as_ptr()),
                0xC000_0000, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .ok();
            h.map(|device| Ring0 { device })
        }
    }

    /// 释放驱动文件 + 注册/启动内核驱动服务
    fn install() -> Result<(), String> {
        let sys_path = extract_driver()?;

        // 服务名沿用 LHM 规则：R0 + exe 名（避免与其他工具冲突）
        let exe_stem = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "winmon".to_string());
        let service_name = format!(
            "R0{}",
            exe_stem
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
        );

        unsafe {
            let mgr = OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE)
                .map_err(|e| format!("OpenSCManager 失败（需管理员权限）: {e}"))?;

            // 先清掉同名旧服务，保证 ImagePath 指向本次释放的驱动文件
            if let Ok(old) = OpenServiceW(mgr, PCWSTR(wide(&service_name).as_ptr()), SERVICE_ALL_ACCESS) {
                let mut st = SERVICE_STATUS::default();
                let _ = ControlService(old, SERVICE_CONTROL_STOP, &mut st);
                let _ = DeleteService(old);
                let _ = CloseServiceHandle(old);
            }

            let svc = CreateServiceW(
                mgr,
                PCWSTR(wide(&service_name).as_ptr()),
                PCWSTR(wide(&service_name).as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_KERNEL_DRIVER,
                SERVICE_DEMAND_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(wide(&sys_path.to_string_lossy()).as_ptr()),
                None,
                None,
                None,
                None,
                None,
            )
            .map_err(|e| format!("CreateService({service_name}) 失败: {e}"))?;

            if let Err(e) = StartServiceW(svc, None) {
                if e.code() != windows::core::HRESULT(ERROR_SERVICE_ALREADY_RUNNING.0 as i32) {
                    let _ = DeleteService(svc);
                    let _ = CloseServiceHandle(svc);
                    let _ = CloseServiceHandle(mgr);
                    return Err(format!(
                        "StartService 失败: {e}（可能被安全软件/内核隔离拦截）"
                    ));
                }
            }
            let _ = CloseServiceHandle(svc);
            let _ = CloseServiceHandle(mgr);
        }
        Ok(())
    }

    /// 读 64 位 MSR
    pub fn read_msr(&self, index: u32) -> Option<u64> {
        let mut out: u64 = 0;
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_OLS_READ_MSR,
                Some(&index as *const u32 as *const core::ffi::c_void),
                4,
                Some(&mut out as *mut u64 as *mut core::ffi::c_void),
                8,
                Some(&mut ret),
                None,
            )
            .ok()?;
        }
        Some(out)
    }

    /// 读 PCI 配置空间 32 位（pci_address = (bus<<8)|(dev<<3)|fn）
    pub fn read_pci_config(&self, pci_address: u32, reg_address: u32) -> Option<u32> {
        let inp = ReadPciIn {
            pci_address,
            reg_address,
        };
        let mut out: u32 = 0;
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_OLS_READ_PCI_CONFIG,
                Some(&inp as *const ReadPciIn as *const core::ffi::c_void),
                std::mem::size_of::<ReadPciIn>() as u32,
                Some(&mut out as *mut u32 as *mut core::ffi::c_void),
                4,
                Some(&mut ret),
                None,
            )
            .ok()?;
        }
        Some(out)
    }

    /// 写 PCI 配置空间 32 位（AMD SMN 访问入口：向 0x60 写 SMN 地址）
    pub fn write_pci_config(&self, pci_address: u32, reg_address: u32, value: u32) -> bool {
        let inp = WritePciIn {
            pci_address,
            reg_address,
            value,
        };
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_OLS_WRITE_PCI_CONFIG,
                Some(&inp as *const WritePciIn as *const core::ffi::c_void),
                std::mem::size_of::<WritePciIn>() as u32,
                None,
                0,
                None,
                None,
            )
            .is_ok()
        }
    }
}

impl Drop for Ring0 {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.device);
        }
    }
}

/// 释放驱动文件到磁盘，返回路径
fn extract_driver() -> Result<std::path::PathBuf, String> {
    let path = util::exe_dir().join("winmon_ring0.sys");
    let target = if std::fs::metadata(&path).map(|m| m.len() == DRIVER_SYS.len() as u64).unwrap_or(false) {
        path
    } else if std::fs::write(&path, DRIVER_SYS).is_ok() {
        path
    } else {
        // exe 目录不可写时退回临时目录
        let p = std::env::temp_dir().join("winmon_ring0.sys");
        std::fs::write(&p, DRIVER_SYS).map_err(|e| format!("释放驱动文件失败: {e}"))?;
        p
    };
    Ok(target)
}

use crate::util;
use windows::core::PCWSTR;
fn wide(s: &str) -> Vec<u16> {
    util::wide(s)
}
