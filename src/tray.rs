//! SNI（Status Notifier Item）托盘图标实现。

use crate::menu;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use zbus::{interface, zvariant::ObjectPath, Connection};

pub struct StatusNotifierItem {
    title: String,
    icon_name: String,
    dark: bool,
    /// 左键激活通道：单击/双击都投递「窗口排列」。
    click_tx: Arc<Mutex<Option<UnboundedSender<i32>>>>,
}

/// 精确像素的图标数据（QImage::Format_ARGB32，little-endian 内存序 B,G,R,A）。
/// 直接给托盘，绕开 QIcon::fromTheme 的缩放/缓存，保证 2px 纯黑描边清晰锐利。
macro_rules! argb {
    ($name:literal, $size:literal) => {
        ($size, include_bytes!(concat!("../assets/argb/status-", $name, "-", $size, ".argb")).as_slice())
    };
}
// 14px 优先；16px 为主位图：14x14 黑框、2px 中缝、四周各 1px 留白，
// 与相邻系统图标渲染高度一致；用户实测后回退到 0.5.6 这套几何。
static LIGHT_ICONS: &[(i32, &[u8])] = &[
    argb!("light", 14), argb!("light", 16), argb!("light", 22), argb!("light", 24),
    argb!("light", 32), argb!("light", 48),
];
static DARK_ICONS: &[(i32, &[u8])] = &[
    argb!("dark", 14), argb!("dark", 16), argb!("dark", 22), argb!("dark", 24),
    argb!("dark", 32), argb!("dark", 48),
];

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    fn id(&self) -> String {
        "lithezones".to_string()
    }

    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.title.clone()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".to_string()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.icon_name.clone()
    }

    /// 直接提供各尺寸位图，托盘无需按主题名缩放 PNG（缩放会让 2px 描边变成灰边）。
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let src = if self.dark { DARK_ICONS } else { LIGHT_ICONS };
        src.iter()
            .map(|(size, data)| (*size, *size, data.to_vec()))
            .collect()
    }

    /// 额外图标搜索路径。带上它 QIcon::fromTheme 才能找到我们安装的
    /// scalable SVG，作为 IconPixmap 之外的兜底。
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if let Ok(home) = std::env::var("HOME") {
            paths.push(format!("{}/.local/share/icons", home));
        }
        paths.push("/usr/share/icons".to_string());
        paths
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str("/Menu").unwrap()
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// 左键单击/双击都直接打开窗口排列。
    /// 之前的"第一次点击等 320ms、没等到第二次就返回错误让托盘弹菜单"方案，
    /// 在真实 DDE 托盘里表现为单击不弹窗；用户要的是单击就进窗口排列。
    /// 双击会来两次 Activate，handle_menu_click 里已有"面板已开则切标签并置前"
    /// 的去重逻辑，不会开出第二个窗口。
    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        if let Some(tx) = self.click_tx.lock().unwrap().as_ref() {
            let _ = tx.send(menu::ids::WINDOW_MANAGER);
        }
        Ok(())
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}
}

/// 发布 SNI 对象并注册到 Watcher。
pub async fn publish(
    conn: &Connection,
    path: &str,
    bus_name: &str,
    dark: bool,
    click_tx: Arc<Mutex<Option<UnboundedSender<i32>>>>,
) -> Result<()> {
    // 按用户要求回到最原始的托盘图标名：0.3.5～0.3.7 使用的 dde-layout-status-*。
    // 图标图案仍是用户选定的 0.3.7 原图；包内不再附带 status2/status3 等重复名字。
    let icon_name = if dark {
        "dde-layout-status-dark"
    } else {
        "dde-layout-status-light"
    }
    .to_string();
    let item = StatusNotifierItem {
        title: "轻格分窗".to_string(),
        icon_name,
        dark,
        click_tx,
    };
    conn.object_server().at(path, item).await?;

    conn.call_method(
        Some("org.kde.StatusNotifierWatcher"),
        "/StatusNotifierWatcher",
        Some("org.kde.StatusNotifierWatcher"),
        "RegisterStatusNotifierItem",
        &(bus_name,),
    )
    .await?;
    Ok(())
}
