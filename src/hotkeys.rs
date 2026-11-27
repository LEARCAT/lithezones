//! 全局快捷键：用 XGrabKey 绑定 Super+1~9 等组合，事件在独立线程轮询。

use crate::config::Settings;
use crate::model::LayoutKind;
use anyhow::Result;
// 必须用 tokio 的通道：std::sync::mpsc::Receiver::recv() 是阻塞调用，
// 放在 tokio::select! 里会把整个异步主循环卡死，菜单点击再也得不到响应。
use tokio::sync::mpsc;
use std::thread;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt, GrabMode, KeyPressEvent, ModMask,
};

/// 解析 "Super+1" / "Ctrl+Alt+1" 为 (修饰键掩码, keysym 名称)。
fn parse_binding(binding: &str) -> Option<(u16, String)> {
    let parts: Vec<&str> = binding.split('+').map(|s| s.trim()).collect();
    if parts.len() < 2 {
        return None;
    }
    let mut mods = 0u16;
    let key = parts.last()?.to_string();
    for part in &parts[..parts.len() - 1] {
        match part.to_lowercase().as_str() {
            "super" | "meta" | "win" => mods |= 0x40, // Mod4
            "ctrl" | "control" => mods |= 0x04,       // Control
            "alt" => mods |= 0x08,                    // Mod1
            "shift" => mods |= 0x01,                  // Shift
            _ => {}
        }
    }
    Some((mods, key))
}

/// keysym 名称转 keycode。
fn keysym_to_keycode(conn: &x11rb::rust_connection::RustConnection, name: &str) -> Option<u8> {
    let keysym = if name.len() == 1 {
        let c = name.as_bytes()[0];
        if c.is_ascii_digit() {
            c as u32
        } else if c.is_ascii_alphabetic() {
            c.to_ascii_lowercase() as u32
        } else {
            return None;
        }
    } else {
        match name.to_lowercase().as_str() {
            "space" => 0x20,
            "return" | "enter" => 0xff0d,
            "tab" => 0xff09,
            "escape" | "esc" => 0xff1b,
            _ => {
                if let Some(num) = name.strip_prefix('f').or_else(|| name.strip_prefix('F')) {
                    if let Ok(n) = num.parse::<u32>() {
                        if (1..=12).contains(&n) {
                            return Some((0xffbe + n - 1) as u8);
                        }
                    }
                }
                return None;
            }
        }
    };

    let setup = conn.setup();
    let min_kc = setup.min_keycode;
    let max_kc = setup.max_keycode;
    let mapping = conn
        .get_keyboard_mapping(min_kc, max_kc - min_kc + 1)
        .ok()?
        .reply()
        .ok()?;
    let per_key = mapping.keysyms_per_keycode as usize;
    for (i, keycode) in (min_kc..=max_kc).enumerate() {
        for j in 0..per_key {
            let sym = mapping.keysyms.get(i * per_key + j).copied().unwrap_or(0);
            if sym == keysym {
                return Some(keycode);
            }
        }
    }
    None
}

/// 快捷键动作，通过 channel 发给主循环处理。
#[derive(Debug, Clone)]
pub enum HotkeyAction {
    Layout(LayoutKind),
    Snapshot(usize), // 1-based
    OpenWindowManager,  // 打开面板「窗口排列」页
    OpenLayoutManager,  // 打开面板「布局管理」页
}

/// 动作名 → 动作。抽出来供启动和运行时重载共用。
fn action_for(name: &str) -> Option<HotkeyAction> {
    match name {
        "panel_window" => Some(HotkeyAction::OpenWindowManager),
        "panel_layout" => Some(HotkeyAction::OpenLayoutManager),
        _ => {
            if let Some(idx) = name.strip_prefix("snapshot_") {
                let n: usize = idx.parse().ok()?;
                Some(HotkeyAction::Snapshot(n))
            } else {
                let kind = match name {
                    "layout_quarter" => LayoutKind::Quarter,
                    "layout_top_bottom" => LayoutKind::TopBottom,
                    "layout_left_right" => LayoutKind::LeftRight,
                    "layout_left1_right2" => LayoutKind::Left1Right2,
                    "layout_top1_bottom2" => LayoutKind::Top1Bottom2,
                    "layout_left2_right1" => LayoutKind::Left2Right1,
                    "layout_top2_bottom1" => LayoutKind::Top2Bottom1,
                    "layout_top3_bottom3" => LayoutKind::Top3Bottom3,
                    "layout_center_float" => LayoutKind::CenterFloat,
                    _ => return None,
                };
                Some(HotkeyAction::Layout(kind))
            }
        }
    }
}

/// X11 的修饰键掩码：Shift=1 Lock=2 Control=4 Mod1(Alt)=8 Mod2=16
/// Mod3=32 Mod4(Super)=64 Mod5=128。匹配时忽略 CapsLock/NumLock 噪声位，
/// 但必须保留 Super(0x40)——旧代码 `state & 0x0f` 会抹掉它，导致 Super 组合
/// 虽然 grab 成功、事件却永远匹配不上。
fn effective_mods(state: u16) -> u16 {
    state & 0xff & !(0x02 | 0x10)
}

/// 面板改键时用：校验组合键串是否可解析、是否带修饰键、键名是否受支持。
/// 只放行「至少一个修饰键 + 一个可用键」，避免用户设成裸数字把系统输入全抢了。
pub fn validate_binding(binding: &str) -> bool {
    let (mods, key_name) = match parse_binding(binding) {
        Some(v) => v,
        None => return false,
    };
    if mods == 0 {
        return false;
    }
    let key = key_name.to_lowercase();
    if key.len() == 1 {
        return key.as_bytes()[0].is_ascii_alphanumeric();
    }
    matches!(key.as_str(), "space" | "return" | "enter" | "tab" | "escape" | "esc")
        || (key.strip_prefix('f').is_some()
            && key[1..].parse::<u32>().map(|n| (1..=12).contains(&n)).unwrap_or(false))
}

/// 快捷键控制柄：action 接收端 + 运行时重载发送端。
/// 面板里改完快捷键后调 reload()，线程会 ungrab 旧键、grab 新键，无需重启。
pub struct HotkeyHandle {
    pub actions: mpsc::UnboundedReceiver<HotkeyAction>,
    reload: std::sync::mpsc::Sender<std::collections::HashMap<String, String>>,
}

impl HotkeyHandle {
    /// 取出重载发送端（可克隆），这样接收端能独立 move 进主循环 select!，
    /// 不会因部分移动导致 handle 本身没法再用。
    pub fn reloader(&self) -> HotkeyReloader {
        HotkeyReloader { tx: self.reload.clone() }
    }
}

#[derive(Clone)]
pub struct HotkeyReloader {
    tx: std::sync::mpsc::Sender<std::collections::HashMap<String, String>>,
}

impl HotkeyReloader {
    pub fn reload(&self, hotkeys: std::collections::HashMap<String, String>) {
        let _ = self.tx.send(hotkeys);
    }
}

/// 启动快捷键监听线程。
pub fn start(settings: &Settings) -> Result<HotkeyHandle> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (reload_tx, reload_rx) = std::sync::mpsc::channel();
    let initial = settings.hotkeys.clone();

    thread::spawn(move || {
        let conn = match x11rb::connect(None) {
            Ok((c, _)) => c,
            Err(e) => {
                eprintln!("快捷键线程无法连接 X11: {}", e);
                return;
            }
        };
        let root = conn.setup().roots[0].root;
        // (mods, keycode) 记录已 grab 的键，重载时用它们逐个 ungrab。
        let mut grabbed: Vec<(u16, u8)> = Vec::new();
        let mut bindings: Vec<(u16, u8, HotkeyAction)> = Vec::new();

        let rebuild = |conn: &x11rb::rust_connection::RustConnection,
                       grabbed: &mut Vec<(u16, u8)>,
                       bindings: &mut Vec<(u16, u8, HotkeyAction)>,
                       hotkeys: &std::collections::HashMap<String, String>| {
            for (mods, keycode) in grabbed.drain(..) {
                let _ = conn.ungrab_key(keycode, root, ModMask::from(mods as u8));
            }
            bindings.clear();
            for (name, binding) in hotkeys {
                if binding.is_empty() {
                    continue;
                }
                let action = match action_for(name) {
                    Some(a) => a,
                    None => continue,
                };
                let (mods, key_name) = match parse_binding(binding) {
                    Some(v) => v,
                    None => continue,
                };
                let keycode = match keysym_to_keycode(conn, &key_name) {
                    Some(c) => c,
                    None => continue,
                };
                let _ = conn.grab_key(
                    false, root, ModMask::from(mods as u8), keycode,
                    GrabMode::ASYNC, GrabMode::ASYNC,
                );
                grabbed.push((mods, keycode));
                bindings.push((mods, keycode, action));
            }
            let _ = conn.flush();
        };

        rebuild(&conn, &mut grabbed, &mut bindings, &initial);

        loop {
            // 面板改键后立即生效：非阻塞取最新配置，重建 grab。
            if let Ok(hotkeys) = reload_rx.try_recv() {
                rebuild(&conn, &mut grabbed, &mut bindings, &hotkeys);
            }
            // 轮询事件，带上限防卡死
            let mut count = 0;
            while count < 64 {
                let event = match conn.poll_for_event() {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(_) => return,
                };
                count += 1;
                if let x11rb::protocol::Event::KeyPress(KeyPressEvent { state, detail, .. }) = event {
                    let mods = effective_mods(u16::from(state));
                    for (bm, keycode, action) in &bindings {
                        if *bm == mods && *keycode == detail {
                            let _ = tx.send(action.clone());
                            break;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(30));
        }
    });

    Ok(HotkeyHandle { actions: rx, reload: reload_tx })
}
