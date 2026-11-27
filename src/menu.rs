//! D-Bus Menu（com.canonical.dbusmenu）实现：托盘右键菜单。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use zbus::{interface, zvariant::{Value, StructureBuilder}, Connection};

/// 布局示意图标数据：a(iiay)，BGRA 字节（QImage ARGB32 内存序）。
macro_rules! icon_table {
    ($( $key:literal ),* $(,)?) => {
        fn layout_icon(key: &str, dark: bool) -> Vec<(i32, i32, Vec<u8>)> {
            let theme = if dark { "dark" } else { "light" };
            match (key, theme) {
                $(
                    ($key, "dark") => vec![(22, 22, include_bytes!(
                        concat!("../assets/menu/layout-", $key, "-dark.argb")).to_vec())],
                    ($key, _) => vec![(22, 22, include_bytes!(
                        concat!("../assets/menu/layout-", $key, "-light.argb")).to_vec())],
                )*
                _ => Vec::new(),
            }
        }
    };
}
icon_table!(
    "quarter", "top_bottom", "left_right", "left1_right2", "top1_bottom2",
    "left2_right1", "top2_bottom1", "top3_bottom3", "center_float"
);

/// 菜单项：id、标签、是否可勾选、是否已勾选、是否分隔线、布局示意图标 key。
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub checkable: bool,
    pub checked: bool,
    pub separator: bool,
    /// 非空则该项用布局示意图标代替文字。
    pub icon_key: Option<&'static str>,
}

impl MenuItem {
    fn action(id: i32, label: impl Into<String>) -> Self {
        Self { id, label: label.into(), checkable: false, checked: false, separator: false, icon_key: None }
    }
    fn check(id: i32, label: impl Into<String>, checked: bool) -> Self {
        Self { id, label: label.into(), checkable: true, checked, separator: false, icon_key: None }
    }
}

/// 菜单状态：保存所有项，供 D-Bus 接口查询。
#[derive(Clone)]
pub struct MenuState {
    pub items: Arc<Mutex<Vec<MenuItem>>>,
    pub click_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<i32>>>>,
    /// 菜单内容版本号，每次重建 +1；托盘据此判断是否需要重新拉 GetLayout
    revision: Arc<AtomicU32>,
    last_shown: Arc<AtomicU32>,
    /// 深色主题：布局示意图标选白/黑
    dark: Arc<AtomicBool>,
}

impl MenuState {
    pub fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
            click_tx: Arc::new(Mutex::new(None)),
            revision: Arc::new(AtomicU32::new(1)),
            last_shown: Arc::new(AtomicU32::new(0)),
            dark: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_dark(&self, dark: bool) {
        self.dark.store(dark, Ordering::SeqCst);
    }

    pub fn set_items(&self, items: Vec<MenuItem>) {
        *self.items.lock().unwrap() = items;
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    pub fn revision(&self) -> u32 {
        self.revision.load(Ordering::SeqCst)
    }

    pub fn emit_click(&self, id: i32) {
        if let Some(tx) = self.click_tx.lock().unwrap().as_ref() {
            let _ = tx.send(id);
        }
    }
}

pub struct DbusMenu {
    state: MenuState,
}

#[interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    /// 返回菜单布局。
    /// GetLayout(i parentId, i recursionDepth, as propertyNames)
    ///     -> (u revision, (ia{sv}av) layout)
    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (
        u32,
        (i32, HashMap<String, Value<'static>>, Vec<Value<'static>>),
    ) {
        let items = self.state.items.lock().unwrap();
        let dark = self.state.dark.load(Ordering::SeqCst);
        let mut children: Vec<Value<'static>> = Vec::new();
        for item in items.iter() {
            let mut props: HashMap<String, Value<'static>> = HashMap::new();
            if item.separator {
                props.insert("type".into(), Value::new("separator"));
            } else {
                if let Some(key) = item.icon_key {
                    // 纯图示：不设 label，用 icon-data 位图 + icon-name 兜底
                    props.insert("type".into(), Value::new("standard"));
                    props.insert("icon-data".into(), Value::new(layout_icon(key, dark)));
                } else {
                    props.insert("label".into(), Value::new(item.label.clone()));
                    props.insert("type".into(), Value::new("standard"));
                }
                if item.checkable {
                    props.insert("toggle-type".into(), Value::new("checkmark"));
                    props.insert(
                        "toggle-state".into(),
                        Value::new(if item.checked { 1i32 } else { 0i32 }),
                    );
                }
                props.insert("enabled".into(), Value::new(true));
            }
            let child: Value<'static> = Value::new(
                StructureBuilder::new()
                    .add_field(item.id)
                    .add_field(props)
                    .add_field(Vec::<Value<'static>>::new())
                    .build()
                    .unwrap(),
            );
            children.push(child);
        }
        (self.state.revision(), (0i32, HashMap::new(), children))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, Value<'static>>)> {
        let items = self.state.items.lock().unwrap();
        let dark = self.state.dark.load(Ordering::SeqCst);
        ids.iter()
            .filter_map(|id| {
                let item = items.iter().find(|i| i.id == *id)?;
                let mut props = HashMap::new();
                if let Some(key) = item.icon_key {
                    props.insert("icon-data".into(), Value::new(layout_icon(key, dark)));
                } else {
                    props.insert("label".into(), Value::new(item.label.clone()));
                }
                Some((*id, props))
            })
            .collect()
    }

    fn event(
        &self,
        id: i32,
        event_id: &str,
        _data: Value<'_>,
        _timestamp: u32,
    ) {
        if event_id == "clicked" {
            self.state.emit_click(id);
        }
    }

    fn about_to_show(&self, id: i32) -> bool {
        if id != 0 {
            return false;
        }
        let rev = self.state.revision();
        let last = self.state.last_shown.swap(rev, Ordering::SeqCst);
        rev != last
    }

    /// 返回单个菜单项属性（规范 GetProperty(i, s) -> v）。
    fn get_property(&self, id: i32, name: &str) -> Value<'static> {
        let items = self.state.items.lock().unwrap();
        let dark = self.state.dark.load(Ordering::SeqCst);
        if let Some(item) = items.iter().find(|i| i.id == id) {
            return match name {
                "label" => Value::new(item.label.clone()),
                "icon-data" if item.icon_key.is_some() => {
                    Value::new(layout_icon(item.icon_key.unwrap(), dark))
                }
                "enabled" => Value::new(true),
                "type" => Value::new(if item.separator {
                    "separator"
                } else {
                    "standard"
                }),
                "toggle-type" if item.checkable => Value::new("checkmark"),
                "toggle-state" if item.checkable => {
                    Value::new(if item.checked { 1i32 } else { 0i32 })
                }
                _ => Value::new(String::new()),
            };
        }
        Value::new(String::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
}

/// 发布 D-Bus Menu 对象，返回菜单状态。
pub async fn publish(conn: &Connection, path: &str) -> zbus::Result<MenuState> {
    let state = MenuState::new();
    let menu = DbusMenu { state: state.clone() };
    conn.object_server().at(path, menu).await?;
    Ok(state)
}

/// 菜单 ID 常量（与 menu.rs 中的定义对应）。
pub mod ids {
    pub const RELOAD_CONFIG: i32 = 60;
    pub const AUTOSTART: i32 = 62;
    /// 托盘菜单第一项：打开统一的窗口排列面板
    pub const WINDOW_MANAGER: i32 = 50;
    /// 打开统一的「布局管理」窗口（保存 / 加载 / 删除 / 重命名）
    pub const LAYOUT_MANAGER: i32 = 51;
    /// 软件更新：检查 Gitee / GitHub Release，下载并更新。
    pub const UPDATE: i32 = 70;
    pub const QUIT: i32 = 99;
}

/// 托盘菜单：固定五项（窗口排列 / 布局管理 / 开机启动 / 软件更新 / 退出软件）。
/// 保存、加载、删除、重命名布局全部收进统一的「布局管理」窗口。
pub fn build_menu_items(_snapshots: &[crate::model::Snapshot], autostart: bool) -> Vec<MenuItem> {
    let mut items = Vec::new();
    items.push(MenuItem::action(ids::WINDOW_MANAGER, "窗口排列"));
    items.push(MenuItem::action(ids::LAYOUT_MANAGER, "布局管理"));
    items.push(MenuItem::check(ids::AUTOSTART, "开机启动", autostart));
    items.push(MenuItem::action(ids::UPDATE, "软件更新"));
    items.push(MenuItem::action(ids::QUIT, "退出软件"));
    items
}
