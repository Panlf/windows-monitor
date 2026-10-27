//! 后台采样线程：低优先级、按配置间隔采集，负责记录 CSV 与发布最新采样值

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST};

use crate::collect::Collector;
use crate::metrics::{Metric, Sample};
use crate::recorder::Recorder;
use crate::util;

/// 采样线程命令
pub enum Cmd {
    Start {
        dir: String,
        interval: u32,
        metrics: Vec<Metric>,
    },
    Stop,
}

/// 记录状态（供界面显示）
pub enum Status {
    Idle,
    Recording {
        rows: u64,
    },
    Error(String),
}

pub struct Shared {
    pub latest: Mutex<Option<Sample>>,
    pub cmds: Mutex<Vec<Cmd>>,
    pub status: Mutex<Status>,
    pub refresh_secs: AtomicU32,
    pub running: AtomicBool,
}

impl Shared {
    pub fn new(refresh_secs: u32) -> Arc<Shared> {
        Arc::new(Shared {
            latest: Mutex::new(None),
            cmds: Mutex::new(Vec::new()),
            status: Mutex::new(Status::Idle),
            refresh_secs: AtomicU32::new(refresh_secs.clamp(1, 3600)),
            running: AtomicBool::new(true),
        })
    }

    pub fn push_cmd(&self, cmd: Cmd) {
        self.cmds.lock().unwrap().push(cmd);
    }

    pub fn latest(&self) -> Option<Sample> {
        self.latest.lock().unwrap().clone()
    }
}

/// 启动采样线程
pub fn spawn(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        unsafe {
            // 低于普通优先级，避免影响前台应用
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
        }
        let mut collector = Collector::new();
        let mut recorder: Option<Recorder> = None;

        while shared.running.load(Ordering::Relaxed) {
            // 1. 处理控制命令
            let cmds: Vec<Cmd> = std::mem::take(&mut *shared.cmds.lock().unwrap());
            for cmd in cmds {
                match cmd {
                    Cmd::Start {
                        dir,
                        interval,
                        metrics,
                    } => {
                        drop_recorder(&shared, recorder.take());
                        let disks = disk_names(&mut collector);
                        match Recorder::start(&dir, interval, &metrics, &disks) {
                            Ok(r) => {
                                *shared.status.lock().unwrap() = Status::Recording { rows: 0 };
                                recorder = Some(r);
                            }
                            Err(e) => {
                                *shared.status.lock().unwrap() = Status::Error(e);
                            }
                        }
                    }
                    Cmd::Stop => {
                        drop_recorder(&shared, recorder.take());
                        *shared.status.lock().unwrap() = Status::Idle;
                    }
                }
            }

            // 2. 采集。捕获 panic：单次底层读取异常不阻断录制，避免整个采样线程崩溃。
            let now = util::now_unix();
            let s = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                collector.sample()
            }))
            .unwrap_or_else(|_| Sample::empty(now));

            // 3. 记录（内部按记录间隔节流）
            if let Some(r) = recorder.as_mut() {
                r.write_if_due(&s);
                *shared.status.lock().unwrap() = Status::Recording { rows: r.rows() };
            }

            // 4. 发布最新值
            *shared.latest.lock().unwrap() = Some(s);

            // 5. 按刷新间隔休眠；记录间隔更小时用更短周期
            let mut tick = shared.refresh_secs.load(Ordering::Relaxed).max(1);
            if recorder.is_some() {
                tick = tick.min(1);
            }
            std::thread::sleep(std::time::Duration::from_secs(tick as u64));
        }
        drop_recorder(&shared, recorder.take());
    })
}

fn drop_recorder(shared: &Shared, r: Option<Recorder>) {
    if r.is_some() {
        *shared.status.lock().unwrap() = Status::Idle;
    }
    drop(r);
    let _ = shared;
}

/// 采集一轮拿到当前盘符列表（用于建立 CSV 磁盘列）
fn disk_names(collector: &mut Collector) -> Vec<String> {
    collector
        .sample()
        .disks
        .into_iter()
        .map(|d| d.name)
        .collect()
}
