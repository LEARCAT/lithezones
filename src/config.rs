//! 配置读写：JSON 文件，原子写入 + 备份。

use crate::model::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub use_workarea: bool,
    pub frame_compensation: bool,
    pub all_desktops: bool,
    pub minimize_extra_windows: bool,
    pub snapshot_restore_mode: String, // "relative" | "absolute"
    pub theme: String, // "auto" | "light" | "dark"
    pub hotkeys: HashMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        let mut hotkeys = HashMap::new();
        // 0.5.43：用户确认「直接套布局」的快捷键已无实际意义，改为打开面板。
        // 0.5.48：Super+1~9 被 deepin 占用为「切换工作区」、Super+Shift+1~9 为
        // 「移动到工作区」，面板入口和它冲突；改用系统完全空闲的 Alt+1/Alt+2。
        // 布局类键默认留空，代码仍支持手动配置恢复旧行为。
        hotkeys.insert("panel_window".into(), "Alt+1".into());
        hotkeys.insert("panel_layout".into(), "Alt+2".into());
        for name in [
            "layout_quarter", "layout_top_bottom", "layout_left_right",
            "layout_left1_right2", "layout_top1_bottom2", "layout_left2_right1",
            "layout_top2_bottom1", "layout_top3_bottom3", "layout_center_float",
        ] {
            hotkeys.insert(name.into(), String::new());
        }
        for i in 1..=9 {
            hotkeys.insert(format!("snapshot_{}", i), String::new());
        }
        Self {
            use_workarea: true,
            frame_compensation: true,
            all_desktops: false,
            minimize_extra_windows: true,
            snapshot_restore_mode: "relative".into(),
            theme: "auto".into(),
            hotkeys,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigData {
    pub settings: Settings,
    pub snapshots: Vec<Snapshot>,
}

impl Default for ConfigData {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            snapshots: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    path: PathBuf,
    pub data: ConfigData,
}

/// 旧配置快捷键迁移（纯函数，便于单测）：
/// 0.5.43 起数字键改为打开面板，旧的布局类默认键（Super+1~9）必须清空，
/// 否则会和 panel_window/panel_layout 抢同一个组合键。
/// 只清「仍是出厂默认值」的项——用户手动改成别的键的布局快捷键保留。
fn migrate_hotkeys(hotkeys: &mut std::collections::HashMap<String, String>) {
    let old_layout_defaults = [
        ("layout_quarter", "Super+1"), ("layout_top_bottom", "Super+2"),
        ("layout_left_right", "Super+3"), ("layout_left1_right2", "Super+4"),
        ("layout_top1_bottom2", "Super+5"), ("layout_center_float", "Super+6"),
        ("layout_left2_right1", "Super+7"), ("layout_top2_bottom1", "Super+8"),
        ("layout_top3_bottom3", "Super+9"),
    ];
    for (key, default_binding) in old_layout_defaults {
        if hotkeys.get(key).map(String::as_str) == Some(default_binding) {
            hotkeys.insert(key.to_string(), String::new());
        }
    }
    // 0.5.48：面板入口旧默认 Super+1/Super+2 与 deepin「切换工作区」冲突，
    // 迁到空闲的 Alt+1/Alt+2。只改仍是旧出厂值的项，用户自定义键保留。
    for (key, old, new) in [
        ("panel_window", "Super+1", "Alt+1"),
        ("panel_layout", "Super+2", "Alt+2"),
    ] {
        if hotkeys.get(key).map(String::as_str) == Some(old) {
            hotkeys.insert(key.to_string(), new.to_string());
        }
    }
    // 补上缺失的默认快捷键（含新增的 panel_window/panel_layout），
    // 不覆盖用户已经自定义的键。
    let defaults = Settings::default().hotkeys;
    for (key, binding) in defaults {
        hotkeys.entry(key).or_insert(binding);
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let mut data = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("配置文件解析失败({})，使用默认值: {}", path.display(), e);
                ConfigData::default()
            }),
            Err(_) => ConfigData::default(),
        };
        migrate_hotkeys(&mut data.settings.hotkeys);
        Self { path, data }
    }

    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.path.exists() {
            let backup = self.path.with_extension("json.bak");
            let _ = std::fs::copy(&self.path, &backup);
        }
        let json = serde_json::to_string_pretty(&self.data)
            .map_err(|e| std::io::Error::other(e))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }

    pub fn is_dark(&self) -> bool {
        match self.data.settings.theme.as_str() {
            "light" => false,
            "dark" => true,
            _ => detect_dark_theme(),
        }
    }
}

pub fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Path::new(&dir).join("lithezones/config.json");
    }
    dirs_home().join(".config/lithezones/config.json")
}

pub fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/home/LEARCAT"))
}

/// 通过 DDE Appearance D-Bus 接口检测深浅色。
fn detect_dark_theme() -> bool {
    // 轻量实现：读 GTK 设置；DDE 通常同步这里。
    for p in [
        dirs_home().join(".config/gtk-3.0/settings.ini"),
        dirs_home().join(".config/gtk-4.0/settings.ini"),
    ] {
        if let Ok(s) = std::fs::read_to_string(&p) {
            for line in s.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("gtk-application-prefer-dark-theme") {
                    if v.contains('1') || v.to_lowercase().contains("true") {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_old_layout_hotkeys_to_panel() {
        // 模拟旧配置：9 个布局快捷键占着 Super+1~9，另有一个用户自定义快照键。
        let mut hm: std::collections::HashMap<String, String> = [
            ("layout_quarter", "Super+1"),
            ("layout_top_bottom", "Super+2"),
            ("layout_left_right", "Super+3"),
            ("layout_left1_right2", "Super+4"),
            ("layout_top1_bottom2", "Super+5"),
            ("layout_center_float", "Super+6"),
            ("layout_left2_right1", "Super+7"),
            ("layout_top2_bottom1", "Super+8"),
            ("layout_top3_bottom3", "Super+9"),
            ("snapshot_1", "Super+Alt+1"),
        ].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        migrate_hotkeys(&mut hm);
        // 旧的布局默认键必须清空，腾出 Super+1/2 给面板。
        assert_eq!(hm.get("layout_quarter").unwrap(), "");
        assert_eq!(hm.get("layout_top_bottom").unwrap(), "");
        // 面板快捷键补上默认值（0.5.48 起避开系统工作区键）。
        assert_eq!(hm.get("panel_window").unwrap(), "Alt+1");
        assert_eq!(hm.get("panel_layout").unwrap(), "Alt+2");
        // 用户自定义键不受影响。
        assert_eq!(hm.get("snapshot_1").unwrap(), "Super+Alt+1");
    }

    #[test]
    fn migrate_keeps_customized_layout_hotkey() {
        // 用户把 quarter 改成了 Ctrl+1：不是出厂默认值，应保留。
        let mut hm: std::collections::HashMap<String, String> = [
            ("layout_quarter", "Ctrl+1"),
        ].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        migrate_hotkeys(&mut hm);
        assert_eq!(hm.get("layout_quarter").unwrap(), "Ctrl+1");
        assert_eq!(hm.get("panel_window").unwrap(), "Alt+1");
    }

    #[test]
    fn migrate_old_panel_bindings_avoid_workspace_keys() {
        // 从 0.5.43~0.5.47 升级：仍是旧出厂值的面板键要迁到 Alt+1/2，
        // 用户自定义的面板键不能动。
        let mut hm: std::collections::HashMap<String, String> = [
            ("panel_window", "Super+1"),
            ("panel_layout", "Ctrl+Alt+L"),
        ].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        migrate_hotkeys(&mut hm);
        assert_eq!(hm.get("panel_window").unwrap(), "Alt+1");
        assert_eq!(hm.get("panel_layout").unwrap(), "Ctrl+Alt+L");
    }
}
