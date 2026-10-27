//! 系统托盘图标

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};


use crate::util;

const TRAY_UID: u32 = 1;

pub fn add(hwnd: HWND) {
    let mut data = NOTIFYICONDATAW::default();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_UID;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = super::WM_APP_TRAY;
    data.hIcon = super::app_icon();
    let tip = util::wide("WinMon 机器监控");
    let n = tip.len().min(data.szTip.len());
    data.szTip[..n].copy_from_slice(&tip[..n]);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
    }
}

pub fn remove(hwnd: HWND) {
    let mut data = NOTIFYICONDATAW::default();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_UID;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}
