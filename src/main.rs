//! LitheZones（轻格分窗）：Deepin X11 轻量窗口布局工具（Rust 版）
//! 托盘常驻，无主窗口，底层用 x11rb 直接操作 X11。

mod app;
mod config;
mod hotkeys;
mod layout;
mod menu;
mod model;
mod snapshot;
mod tray;
mod x11;

use anyhow::Result;
use config::Config;
use layout::apply;
use menu::{ids, MenuState};
use model::{LayoutKind, Rect};
use app::{AppControl, APP_BUS_NAME, APP_IFACE, APP_PATH};
use enumflags2::BitFlags;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zbus::connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::Value;

/// 面板实时窗口协议分隔符：US 分字段、RS 分记录。UI 文案可能含 `|`，
/// 而 stdin/stdout 协议本身用 TAB 分行，所以这里不能复用这两种字符。
const US: char = '\u{1f}';
const RS: char = '\u{1e}';

/// 面板会话共享状态。面板的 stdin/stdout 循环运行在独立阻塞线程里，
/// 主事件循环不再被"面板开着"这件事卡住——否则托盘点击、快捷键、退出信号
/// 都会排队等到面板关闭，表现为"点图标打不开/很慢"。
/// 面板线程回传给主循环的消息。保存/重命名/删除后必须立刻把配置交回主循环，
/// 否则托盘菜单和快捷键在面板关闭前仍用旧快照列表；但只有最终 Closed 才允许
/// 把「面板是否打开」标志复位，中途配置更新不能误判为面板已关。
enum PanelUpdate {
    Config(Config),
    /// 面板里改了快捷键：配置已由面板线程写盘，这里只要求主循环重载 grab。
    ReloadHotkeys(std::collections::HashMap<String, String>),
    Closed,
}

struct PanelControl {
    open: Arc<AtomicBool>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    cfg_back: tokio::sync::mpsc::UnboundedSender<PanelUpdate>,
    x: Arc<x11::X11>,
    /// 面板最近一次被最小化（HIDDEN）的时刻。用于区分：
    ///   · 刚点 dock 图标最小化（几秒内）→ 收到 Activate 时不再激活，保持最小化；
    ///   · 已最小化很久后点托盘/启动器/菜单 → 正常激活恢复显示。
    minimized_at: Arc<Mutex<Option<Instant>>>,
    /// 面板自己因「应用/加载布局」主动最小化。置前线程据此跳过，
    /// 避免刚 iconify 又被拉起来；下次托盘/启动器打开时复位。
    self_minimized: Arc<AtomicBool>,
    /// 打开面板前用户正在操作的窗口。加载快照时恢复它会把焦点还给用户，
    /// 避免最后一个被取消最小化的窗口莫名其妙抢到前台。
    last_focus: Arc<Mutex<Option<String>>>,
}

/// 让当前进程自杀并立即拉起一个全新实例打开窗口排列。
///
/// 注销/重登后 X server 会重启，XID 全部重新分配，但 D-Bus session bus
/// （systemd --user 持有，/run/user/<uid>/bus）通常不重启。结果是旧托盘进程
/// 还占着 com.lithezones.app，Ping/Identity 都正常，可它的 X11 连接指向已死
/// 的 display——这就是"托盘图标还在、点它打不开窗口"的根因（活壳）。
///
/// 活壳自己开不出窗，但它的 Quit() 方法不依赖 X11，可以正常退出；随后释放
/// 名称、重启一个连着新 display 的实例，窗口就能开出来。外部启动器
/// （--window-manager）已有同样的兜底，这里给托盘点击补上等价逻辑。
async fn self_heal_respawn(conn: &zbus::Connection, reason: &str) -> bool {
    crate::vlog!("活壳自愈：{}，退出并拉起新实例", reason);
    // Quit() 只往 tokio channel 写一条消息，不碰 X11，活壳也能响应。
    let _ = conn
        .call_method(Some(APP_BUS_NAME), APP_PATH, Some(APP_IFACE), "Quit", &())
        .await;
    if !wait_name_released(conn, NAME_RELEASE_TIMEOUT).await {
        crate::vlog!("自愈：旧名称未按时释放，继续尝试抢占");
    }
    // 主动释放（若本进程就是持有者），再拉起新实例。
    let _ = conn.release_name(APP_BUS_NAME).await;
    let exe = std::env::current_exe().ok();
    if let Some(exe) = exe {
        let _ = std::process::Command::new(exe)
            .arg("--window-manager")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    true // 视同 quit：主循环 break，本活壳进程退出。
}

/// 菜单点击处理
async fn handle_menu_click(
    id: i32,
    cfg: &mut Config,
    x: &x11::X11,
    menu_state: &MenuState,
    panel_ctl: &PanelControl,
    conn: &zbus::Connection,
) -> anyhow::Result<bool> {
    let mut quit = false;
    crate::vlog!("菜单点击 id={}", id);

    match id {
        // 窗口排列 / 布局管理现在共用一个面板窗口，左上角标签切换。
        // 菜单入口只决定初始标签，不再各起一个独立进程。
        ids::WINDOW_MANAGER | ids::LAYOUT_MANAGER => {
            let settings = cfg.data.settings.clone();
            // 活壳检测（注销/重登后 X server 重启，本进程 X11 连接已失效）：
            // 这里任何 X11 查询失败都说明当前 display 已不可用。旧逻辑用 `?`
            // 直接返回 Err、主循环只打日志，用户看到的就是"点托盘没反应"。
            // 改为触发自愈：退出活壳、抢占重建、在新 display 上开窗。
            let areas = match x.workareas() {
                Ok(a) => a,
                Err(e) => return Ok(self_heal_respawn(conn, &format!("X11 工作区查询失败: {}", e)).await),
            };
            let focus_id = match x.active_window_info() {
                Ok(v) => v.map(|w| w.id.clone()),
                Err(e) => return Ok(self_heal_respawn(conn, &format!("X11 焦点查询失败: {}", e)).await),
            };
            // 记住打开面板前用户正在用的窗口；加载快照后把焦点还给它。
            *panel_ctl.last_focus.lock().unwrap() = focus_id.clone();
            // 候选池包含所有窗口（含用户最小化的），勾选中会在 apply 时恢复摆位。
            // 面板默认勾选前 n0 个，所以排序决定"默认排哪些"：
            //   可见窗口优先，隐藏（最小化）窗口靠后——用户手动勾才 restore 摆位。
            //   固定尺寸窗口不再特殊对待：它们现在会按 min/max 在格子里居中或浮窗，
            //   不再是"勾了也没用"，所以按普通可见窗口排序即可。
            let mut pool: Vec<_> = x.managed_windows_with_hidden(&settings)?;
            pool.sort_by_key(|w| w.hidden);
            if let Some(fid) = focus_id.as_deref() {
                if let Some(i) = pool.iter().position(|w| w.id == fid) {
                    let w = pool.remove(i);
                    pool.insert(0, w);
                }
            }
            let initial = LayoutKind::Quarter;
            let tab = if id == ids::WINDOW_MANAGER { "window" } else { "layout" };
            // 面板"是否开着"以真实窗口为准，不能只信 open 标志。
            // 历史 bug：open=true 但面板窗口早已关闭/崩溃（panel() 卡在 stdout
            // 循环、cfg 回传丢失）时，open 永远停在 true，此后每次点击都只往
            // 失效的 stdin 写 tab、直接 return，面板再也打不开——用户点一整晚
            // 都没反应。这里先按窗口实际存在性判断：窗口没了就强制复位标志、
            // 走下面重新开面板，而不是被卡死的标志骗过去。
            let flagged_open = panel_ctl.open.load(Ordering::Acquire);
            let visible = window_manager_visible();
            if flagged_open && visible {
                if let Some(s) = panel_ctl.stdin.lock().unwrap().as_mut() {
                    if writeln!(s, "tab\t{}", tab).is_ok() && s.flush().is_ok() {
                        // 这次是用户明确要重新打开。先取出旧值：若为 true，
                        // 说明面板是「应用/加载布局」后被程序自己收起的，
                        // 这次点击必须立即恢复，不能被 1.5 秒抑制挡住。
                        let was_self_min =
                            panel_ctl.self_minimized.swap(false, Ordering::AcqRel);
                        if was_self_min {
                            *panel_ctl.minimized_at.lock().unwrap() = None;
                        }
                        // 按标题激活会命中失效旧窗口，改用按 ID 精确激活。
                        let _ = raise_panel_window(&panel_ctl.minimized_at, was_self_min);
                        return Ok(false);
                    }
                    // stdin 写失败也说明面板已死，落下面的重建逻辑。
                }
                panel_ctl.stdin.lock().unwrap().take();
            }
            if flagged_open && !visible {
                crate::vlog!("面板标志为开但窗口已不存在，强制复位并重开");
                panel_ctl.open.store(false, Ordering::Release);
                panel_ctl.stdin.lock().unwrap().take();
            }
            crate::vlog!("打开统一面板 tab={}, 候选窗口 {} 个", tab, pool.len());
            panel_ctl.self_minimized.store(false, Ordering::Release);
            panel_ctl.open.store(true, Ordering::Release);
            let mut owned_cfg = (*cfg).clone();
            let owned_x = panel_ctl.x.clone();
            let owned_pool = pool;
            let owned_areas = areas;
            let owned_focus = focus_id;
            let owned_tab = tab.to_string();
            let stdin_slot = panel_ctl.stdin.clone();
            let cfg_tx = panel_ctl.cfg_back.clone();
            let dark = owned_cfg.is_dark();
            // 面板窗口由 Tk 异步创建，deepin/kwin 下 withdraw→deiconify 竞态
            // 偶尔把刚创建的面板钉成 _NET_WM_STATE_HIDDEN，表现为"点开看不到"。
            //
            // 只在窗口「第一次出现」时置前一次（最多追 6 秒等它出现）。
            // 一旦这一轮置前做完，线程立刻结束，绝不再看 HIDDEN——否则用户
            // 在这 6 秒内点最小化会被当成竞态重新激活，面板「最小化又弹出」。
            // 这是最小化自动弹出的真正根因：旧版守护线程 6 秒内每 150ms 无条件
            // activate，实测 xdotool windowminimize 后 100~200ms HIDDEN、300ms 就
            // 被重新拉回 FOCUSED。改成一次性置前后，最小化由 Tk 自己负责。
            let raiser_alive = panel_ctl.open.clone();
            let raiser_x = panel_ctl.x.clone();
            let raiser_self_min = panel_ctl.self_minimized.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(6);
                while raiser_alive.load(Ordering::Acquire) && Instant::now() < deadline {
                    if let Some(id) = find_panel_window() {
                        // 等 Tk 把 withdraw→deiconify 切换做完再置前，避免抢在竞态中间。
                        std::thread::sleep(Duration::from_millis(350));
                        // 用户刚点「应用/加载」主动最小化时不能再置前。
                        if raiser_alive.load(Ordering::Acquire)
                            && !raiser_self_min.load(Ordering::Acquire)
                        {
                            // 顺带把顶层窗口的 WM_CLASS 改成 lithezones，让 dock
                            // 把面板和启动器图标关联起来（Tk 默认是 tk/Tk）。
                            let _ = raiser_x.set_wm_class(&id, "lithezones", "LitheZones");
                            activate_panel_by_id(&id);
                        }
                        break; // 只置前一次，之后完全放手，尊重用户最小化。
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            });
            let owned_self_min = panel_ctl.self_minimized.clone();
            let owned_restore_focus = panel_ctl.last_focus.lock().unwrap().clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = panel(
                    &mut owned_cfg,
                    &owned_x,
                    &owned_pool,
                    &owned_areas,
                    owned_focus.as_deref(),
                    dark,
                    initial,
                    &owned_tab,
                    &stdin_slot,
                    &cfg_tx,
                    &owned_self_min,
                    owned_restore_focus.as_deref(),
                ) {
                    eprintln!("面板通信失败: {}", e);
                }
                *stdin_slot.lock().unwrap() = None;
                // 保存/重命名/删除已在处理命令时增量回传；这里只通知主循环
                // 「面板真的关闭了」，避免中途 Config 更新误清 open 标志。
                let _ = cfg_tx.send(PanelUpdate::Closed);
            });
        }
        ids::RELOAD_CONFIG => {
            *cfg = Config::load();
            refresh_menu(cfg, menu_state)?;
        }
        ids::AUTOSTART => {
            toggle_autostart(cfg)?;
            refresh_menu(cfg, menu_state)?;
            notify(if autostart_enabled() {
                "已开启开机启动"
            } else {
                "已关闭开机启动"
            });
        }
        ids::QUIT => {
            quit = true;
        }
        _ => {}
    }
    Ok(quit)
}

/// 定位随包分发的 tkinter 脚本：可执行文件旁的 ../share/lithezones/，
/// 以及开发时的源码目录 share/。
fn resource_script(name: &str) -> Option<std::path::PathBuf> {
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("../share/lithezones").join(name));
            cands.push(dir.join(name));
        }
    }
    cands.push(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("share").join(name));
    cands.into_iter().find(|p| p.exists())
}

/// 启动器发来 Event 后，给面板足够长的出现时间。
/// 实测字体缓存失效时，panel.py 的 gsettings/fc-match 冷查询要 2.1 秒，
/// 原来只等 2 秒会误判"窗口没出现"，把本来正在打开的旧实例杀掉。
const WM_SHOW_TIMEOUT: Duration = Duration::from_secs(6);
/// 通知旧实例 Quit 后，等 D-Bus 名称真正释放，避免抢占名称时的竞态。
const NAME_RELEASE_TIMEOUT: Duration = Duration::from_secs(3);

/// 关闭所有残留的「轻格分窗 - 管理面板」窗口。
///
/// Quit() 只结束 Rust 主循环；tkinter 面板是被 spawn 的独立进程，父进程退出后
/// 它常变成孤儿窗口继续留在桌面。旧实例被抢占后，这个旧面板（旧配色/旧逻辑）
/// 仍停在当前版本之上，用户装了新版却一直看到旧界面；同时 `window_manager_visible()`
/// 也会被这个孤儿窗口骗到，以为面板已开。升级/退出时必须主动清掉。
fn close_stale_panel_windows() {
    let out = match std::process::Command::new("wmctrl").arg("-l").output() {
        Ok(o) => o,
        Err(_) => return,
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.contains("轻格分窗 - 管理面板") {
            continue;
        }
        if let Some(id) = line.split_whitespace().next() {
            let _ = std::process::Command::new("wmctrl")
                .args(["-i", "-c", id])
                .output();
        }
    }
}

/// 把管理面板窗口激活置前（deepin/dde-kwin 加固）。
///
/// 面板用 Tk 的 `-topmost` + `lift()` 置顶，但实测 dde-kwin 下不生效：
/// 面板明明打开了（IsViewable、带 ABOVE 状态），却被铺满屏幕的布局窗口
/// 整个盖住，用户看不到，就以为"窗口排列打不开"。
///
/// 从 Rust 侧发 `_NET_ACTIVE_WINDOW` 才可靠。两个坑：
///   ① 不能按标题激活（wmctrl -a）——失效的旧面板窗口会留在 wmctrl 列表里，
///      按标题会匹配到旧窗口，激活无效；X11 的 XID 递增，最大的即最新的。
///   ② 不能用 Tk 的 winfo_id()——它返回的是内部子窗口 ID（实测 0x4600008），
///      与 wmctrl 管理的顶层窗口 ID（0x04600009）不是同一个。
/// 找到最新的面板窗口 ID（XID 递增，最大的即最新）。
fn find_panel_window() -> Option<String> {
    let out = std::process::Command::new("wmctrl")
        .arg("-l")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut best: Option<(u32, String)> = None;
    for line in text.lines() {
        if !line.contains("轻格分窗 - 管理面板") {
            continue;
        }
        let id = line.split_whitespace().next().unwrap_or("");
        if let Ok(v) = u32::from_str_radix(id.trim_start_matches("0x"), 16) {
            if best.as_ref().map_or(true, |(b, _)| v > *b) {
                best = Some((v, id.to_string()));
            }
        }
    }
    best.map(|(_, id)| id)
}

/// 激活面板窗口并等 kwin 处理完。只用 wmctrl -i -a：实测 deepin/kwin 下
/// 紧跟一个手写 _NET_ACTIVE_WINDOW client message 会与 wmctrl 正常激活竞争，
/// 反而把面板钉在 HIDDEN；wmctrl -i -a 单独用反复验证都能恢复成 FOCUSED。
fn activate_panel_by_id(id: &str) {
    let _ = std::process::Command::new("wmctrl")
        .args(["-i", "-a", id])
        .output();
    std::thread::sleep(Duration::from_millis(100));
}

/// 面板窗口当前是否持有输入焦点（在最前台）。
/// 点 dock 图标时若面板已经聚焦，用户的意图是「最小化」，此时绝不能再激活。
fn panel_window_focused(id: &str) -> bool {
    let out = match std::process::Command::new("xprop")
        .arg("-root")
        .arg("_NET_ACTIVE_WINDOW")
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // 形如：_NET_ACTIVE_WINDOW(WINDOW): window id # 0x0c00000b
    let active = text.rsplit("0x").next().unwrap_or("").trim();
    let want = id.trim_start_matches("0x").to_ascii_lowercase();
    !active.is_empty() && active.to_ascii_lowercase().starts_with(&want)
}

/// 找到并激活面板窗口（供「面板已开、切标签」处调用）。
/// minimized_at：面板最近一次变成 HIDDEN 的时刻（由后台线程维护）。
/// force_reopen：这次 Activate 来自用户明确要重新打开「应用/加载后自动收起」
/// 的面板，必须绕过 1.5 秒抑制，否则窗口弹出后还会被残余的最小化请求按下。
fn raise_panel_window(
    minimized_at: &Arc<Mutex<Option<Instant>>>,
    force_reopen: bool,
) -> Option<String> {
    let id = find_panel_window()?;
    // 尊重用户点 dock 图标的最小化操作：deepin 点击运行中应用图标时，
    // 先最小化窗口再发 SNI Activate。若面板刚被最小化（1.5 秒内），这次
    // Activate 就是"最小化副作用"，不再激活，否则会把最小化立刻抵消，
    // 用户看到的就是「要点 3 次以上才能最小化，否则一直弹出来」。
    // 但应用/加载后由程序自己收起的面板，用户再点图标是明确要恢复，
    // 不能被这条抑制挡住（force_reopen）。
    let just_minimized = minimized_at
        .lock()
        .unwrap()
        .map(|t| t.elapsed() < Duration::from_millis(1500))
        .unwrap_or(false);
    if panel_window_hidden(&id) && just_minimized && !force_reopen {
        return None;
    }
    // 面板已聚焦（在最前台）：用户点 dock 图标是想最小化它，不抢焦点。
    if panel_window_focused(&id) {
        return None;
    }
    // 面板最小化已久 / 被别的窗口盖住：置前恢复。
    activate_panel_by_id(&id);
    // 这次激活已完成，清掉「刚最小化」时刻，避免紧接着的下一次点击仍被抑制。
    *minimized_at.lock().unwrap() = None;
    Some(id)
}

/// 面板窗口是否仍处于 _NET_WM_STATE_HIDDEN（最小化/未显示）。
/// xprop 读不到时保守当作可见，避免死等。
fn panel_window_hidden(id: &str) -> bool {
    let out = match std::process::Command::new("xprop")
        .args(["-id", id, "_NET_WM_STATE"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.contains("_NET_WM_STATE") {
        return false;
    }
    text.contains("_NET_WM_STATE_HIDDEN")
}

/// 判断窗口排列是否真的显示出来。
/// D-Bus Menu.Event 返回成功只说明事件进了连接；旧实例可能收下事件却不弹窗。
fn window_manager_visible() -> bool {
    std::process::Command::new("wmctrl")
        .arg("-l")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.contains("轻格分窗 - 管理面板"))
        })
        .unwrap_or(false)
}

async fn wait_for_window_manager(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if window_manager_visible() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

/// 等 well-known name 从 D-Bus 上消失。Exit 后 dbus-daemon 可能短暂保留连接，
/// 此时立刻 RequestName(ReplaceExisting) 会失败；启动器随后只能把 Event 再发给
/// 正在退出的旧实例，窗口就永远出不来。
async fn wait_name_released(conn: &zbus::Connection, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let held = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(APP_BUS_NAME,),
            )
            .await
            .is_ok();
        if !held {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 弹出统一面板：窗口排列 / 布局管理共用一个窗口。
/// tab 只决定初始标签；apply 命令在此直接调用 layout::apply，其余命令交给
/// handle_layout_op。通信为逐行命令 + 应答，与旧 layout_manager 一致。
fn panel(
    cfg: &mut Config,
    x: &x11::X11,
    pool: &[model::WindowInfo],
    areas: &[Rect],
    focus_id: Option<&str>,
    dark: bool,
    initial: LayoutKind,
    tab: &str,
    stdin_slot: &Arc<Mutex<Option<ChildStdin>>>,
    cfg_tx: &tokio::sync::mpsc::UnboundedSender<PanelUpdate>,
    self_minimized: &Arc<AtomicBool>,
    restore_focus: Option<&str>,
) -> Result<()> {
    let script = resource_script("panel.py")
        .ok_or_else(|| anyhow::anyhow!("找不到 panel.py"))?;
    // 固定用系统 python3：PATH 里可能是别的版本（例如 3.13），而包内 vendor
    // 的 Pillow C 扩展是 cp312；版本错配会让面板启动即崩、表现为“点不开”。
    // 系统 /usr/bin/python3 稳定为 3.12，和 vendor 的 .so ABI 一致。
    let py = if std::path::Path::new("/usr/bin/python3").exists() {
        "/usr/bin/python3"
    } else {
        "python3"
    };
    // deepin V23 默认可能不带 python3-tk；control 不再把它列为硬依赖，
    // 否则 dpkg 直接在依赖阶段失败、postinst 的友好提示根本没机会显示。
    // 这里在打开面板前再查一次，缺依赖时用桌面通知 + stderr 明确告知用户。
    let probe = std::process::Command::new(py)
        .arg("-c")
        .arg("import sys, tkinter; sys.exit(0 if sys.version_info[:2] == (3, 12) else 2)")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match probe {
        Ok(s) if s.success() => {}
        Ok(s) if s.code() == Some(2) => {
            let msg = "管理面板需要 python3 3.12（当前系统 python3 版本不匹配），请安装 python3.12 后重试。";
            notify(msg);
            anyhow::bail!(msg);
        }
        _ => {
            let msg = "缺少 python3-tk，管理面板无法打开。请在终端执行：sudo apt install python3-tk";
            notify(msg);
            anyhow::bail!(msg);
        }
    }
    let mut cmd = std::process::Command::new(py);
    cmd.arg(&script)
        .arg(format!("--tab={}", tab))
        .arg(format!("--layout={}", initial.slug()))
        .arg(format!("--theme={}", if dark { "dark" } else { "light" }))
        .arg(format!("--version={}", env!("CARGO_PKG_VERSION")))
        // 计数与预览矩形都用完整候选池（含被收起的窗口），和下面传给 Python 的
        // 窗口卡片一致。此前用只含可见窗口的 windows，导致面板提示「2 个可管理
        // 窗口」而实际卡片更多，保存/加载看着像丢窗口。
        .arg(format!("--count={}", pool.len()))
        .arg(format!("--current-rects={}", current_rects(pool, areas)));
    for s in &cfg.data.snapshots {
        cmd.arg(format!(
            "--item={}\u{1f}{}\u{1f}{}\u{1f}{}",
            clean_msg(&s.name),
            clean_msg(&s.created_at),
            s.windows.len(),
            snapshot_rects(s)
        ));
    }
    for w in pool {
        let title = if w.title.is_empty() { w.class.clone() } else { w.title.clone() };
        let r = w.frame_rect();
        // 格式：id|x|y|宽|高|显示名|hidden。脚本 split('|', 6)，
        // 显示名里可含 '|'；hidden 供列表模式显示「已最小化」。
        cmd.arg(format!(
            "{}|{}|{}|{}|{}|{} — {}|{}",
            w.id, r.x, r.y, r.width, r.height, w.class, title,
            if w.hidden { 1 } else { 0 }
        ));
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = cmd.spawn()?;
    {
        let mut slot = stdin_slot.lock().unwrap();
        *slot = child.stdin.take();
        if slot.is_none() {
            anyhow::bail!("无法连接面板窗口");
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("无法读取面板窗口"))?;
    let settings = cfg.data.settings.clone();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.first().copied().unwrap_or("") == "close" {
            break;
        }
        let cmd = fields.first().copied().unwrap_or("");
        let reply = match cmd {
            "apply" => {
                let kind = fields
                    .get(1)
                    .and_then(|s| kind_from_slug(s))
                    .unwrap_or(initial);
                // 面板开着时会自动刷新窗口列表，用户可能勾选了面板打开后才出现
                // 的窗口。必须按点击应用这一刻的实时窗口池解析 ID，不能再用
                // pool（打开面板时的旧快照），否则新窗口会被当成"未勾选"。
                let live_settings = cfg.data.settings.clone();
                let live = (|| -> anyhow::Result<(Vec<model::WindowInfo>, Vec<Rect>)> {
                    Ok((live_panel_pool(x, &live_settings, focus_id)?, x.workareas()?))
                })();
                match live {
                    Ok((live_pool, live_areas)) => {
                        let mut chosen = Vec::new();
                        for id in fields.iter().skip(2) {
                            if let Some(w) = live_pool.iter().find(|w| w.id == *id) {
                                chosen.push(w.clone());
                            }
                        }
                        if chosen.is_empty() {
                            format!("err\t{}", clean_msg("未勾选任何窗口"))
                        } else {
                            // 面板里用户按卡片/列表序号勾选；传 None 让窗口严格
                            // 按该顺序落槽，不再把打开面板前的焦点窗口抢到第一个。
                            match apply(x, kind, &chosen, None, &live_areas, &live_settings) {
                                Ok(_) => format!("ok\t{}", clean_msg("布局已应用")),
                                Err(e) => {
                                    format!("err\t{}", clean_msg(&format!("布局失败：{}", e)))
                                }
                            }
                        }
                    }
                    Err(e) => format!(
                        "err\t{}",
                        clean_msg(&format!("刷新当前窗口状态失败：{}", e))
                    ),
                }
            }
            // 面板打开期间由 Python 每 1.5s 查询一次，实时更新候选窗口与
            // 缩略图。返回一行 US/RS 编码，沿用与首次打开相同的候选池排序。
            "windows" => {
                let live_settings = cfg.data.settings.clone();
                match live_panel_pool(x, &live_settings, focus_id) {
                    Ok(pool) => format!("ok\t{}", encode_panel_entries(&pool)),
                    Err(e) => format!(
                        "err\t{}",
                        clean_msg(&format!("刷新窗口列表失败：{}", e))
                    ),
                }
            }
            // 面板「快捷键」页读取可修改的入口键：name=组合键，US 分隔。
            // 只暴露面板入口两项，布局/快照键仍可由配置文件手动设置。
            "hotkeys" => {
                let hk = &cfg.data.settings.hotkeys;
                let entries: Vec<String> = ["panel_window", "panel_layout"]
                    .iter()
                    .map(|name| format!("{}={}", name, hk.get(*name).cloned().unwrap_or_default()))
                    .collect();
                format!("ok\t{}", entries.join(&US.to_string()))
            }
            // 面板里改快捷键：校验 → 写配置 → 原子落盘 → 通知主循环热重载 grab。
            // 裸键和不可解析组合直接拒绝，避免用户把普通按键全抢掉。
            "set_hotkey" => {
                let name = fields.get(1).copied().unwrap_or("");
                let binding = fields.get(2).copied().unwrap_or("");
                if name != "panel_window" && name != "panel_layout" {
                    format!("err\t{}", clean_msg("不支持修改这个快捷键"))
                } else if !hotkeys::validate_binding(binding) {
                    format!("err\t{}", clean_msg("快捷键格式不对，请带 Ctrl/Alt/Shift/Super 再按一个键"))
                } else {
                    cfg.data.settings.hotkeys.insert(name.to_string(), binding.to_string());
                    match cfg.save() {
                        Ok(_) => {
                            let _ = cfg_tx.send(PanelUpdate::ReloadHotkeys(
                                cfg.data.settings.hotkeys.clone(),
                            ));
                            format!("ok\t{}", clean_msg("快捷键已保存并立即生效"))
                        }
                        Err(e) => format!(
                            "err\t{}",
                            clean_msg(&format!("保存配置失败：{}", e))
                        ),
                    }
                }
            }
            // 0.5.52 机制改动：应用/加载布局后由 Rust 统一用 ICCCM
            // WM_CHANGE_STATE(IconicState) 最小化面板（等价 xdotool
            // windowminimize），Python 侧不再调用 Tk root.iconify()。
            // 原因：deepin V23 (dde-kwin) 下 Tk iconify 与 WM 状态机不同步——
            // Tk 内部仍认定窗口是 iconic，用户从 dock 恢复后 Tk 又在约 50ms
            // 内把窗口收回 HIDDEN（「点图标弹出又立刻缩回」）。改用 ICCBM
            // 后实测 dock 恢复稳定保持 FOCUSED。
            "minimize" => {
                self_minimized.store(true, Ordering::Release);
                if let Some(id) = find_panel_window() {
                    // 立即发一次 ICCBM：Python 保持 normal，不再有 Tk 的
                    // 双重最小化风险。执行前复查标志，避免过期命令误伤
                    // 已经重新打开的面板。
                    if self_minimized.load(Ordering::Acquire)
                        && !panel_window_hidden(&id)
                    {
                        if let Ok(win) = x.window_by_id(&id) {
                            if let Err(e) = x.minimize(&win) {
                                crate::vlog!("面板最小化失败: {}", e);
                            } else {
                                crate::vlog!("面板已用 ICCBM 最小化: {}", id);
                            }
                        }
                    }
                }
                format!("ok\t{}", clean_msg("面板已最小化"))
            }
            // 保存/加载必须用点按钮那一刻的实时窗口与工作区：
            // pool/areas 是打开面板时的快照，面板开着时布局或窗口状态可能已变化，
            // 继续用旧池会保存旧几何、找不到刚恢复的窗口，表现就是"存不上/恢复不了"。
            "save" | "load" => {
                let live_settings = cfg.data.settings.clone();
                let live = (|| -> anyhow::Result<(Vec<model::WindowInfo>, Vec<Rect>)> {
                    Ok((x.managed_windows_with_hidden(&live_settings)?, x.workareas()?))
                })();
                match live {
                    Ok((live_pool, live_areas)) => {
                        let (reply, changed) = handle_layout_op(
                            cfg, x, &live_pool, &live_areas, &live_settings, &fields,
                            restore_focus,
                        );
                        if changed {
                            let _ = cfg_tx.send(PanelUpdate::Config(cfg.clone()));
                        }
                        reply
                    }
                    Err(e) => format!(
                        "err\t{}",
                        clean_msg(&format!("刷新当前窗口状态失败：{}", e))
                    ),
                }
            }
            _ => {
                let (reply, changed) =
                    handle_layout_op(cfg, x, pool, areas, &settings, &fields, None);
                if changed {
                    let _ = cfg_tx.send(PanelUpdate::Config(cfg.clone()));
                }
                reply
            }
        };
        let mut slot = stdin_slot.lock().unwrap();
        if let Some(s) = slot.as_mut() {
            writeln!(s, "{}", reply)?;
            s.flush()?;
        }
    }
    stdin_slot.lock().unwrap().take();
    child.wait()?;
    Ok(())
}

fn kind_from_slug(slug: &str) -> Option<LayoutKind> {
    LayoutKind::all().into_iter().find(|k| k.slug() == slug)
}

/// 判断 D-Bus 名称持有连接对应的进程是否真的还活着。
///
/// 僵尸连接的典型症状：well-known name 还在、`Identity()`/`Ping()` 仍能应答，
/// 但 `GetConnectionUnixProcessID` 报出的 PID 早已退出（进程异常退出后 D-Bus
/// socket 被短暂继承，dbus-daemon 尚未回收连接）。仅凭 Identity 字符串匹配
/// 会被这种连接骗过——因为它正是本二进制启动后残留的，身份完全一致。
///
/// 这里用 `kill(pid, 0)` 只探测进程存在性（不发真实信号），不读 /proc：
/// 受限环境下读不到跨进程 `/proc/<pid>/exe` 会造成误判。EPERM 也算存活
/// （进程在，只是无权发信号）。
async fn owner_process_alive(conn: &zbus::Connection) -> bool {
    let owner: String = match conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(APP_BUS_NAME,),
        )
        .await
        .and_then(|r| r.body().deserialize::<String>())
    {
        Ok(v) => v,
        Err(_) => return false,
    };
    let pid: u32 = match conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(owner,),
        )
        .await
        .and_then(|r| r.body().deserialize::<u32>())
    {
        Ok(v) => v,
        Err(_) => return false,
    };
    unsafe {
        if libc::kill(pid as libc::pid_t, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
    }
}

/// 协议字段/提示语不能含制表符或换行。
fn clean_msg(s: &str) -> String {
    s.replace('\t', " ").replace('\n', " ").replace('\r', " ")
}

/// 窗口列表协议字段还要避开 US/RS（它们用于记录/字段分隔）。
fn clean_field(s: &str) -> String {
    clean_msg(s).replace(US, "").replace(RS, "")
}

/// 面板打开后重新读取候选池：包含最小化窗口，可见优先，面板打开前
/// 用户正在操作的窗口排最前（与首次打开面板时的排序保持一致）。
fn live_panel_pool(
    x: &x11::X11,
    settings: &config::Settings,
    focus_id: Option<&str>,
) -> Result<Vec<model::WindowInfo>> {
    let mut pool = x.managed_windows_with_hidden(settings)?;
    pool.sort_by_key(|w| w.hidden);
    if let Some(fid) = focus_id {
        if let Some(i) = pool.iter().position(|w| w.id == fid) {
            let w = pool.remove(i);
            pool.insert(0, w);
        }
    }
    Ok(pool)
}

/// 把当前候选池编码成一行应答：`id US x US y US w US h US 显示名 US hidden`，
/// 记录之间用 RS。面板据此刷新卡片/列表，并按窗口 ID 保留用户勾选状态。
fn encode_panel_entries(pool: &[model::WindowInfo]) -> String {
    pool.iter()
        .map(|w| {
            let title = if w.title.is_empty() {
                w.class.clone()
            } else {
                w.title.clone()
            };
            let r = w.frame_rect();
            [
                clean_field(&w.id),
                r.x.to_string(),
                r.y.to_string(),
                r.width.to_string(),
                r.height.to_string(),
                clean_field(&format!("{} — {}", w.class, title)),
                if w.hidden { "1" } else { "0" }.to_string(),
            ]
            .join(&US.to_string())
        })
        .collect::<Vec<_>>()
        .join(&RS.to_string())
}

fn encode_rects<I: IntoIterator<Item = Rect>>(rects: I) -> String {
    rects
        .into_iter()
        .map(|r| format!("{},{},{},{}", r.x, r.y, r.width, r.height))
        .collect::<Vec<_>>()
        .join(";")
}

fn snapshot_rects(snap: &model::Snapshot) -> String {
    encode_rects(snap.windows.iter().map(|w| w.rel))
}

/// 当前窗口的相对几何，供新保存的布局即时显示预览。
fn current_rects(windows: &[model::WindowInfo], areas: &[Rect]) -> String {
    if areas.is_empty() {
        return String::new();
    }
    encode_rects(windows.iter().map(|w| {
        let idx = w.area_index(areas);
        let area = areas.get(idx).copied().unwrap_or(areas[0]);
        let r = w.frame_rect();
        Rect::new(
            ((r.x - area.x) as f64 / area.width as f64 * 10000.0).round() as i32,
            ((r.y - area.y) as f64 / area.height as f64 * 10000.0).round() as i32,
            (r.width as f64 / area.width as f64 * 10000.0).round() as i32,
            (r.height as f64 / area.height as f64 * 10000.0).round() as i32,
        )
    }))
}

/// 返回 (给面板的应答, 配置是否变更)。配置变更时由调用方立刻回传主循环，
/// 不能等面板关闭，否则托盘菜单/快捷键在面板开着期间仍看旧快照列表。
fn handle_layout_op(
    cfg: &mut Config,
    x: &x11::X11,
    pool: &[model::WindowInfo],
    areas: &[Rect],
    settings: &config::Settings,
    fields: &[&str],
    restore_focus: Option<&str>,
) -> (String, bool) {
    let fail = |msg: &str| (format!("err\t{}", clean_msg(msg)), false);
    // 加载只改窗口几何，不改配置。
    let ok = |msg: String| (format!("ok\t{}", clean_msg(&msg)), false);
    // 保存/重命名/删除会改配置。
    let ok_changed = |msg: String| (format!("ok\t{}", clean_msg(&msg)), true);
    let idx = || fields.get(1).and_then(|s| s.trim().parse::<usize>().ok());

    match fields.first().copied().unwrap_or("") {
        "save" => {
            let name = fields.get(1).copied().unwrap_or("").trim().to_string();
            if name.is_empty() {
                return fail("请输入布局名称");
            }
            if cfg.data.snapshots.iter().any(|s| s.name == name) {
                return fail("已有同名布局，请换一个名称");
            }
            // pool 由调用方在点击保存瞬间重新读取，含被收起/最小化的窗口。
            match snapshot::capture(x, name.clone(), pool, areas, settings) {
                Ok(snap) => {
                    cfg.data.snapshots.push(snap);
                    match cfg.save() {
                        Ok(_) => {
                            // 把刚保存的真实窗口数与相对几何一并回给面板，
                            // 避免面板拿打开时的旧 current_rects 画错误预览。
                            let msg = clean_msg(&format!("已保存「{}」", name));
                            (
                                format!(
                                    "ok\t{}\t{}\t{}",
                                    msg,
                                    pool.len(),
                                    current_rects(pool, areas)
                                ),
                                true,
                            )
                        }
                        Err(e) => fail(&format!("保存失败：{}", e)),
                    }
                }
                Err(e) => fail(&format!("保存失败：{}", e)),
            }
        }
        "load" => {
            let i = match idx() {
                Some(i) if i < cfg.data.snapshots.len() => i,
                _ => return fail("布局不存在或已被删除"),
            };
            let snap = cfg.data.snapshots[i].clone();
            match snapshot::restore(x, &snap, pool, areas, settings) {
                Ok(n) => {
                    // 焦点还给打开面板前用户正在用的窗口。只在这个窗口本身
                    // 属于快照且保存时可见时才激活，避免把保存时最小化的
                    // 窗口重新顶到前台。
                    if let Some(id) = restore_focus {
                        let wanted = crate::layout::normalize_id(id);
                        let saved_visible = snap.windows.iter().any(|r| {
                            !r.hidden && crate::layout::normalize_id(&r.id) == wanted
                        });
                        if saved_visible {
                            if let Ok(win) = x.window_by_id(id) {
                                if !win.title.starts_with("轻格分窗 - ") {
                                    let _ = x.activate(&win);
                                }
                            }
                        }
                    }
                    ok(format!("已加载「{}」，恢复 {} 个窗口", snap.name, n))
                }
                Err(e) => fail(&format!("加载失败：{}", e)),
            }
        }
        "delete" => {
            let i = match idx() {
                Some(i) if i < cfg.data.snapshots.len() => i,
                _ => return fail("布局不存在或已被删除"),
            };
            let name = cfg.data.snapshots.remove(i).name;
            match cfg.save() {
                Ok(_) => ok_changed(format!("已删除「{}」", name)),
                Err(e) => fail(&format!("删除失败：{}", e)),
            }
        }
        "rename" => {
            let i = match idx() {
                Some(i) if i < cfg.data.snapshots.len() => i,
                _ => return fail("布局不存在或已被删除"),
            };
            let new_name = fields.get(2).copied().unwrap_or("").trim().to_string();
            if new_name.is_empty() {
                return fail("名称不能为空");
            }
            if cfg.data.snapshots.iter().enumerate().any(|(k, s)| k != i && s.name == new_name) {
                return fail("已有同名布局，请换一个名称");
            }
            cfg.data.snapshots[i].name = new_name.clone();
            match cfg.save() {
                Ok(_) => ok_changed(format!("已重命名为「{}」", new_name)),
                Err(e) => fail(&format!("重命名失败：{}", e)),
            }
        }
        _ => fail("未知操作"),
    }
}

fn notify(msg: &str) {
    if which("notify-send").is_some() {
        let _ = std::process::Command::new("notify-send")
            .args(["轻格分窗", msg])
            .spawn();
    } else if which("zenity").is_some() {
        let _ = std::process::Command::new("zenity")
            .args(["--info", "--text", msg, "--title", "轻格分窗"])
            .spawn();
    }
}

fn refresh_menu(cfg: &Config, state: &MenuState) -> Result<()> {
    let autostart = autostart_enabled();
    state.set_dark(cfg.is_dark());
    state.set_items(menu::build_menu_items(&cfg.data.snapshots, autostart));
    Ok(())
}

fn autostart_path() -> std::path::PathBuf {
    config::dirs_home().join(".config/autostart/lithezones.desktop")
}

fn autostart_enabled() -> bool {
    autostart_path().exists()
}

fn toggle_autostart(_cfg: &Config) -> Result<()> {
    let path = autostart_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let exe = std::env::current_exe()?;
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=轻格分窗\n\
                 Comment=LitheZones 轻量窗口布局工具\nExec={} --tray\n\
                 Terminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            ),
        )?;
    }
    Ok(())
}

/// 把轻格分窗固定到 deepin dock，让任务栏常驻显示启动器图标。
///
/// 面板最小化后，dock 里若没有本应用图标，用户就看不到恢复入口。
///
/// 实测 deepin V23 的 dock（dde-shell / org.deepin.ds.Dock）收藏应用存在
/// gsettings `com.deepin.dde.dock docked-apps` 里（desktop ID 列表）。旧版
/// 用的 `org.deepin.dde.daemon.Dock1.RequestDock()` 虽然返回 true，但只是
/// V20 兼容层，V23 上是空操作——调了它并不会让图标出现。所以这里直接把
/// `lithezones` 追加进 docked-apps（已有则不动，尊重用户排序）。
///
/// 早期版本因为默认开机自启，登录时应用已运行，dock 会显示「运行中应用」
/// 图标；0.5.39 起按要求不再自启，运行图标就没了，必须真正写进收藏列表。
///
/// 非 deepin 环境没有这个 schema，gsettings 命令失败即静默跳过，不影响主流程。
fn dock_favorites_add_lithezones() {
    use std::process::Command;

    let get = Command::new("gsettings")
        .args(["get", "com.deepin.dde.dock", "docked-apps"])
        .output();
    let out = match get {
        Ok(o) if o.status.success() => o,
        _ => {
            crate::vlog!("dock 图标：非 deepin dock，跳过");
            return;
        }
    };
    let cur = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if cur.contains("lithezones") {
        crate::vlog!("dock 图标：已在收藏列表");
        return;
    }
    // GVariant 字符串数组形如 ['a', 'b']，在结尾 ] 前插入新项。
    let pos = cur.rfind(']').unwrap_or(cur.len());
    let (head, tail) = cur.split_at(pos);
    let sep = if head.trim_end().ends_with('[') { "" } else { ", " };
    let new = format!("{}'lithezones'{}", head, sep) + tail;
    match Command::new("gsettings")
        .args(["set", "com.deepin.dde.dock", "docked-apps", &new])
        .status()
    {
        Ok(s) if s.success() => crate::vlog!("dock 图标：已加入收藏列表"),
        Ok(s) => crate::vlog!("dock 图标：写入失败({})", s),
        Err(e) => crate::vlog!("dock 图标：写入失败({})", e),
    }
}

async fn ensure_dock_icon(_conn: &zbus::Connection) {
    // gsettings 是同步的短命令（<100ms），放到阻塞线程池，别卡住 tokio 主循环。
    match tokio::task::spawn_blocking(dock_favorites_add_lithezones).await {
        Ok(_) => {}
        Err(e) => crate::vlog!("dock 图标：任务执行失败({})", e),
    }
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let p = std::path::Path::new(dir).join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // 桌面图标/启动器点击时带此参数：直接打开窗口排列；未启动则先启动托盘。
    let wants_window_manager = args
        .iter()
        .any(|a| a == "--window-manager" || a == "--manage");

    // --version / --check 不需要 D-Bus
    if args.iter().any(|a| a == "--version") {
        println!("lithezones {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--check") {
        let x = x11::X11::connect()?;
        println!("X11 OK, 工作区: {:?}", x.workareas()?);
        return Ok(());
    }
    // 诊断用：纯计算任意工作区尺寸下的槽位，不连 X11/D-Bus。
    // 用于换分辨率/多屏时核对布局，而不是只看当前这台机器的桌面。
    // 例：--slots=quarter:1366x768
    if let Some(a) = args.iter().find(|s| s.starts_with("--slots=")) {
        let spec = a.trim_start_matches("--slots=");
        let (slug, geom) = spec.split_once(':').unwrap_or((spec, ""));
        let kind = kind_from_slug(slug)
            .ok_or_else(|| anyhow::anyhow!("未知布局：{}", slug))?;
        let wh: Vec<i32> = geom
            .split('x')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if wh.len() != 2 || wh[0] < 1 || wh[1] < 1 {
            return Err(anyhow::anyhow!("尺寸格式应为 WxH，例如 1366x768"));
        }
        let area = Rect::new(0, 0, wh[0], wh[1]);
        let limits = vec![layout::SizeLimit::free(); 4];
        let slots = layout::reflow_slots(kind, area, &limits);
        println!("{} @ {}x{}：{} 格", slug, wh[0], wh[1], slots.len());
        for (i, r) in slots.iter().enumerate() {
            println!(
                "  #{} x={} y={} w={} h={}",
                i + 1, r.x, r.y, r.width, r.height
            );
        }
        return Ok(());
    }
    // 诊断用：请求改变某个窗口的尺寸（位置不变），用于验证拖拽中恢复尺寸。
    if let Some(pos) = args.iter().position(|a| a == "--resize") {
        if args.len() >= pos + 4 {
            let x = x11::X11::connect()?;
            let win = x.window_by_id(&args[pos + 1])?;
            x.request_resize(
                &win,
                args[pos + 2].parse().unwrap_or(0),
                args[pos + 3].parse().unwrap_or(0),
            )?;
            println!(
                "已请求 {} 调整为 {}x{}（位置不变）",
                win.id, args[pos + 2], args[pos + 3]
            );
        }
        return Ok(());
    }

    // 诊断用：打印窗口的 WM_NORMAL_HINTS 最小尺寸。
    if let Some(pos) = args.iter().position(|a| a == "--min-size") {
        let x = x11::X11::connect()?;
        if let Some(id) = args.get(pos + 1) {
            println!("{} min_size={:?}", id, x.min_size_of(id));
        }
        return Ok(());
    }

    // 诊断用：列出当前会被布局处理的窗口，不启动托盘。
    if args.iter().any(|a| a == "--list-windows") {
        let x = x11::X11::connect()?;
        let cfg = Config::load();
        for w in x.managed_windows(&cfg.data.settings)? {
            println!(
                "{} desktop={} hidden={} class={} title={} rect={:?}",
                w.id, w.desktop, w.hidden, w.class, w.title, w.rect
            );
        }
        return Ok(());
    }

    // 诊断用：直接对当前可管理窗口应用指定布局并打印应用前后的几何，
    // 用于截图核对"窗口排列是否真的对齐"，不启动托盘。
    // 可跟一串 0x 窗口 ID 精确模拟面板勾选；不给则处理所有可见窗口。
    if let Some(pos) = args.iter().position(|s| s.starts_with("--apply=")) {
        let slug = args[pos].trim_start_matches("--apply=");
        let kind = kind_from_slug(slug)
            .ok_or_else(|| anyhow::anyhow!("未知布局：{}", slug))?;
        let x = x11::X11::connect()?;
        let cfg = Config::load();
        let ids: Vec<String> = args
            .iter()
            .skip(pos + 1)
            .filter(|a| a.starts_with("0x"))
            .cloned()
            .collect();
        let all = x.managed_windows_with_hidden(&cfg.data.settings)?;
        let windows: Vec<_> = if ids.is_empty() {
            x.managed_windows(&cfg.data.settings)?
        } else {
            ids.iter()
                .filter_map(|id| {
                    all.iter()
                        .find(|w| layout::normalize_id(&w.id) == layout::normalize_id(id))
                        .cloned()
                })
                .collect()
        };
        let areas = x.workareas()?;
        println!("area={:?} 目标={} 窗口数={}", areas.first(), slug, windows.len());
        for w in &windows {
            println!("  before {} {:?} {}", w.id, w.rect, w.title);
        }
        apply(&x, kind, &windows, None, &areas, &cfg.data.settings)?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        for w in x.managed_windows(&cfg.data.settings)? {
            println!("  after  {} {:?} {}", w.id, w.rect, w.title);
        }
        return Ok(());
    }

    // 连接 D-Bus session（SNI 托盘与单实例）
    let conn = connection::Builder::session()?.build().await?;

    // 让任务栏 dock 常驻显示轻格分窗图标：面板最小化后用户仍可点图标恢复。
    // 只在本进程真正成为托盘实例后做（下面的单实例抢占失败会 return），
    // 用 fire-and-forget，避免 dock 不可用时拖慢启动。
    let dock_conn = conn.clone();
    tokio::spawn(async move { ensure_dock_icon(&dock_conn).await });

    // ---- 单实例：用 Identity() 确认持有者就是当前二进制 ----
    // 历史 bug：幽灵名称连自定义 Ping 都会 pong，但进程已不存在；旧版本
    // (/usr/bin 下的 0.3.1) 也一样占名。仅靠 Ping 会让新实例误判"已在运行"而退出，
    // 托盘菜单/双击发的命令掉进黑洞 → 窗口排列打不开。
    // 现在要求 Identity() 返回的「可执行文件绝对路径|版本」与自身完全一致；
    // 否则一律用 ReplaceExisting 抢占，不再依赖会跨 PID 命名空间错位的 /proc 查询。
    let identity = format!(
        "{}|{}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        env!("CARGO_PKG_VERSION")
    );
    let existing: std::result::Result<String, _> = conn
        .call_method(Some(APP_BUS_NAME), APP_PATH, Some(APP_IFACE), "Identity", &())
        .await
        .and_then(|r| r.body().deserialize::<String>());
    let identity_match = existing.as_deref() == Ok(identity.as_str());
    let proc_alive = owner_process_alive(&conn).await;
    // 身份一致就先当作可通知实例；proc_alive 只作诊断，不参与判定。
    // WorkBuddy/沙箱/flatpak 的 PID 命名空间经常看不到 D-Bus 对端进程，
    // kill(pid,0) 会误报"进程已死"，把活着的托盘杀掉再重建；用户看到的就是
    // "应用布局后最小化，再点托盘/启动器整个软件退出"。
    // 真正的僵尸连接由下面的「投递 Event + 等窗口真的出现」兜底：Event
    // 失败/超时/窗口没出现时再 Quit + 抢占重建，比 PID 探活可靠。
    let mut alive = identity_match;
    let wants_quit = args.iter().any(|a| a == "--quit");

    // 启动器/桌面图标点击：先把请求投递给已有实例，并确认窗口真的出现。
    // 历史 bug（"打不开"的真正根源）：下面 match 的 Event 失败/超时两个分支
    // 只打印了"尝试抢占"就结束，既没把 alive 翻成 false，也没退出旧实例，
    // 于是流程落到后面的 `if alive { "已在运行"; return }` 直接返回——
    // 用户点了启动器却石沉大海。三条失败路径必须统一走重建，不允许任何一条
    // 带着 alive=true 漏到下面去。
    // 只有同版本才委托；旧版本/僵尸一律走下面的 Quit + 抢占重建。
    if wants_window_manager && alive {
        let args = (ids::WINDOW_MANAGER, "clicked", Value::new(String::new()), 0u32);
        let event = conn.call_method(
            Some(APP_BUS_NAME),
            "/Menu",
            Some("com.canonical.dbusmenu"),
            "Event",
            &args,
        );
        let shown = match tokio::time::timeout(Duration::from_secs(3), event).await {
            Ok(Ok(_)) => {
                // Event 送达不等于弹窗：老实例可能收下却卡在字体查询/面板线程里。
                wait_for_window_manager(WM_SHOW_TIMEOUT).await
            }
            Ok(Err(e)) => {
                crate::vlog!("启动器 Event 失败（{}），重启实例", e);
                false
            }
            Err(_) => {
                crate::vlog!("启动器 Event 超时，重启实例");
                false
            }
        };
        if shown {
            crate::vlog!("启动器请求：窗口已出现");
            return Ok(());
        }
        // 走到这里说明窗口确实没出来（送达但没弹 / Event 失败 / Event 超时）。
        // 通知旧实例退出、翻转 alive、等名称释放，确保下面进入抢占重建分支。
        crate::vlog!("启动器请求：窗口未出现，退出旧实例并抢占重建");
        let _ = conn
            .call_method(Some(APP_BUS_NAME), APP_PATH, Some(APP_IFACE), "Quit", &())
            .await;
        alive = false;
        if !wait_name_released(&conn, NAME_RELEASE_TIMEOUT).await {
            crate::vlog!("旧实例已通知退出，但 D-Bus 名称仍未释放，继续尝试抢占");
        }
    }

    // --quit 独立于存活判定：只要对方能应答 Identity（活着或僵尸都算），
    // 就先发 Quit 通知它退出。僵尸连接的 Quit 也能让 dbus-daemon 回收连接，
    // 顺带清掉 SNI 注册项。不要因为 PID 判断把它当成"没实例"而跳过。
    if wants_quit && existing.is_ok() {
        match conn
            .call_method(Some(APP_BUS_NAME), APP_PATH, Some(APP_IFACE), "Quit", &())
            .await
        {
            Ok(_) => {
                println!("已通知运行中的 lithezones 退出");
                // 等名称真正释放，紧接着再点启动器时不会被残留名称挡住。
                let _ = wait_name_released(&conn, NAME_RELEASE_TIMEOUT).await;
            }
            Err(e) => eprintln!("通知退出失败：{}", e),
        }
        return Ok(());
    }

    if alive {
        if wants_quit {
            // 上面已处理，这里不会到达。
        } else {
            eprintln!("lithezones 已在运行；退出请执行：lithezones --quit");
        }
        return Ok(());
    }

    if !alive {
        eprintln!(
            "检测到残留/旧版 D-Bus 实例（identity_match={}, proc_alive={}, identity={:?}），强制抢占",
            identity_match,
            proc_alive,
            existing.as_deref().unwrap_or("无响应")
        );
    }

    // 无活实例（含幽灵名称）：强制抢占名称。旧实例 Quit 到 dbus-daemon 真正
    // 释放名称之间有一小段窗口，单次抢占可能失败；重试几次避免启动器白点。
    let mut is_primary = false;
    for attempt in 0..3 {
        let reply = conn
            .request_name_with_flags(
                APP_BUS_NAME,
                BitFlags::from(RequestNameFlags::ReplaceExisting),
            )
            .await;
        is_primary = matches!(
            reply,
            Ok(RequestNameReply::PrimaryOwner) | Ok(RequestNameReply::AlreadyOwner)
        );
        if is_primary || attempt == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !is_primary && wants_window_manager {
        // 抢占失败通常是真有活实例持名，而它可能正是 X11 已断的活壳。
        // 旧逻辑只把 Event 再转发给它——活壳收下也开不出窗，请求就此丢失。
        // 这里再走一轮「Quit 旧实例 → 等名称释放 → 抢占」，确保请求不落空。
        crate::vlog!("抢占失败：再通知旧实例退出并重抢一次");
        let _ = conn
            .call_method(Some(APP_BUS_NAME), APP_PATH, Some(APP_IFACE), "Quit", &())
            .await;
        let _ = wait_name_released(&conn, NAME_RELEASE_TIMEOUT).await;
        for attempt in 0..3 {
            let reply = conn
                .request_name_with_flags(
                    APP_BUS_NAME,
                    BitFlags::from(RequestNameFlags::ReplaceExisting),
                )
                .await;
            is_primary = matches!(
                reply,
                Ok(RequestNameReply::PrimaryOwner) | Ok(RequestNameReply::AlreadyOwner)
            );
            if is_primary || attempt == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    if !is_primary {
        // 二次抢占仍失败：最后把请求转发给持名者，并确认窗口真的出现，
        // 避免"Event 送到了但活壳没弹窗"的石沉大海。
        if wants_window_manager {
            let args = (ids::WINDOW_MANAGER, "clicked", Value::new(String::new()), 0u32);
            let event = conn.call_method(
                Some(APP_BUS_NAME),
                "/Menu",
                Some("com.canonical.dbusmenu"),
                "Event",
                &args,
            );
            match tokio::time::timeout(Duration::from_secs(3), event).await {
                Ok(Ok(_)) => {
                    if !wait_for_window_manager(WM_SHOW_TIMEOUT).await {
                        eprintln!("已通知已有实例，但窗口仍未出现；请执行 lithezones --quit 后重试");
                    }
                }
                _ => eprintln!("无法通知已有实例打开窗口排列"),
            }
        } else if !wants_quit {
            eprintln!("lithezones 已在运行（名称由其它实例持有）");
        }
        return Ok(());
    }
    if wants_quit {
        println!("lithezones 未在运行");
        return Ok(());
    }

    // 退出通道：Quit() 方法与 Ctrl-C 都能结束主循环。
    let (quit_tx, mut quit_rx) = mpsc::unbounded_channel::<()>();
    conn.object_server()
        .at(
            APP_PATH,
            AppControl::new(Arc::new(Mutex::new(Some(quit_tx))), identity.clone()),
        )
        .await?;

    let cfg = Config::load();
    let menu_state = menu::publish(&conn, "/Menu").await?;
    refresh_menu(&cfg, &menu_state)?;

    let (click_tx, mut click_rx) = mpsc::unbounded_channel::<i32>();
    *menu_state.click_tx.lock().unwrap() = Some(click_tx);

    // SNI 服务名带毫秒时间戳：沙箱里 PID 会复用，纯 PID 命名会撞上上次异常
    // 退出遗留的注册项，DDE watcher 会报 "notifier item has been registered"。
    let pid = std::process::id();
    let sni_bus = format!(
        "org.kde.StatusNotifierItem-{}-{}",
        pid,
        chrono::Utc::now().timestamp_millis()
    );
    conn.request_name(sni_bus.as_str()).await?;
    tray::publish(
        &conn,
        "/StatusNotifierItem",
        sni_bus.as_str(),
        cfg.is_dark(),
        menu_state.click_tx.clone(),
    )
    .await?;

    // X11 连接（不需要 async）。Arc 让面板通信线程能复用同一连接。
    let x = Arc::new(x11::X11::connect()?);
    let mut cfg = cfg;
    let (cfg_tx, mut cfg_rx) = tokio::sync::mpsc::unbounded_channel::<PanelUpdate>();
    let panel_ctl = PanelControl {
        open: Arc::new(AtomicBool::new(false)),
        stdin: Arc::new(Mutex::new(None)),
        cfg_back: cfg_tx,
        x: x.clone(),
            minimized_at: Arc::new(Mutex::new(None)),
            self_minimized: Arc::new(AtomicBool::new(false)),
            last_focus: Arc::new(Mutex::new(None)),
        };

    // 后台监测面板窗口的最小化时刻：从「非 HIDDEN」变成「HIDDEN」的瞬间记时间。
    // raise_panel_window 据此判断这次 Activate 是「刚最小化（保持）」还是
    // 「最小化已久（可恢复）」，彻底解决「点 dock 最小化又弹回」与
    // 「最小化后点托盘打不开」这对矛盾。
    {
        let watch_open = panel_ctl.open.clone();
        let watch_min = panel_ctl.minimized_at.clone();
        std::thread::spawn(move || {
            let mut was_hidden = false;
            loop {
                if watch_open.load(Ordering::Acquire) {
                    if let Some(id) = find_panel_window() {
                        let hidden = panel_window_hidden(&id);
                        if hidden && !was_hidden {
                            *watch_min.lock().unwrap() = Some(Instant::now());
                        }
                        was_hidden = hidden;
                    }
                } else {
                    was_hidden = false;
                }
                std::thread::sleep(Duration::from_millis(80));
            }
        });
    }

    // 启动全局快捷键监听线程
    let hotkey = hotkeys::start(&cfg.data.settings)?;
    let hotkey_reloader = hotkey.reloader();
    let mut hotkey_rx = hotkey.actions;

    // 从桌面图标/启动器冷启动时，托盘已经就绪，立刻投递「窗口排列」。
    if wants_window_manager {
        if let Some(tx) = menu_state.click_tx.lock().unwrap().as_ref() {
            let _ = tx.send(ids::WINDOW_MANAGER);
        }
    }

    // 抢占成功后先清掉上一版本遗留的孤儿面板窗口，再打开本版本的新面板。
    // 否则旧面板会浮在新版之上，用户永远看到旧配色/旧逻辑。
    close_stale_panel_windows();
    eprintln!("lithezones {} 已启动", env!("CARGO_PKG_VERSION"));

    // 后台预热面板字体缓存：gsettings/fc-match 子进程合计约 1 秒，
    // 放在启动阶段异步做掉，用户点击菜单时面板可直接使用缓存。
    if let Some(script) = resource_script("panel.py") {
        std::thread::spawn(move || {
            let _ = std::process::Command::new("python3")
                .arg(script)
                .arg("--warm-fonts")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }

    // 菜单点击、快捷键与退出信号主循环
    loop {
        tokio::select! {
            Some(id) = click_rx.recv() => {
                match handle_menu_click(id, &mut cfg, &x, &menu_state, &panel_ctl, &conn).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(e) => eprintln!("菜单操作失败: {}", e),
                }
            }
            Some(action) = hotkey_rx.recv() => {
                let result = match action {
                    hotkeys::HotkeyAction::Layout(kind) => {
                        let windows = x.managed_windows(&cfg.data.settings)?;
                        let areas = x.workareas()?;
                        let focus_id = x.active_window_info()?.map(|w| w.id.clone());
                        apply(&x, kind, &windows, focus_id.as_deref(), &areas, &cfg.data.settings)
                            .map(|_| ())
                    }
                    hotkeys::HotkeyAction::Snapshot(n) => {
                        if let Some(snap) = cfg.data.snapshots.get(n - 1).cloned() {
                            let windows = x.managed_windows(&cfg.data.settings)?;
                            let areas = x.workareas()?;
                            snapshot::restore(&x, &snap, &windows, &areas, &cfg.data.settings)
                                .map(|_| ())
                        } else {
                            Ok(())
                        }
                    }
                    // 打开面板：复用菜单点击通道，走和托盘菜单完全相同的
                    // handle_menu_click 路径（活壳自愈、候选池、置前、单实例）。
                    hotkeys::HotkeyAction::OpenWindowManager
                    | hotkeys::HotkeyAction::OpenLayoutManager => {
                        let id = match action {
                            hotkeys::HotkeyAction::OpenWindowManager => ids::WINDOW_MANAGER,
                            _ => ids::LAYOUT_MANAGER,
                        };
                        if let Some(tx) = menu_state.click_tx.lock().unwrap().as_ref() {
                            let _ = tx.send(id);
                        }
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    eprintln!("快捷键操作失败: {}", e);
                }
            }
            // 面板线程在保存/重命名/删除后立即回传 Config，关闭时再回传 Closed。
            // 中途更新不能清 open 标志，否则面板还开着就被当成已关，下一次点击
            // 会再开一个面板窗口。
            Some(update) = cfg_rx.recv() => {
                match update {
                    PanelUpdate::Config(updated) => {
                        cfg = updated;
                        let _ = refresh_menu(&cfg, &menu_state);
                    }
                    PanelUpdate::ReloadHotkeys(hotkeys) => {
                        hotkey_reloader.reload(hotkeys);
                    }
                    PanelUpdate::Closed => {
                        panel_ctl.open.store(false, Ordering::Release);
                    }
                }
            }
            Some(_) = quit_rx.recv() => {
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    // 退出时一并关掉 tkinter 面板窗口，避免留下旧版本孤儿窗口继续显示在桌面。
    close_stale_panel_windows();
    let _ = conn.release_name(APP_BUS_NAME).await;
    eprintln!("lithezones 退出");
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, class: &str, title: &str, hidden: bool) -> model::WindowInfo {
        model::WindowInfo {
            id: id.to_string(),
            desktop: 0,
            pid: 0,
            rect: Rect::new(10, 20, 500, 300),
            class: class.to_string(),
            title: title.to_string(),
            frame: model::FrameExtents::default(),
            gtk_frame: model::FrameExtents::default(),
            maximized_vert: false,
            maximized_horz: false,
            fullscreen: false,
            hidden,
        }
    }

    #[test]
    fn panel_entries_encode_us_rs_and_hidden() {
        let pool = vec![
            win("0x1", "code", "标题 | 带竖线", false),
            win("0x2", "wechat", "微信", true),
        ];
        let encoded = encode_panel_entries(&pool);
        let records: Vec<&str> = encoded.split(RS).collect();
        assert_eq!(records.len(), 2);
        let first: Vec<&str> = records[0].split(US).collect();
        assert_eq!(first[0], "0x1");
        assert_eq!(first[1], "10");
        assert_eq!(first[2], "20");
        assert_eq!(first[3], "500");
        assert_eq!(first[4], "300");
        assert_eq!(first[5], "code — 标题 | 带竖线");
        assert_eq!(first[6], "0");
        let second: Vec<&str> = records[1].split(US).collect();
        assert_eq!(second[0], "0x2");
        assert_eq!(second[6], "1");
    }

    #[test]
    fn panel_entry_fields_never_break_line_protocol() {
        // 标题里的 TAB/换行以及 US/RS 都必须被去掉，否则面板的逐行协议会错位。
        let field = clean_field("a\tb\nc\rd\u{1f}e\u{1e}f");
        assert_eq!(field, "a b c def");
    }
}
