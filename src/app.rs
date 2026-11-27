//! 单实例控制：通过 D-Bus well-known name 保证同时只有一个实例常驻，
//! 并提供 Quit() 方法供 `lithezones --quit` 远程通知退出。

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use zbus::interface;

/// 应用级 well-known name（不带连字符，避免名称校验歧义）。
pub const APP_BUS_NAME: &str = "com.lithezones.app";
pub const APP_PATH: &str = "/com/lithezones/app";
pub const APP_IFACE: &str = "com.lithezones.app.Application";

pub type QuitSender = Arc<Mutex<Option<UnboundedSender<()>>>>;

pub struct AppControl {
    quit_tx: QuitSender,
    identity: String,
}

impl AppControl {
    pub fn new(quit_tx: QuitSender, identity: String) -> Self {
        Self { quit_tx, identity }
    }
}

#[interface(name = "com.lithezones.app.Application")]
impl AppControl {
    /// 由 `lithezones --quit` 调用，通知主实例结束事件循环。
    fn quit(&self) {
        if let Some(tx) = self.quit_tx.lock().unwrap().as_ref() {
            let _ = tx.send(());
        }
    }

    /// 探活接口，便于诊断单实例是否真的在跑。
    fn ping(&self) -> String {
        "pong".to_string()
    }

    /// 实例身份：可执行文件绝对路径 + 版本。用于区分"真的本程序实例"
    /// 与残留的旧二进制/Ping 幽灵，不依赖 /proc 的 PID（跨 PID 命名空间会错位）。
    fn identity(&self) -> String {
        self.identity.clone()
    }
}
