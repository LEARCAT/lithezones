//! X11 底层封装：基于 x11rb 纯 Rust 实现；几何/状态变更走 wmctrl（dde-kwin 只认 EWMH 请求）。
//! 支持 EWMH 属性读取、窗口枚举、几何设置、状态管理。

/// 详细日志开关：设置 DDE_LAYOUT_DEBUG=1 时才输出逐轮调试信息。
/// 托盘程序平时没有控制台，正常运行时保持安静。
pub fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("DDE_LAYOUT_DEBUG").is_some())
}

/// 仅调试模式下输出的日志。
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {
        if $crate::x11::debug_enabled() {
            eprintln!($($arg)*);
        }
    };
}

use crate::config::Settings;
use crate::model::{FrameExtents, Rect, WindowInfo};
use anyhow::{Context, Result};
use x11rb::atom_manager;
use x11rb::connection::Connection;

/// 把布局算出的「目标外框矩形」换算成写给 X11 的几何。
///
/// 两类窗口语义相反，必须分开处理：
/// - 普通窗口：`rect` 是含装饰的外框，`frame` 是服务端边框，
///   写 X 时要减去边框得到客户端几何（`x+left, w-left-right`）。
/// - CSD 窗口（Chromium/Edge/大象，无 `_NET_FRAME_EXTENTS` 但有
///   `_GTK_FRAME_EXTENTS`）：客户端几何就是窗口几何，且它比可见内容大一圈
///   透明边。要让可见内容填满 `rect`，写 X 时反而要**外扩** gtk_frame，
///   原点反向偏移（`x-left, w+left+right`）。
///
/// `apply_geometry` 与 `expected_client_rect`（回读校验）共用本函数，
/// 保证「设置的几何」与「校验时期望的几何」永远一致。
pub(crate) fn target_geometry(rect: Rect, win: &WindowInfo, compensate: bool) -> (i32, i32, i32, i32) {
    if !compensate {
        return (rect.x, rect.y, rect.width, rect.height);
    }
    let mut x = rect.x + win.frame.left;
    let mut y = rect.y + win.frame.top;
    let mut w = rect.width - win.frame.left - win.frame.right;
    let mut h = rect.height - win.frame.top - win.frame.bottom;
    if !win.gtk_frame.is_zero() {
        x -= win.gtk_frame.left;
        y -= win.gtk_frame.top;
        w += win.gtk_frame.left + win.gtk_frame.right;
        h += win.gtk_frame.top + win.gtk_frame.bottom;
    }
    (x, y, w.max(1), h.max(1))
}
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureRequestEvent, ConfigureWindowAux,
    ConnectionExt, EventMask, StackMode, Window as XWindow, ConfigWindow,
    CONFIGURE_REQUEST_EVENT,
};

atom_manager! {
    pub Atoms: AtomsCookie {
        _NET_SUPPORTED,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CURRENT_DESKTOP,
        _NET_WORKAREA,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_DESKTOP,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_PID,
        _NET_FRAME_EXTENTS,
        _GTK_FRAME_EXTENTS,
        _NET_WM_NAME,
        WM_NAME,
        WM_CLASS,
        WM_TRANSIENT_FOR,
        WM_NORMAL_HINTS,
        UTF8_STRING,
    }
}

pub struct X11 {
    conn: x11rb::rust_connection::RustConnection,
    screen: usize,
    root: XWindow,
    atoms: Atoms,
    /// 被本程序主动收起的窗口 id。排版时要把它们放回候选池，
    /// 否则每切换一次布局，参与排版的窗口就少一批。
    hidden_by_us: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl X11 {
    pub fn connect() -> Result<Self> {
        // 开机/重登自启时，X server（display socket）可能还没就绪。原实现一次
        // 连接失败就 `?` 退出，托盘进程直接死掉且不会自愈——表现就是"重启后
        // 托盘没了、窗口排列/布局管理都打不开"。这里等待并重试，覆盖开机时序。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        let (conn, screen) = loop {
            match x11rb::connect(None) {
                Ok(v) => break v,
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!("无法连接 X11 display（已重试 12 秒）: {:?}", e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        };
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        Ok(Self {
            conn,
            screen,
            root,
            atoms,
            hidden_by_us: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    fn self_hidden(&self, id: &str) -> bool {
        self.hidden_by_us.lock().unwrap().contains(id)
    }

    /// 把被本程序收起的窗口重新映射出来（wmctrl -a 会解除最小化并聚焦）。
    pub fn restore(&self, win: &WindowInfo) -> Result<()> {
        self.wmctrl(&["-i", "-a", win.id.as_str()])?;
        std::thread::sleep(std::time::Duration::from_millis(160));
        self.hidden_by_us.lock().unwrap().remove(&win.id);
        Ok(())
    }

    /// 还原快照记录的 EWMH 状态（最大化 / 全屏）。
    ///
    /// 保存时 maximized/fullscreen 为 true 的窗口，几何会被 WM 按状态重算；
    /// 必须先落几何再置状态，否则加载后看起来"没按保存的样子还原"。
    /// 普通窗口只确认没有残留状态，避免删除快照里本来就没有的标志。
    pub fn apply_saved_states(
        &self,
        id: &str,
        maximized_vert: bool,
        maximized_horz: bool,
        fullscreen: bool,
    ) -> Result<()> {
        let wid_u32 = u32::from_str_radix(id.trim_start_matches("0x"), 16)?;
        let mut props: Vec<&str> = Vec::new();
        if fullscreen {
            props.push("fullscreen");
        }
        if maximized_vert {
            props.push("maximized_vert");
        }
        if maximized_horz {
            props.push("maximized_horz");
        }
        self.clear_max_states(id, wid_u32);
        if props.is_empty() {
            return Ok(());
        }
        self.state_each(id, &props, true);
        std::thread::sleep(std::time::Duration::from_millis(160));
        Ok(())
    }

    /// 给指定顶层窗口设置 WM_CLASS（应用名/类），让 dock 能把面板窗口关联到
    /// lithezones.desktop 启动器图标。Tk 8.6 默认 WM_CLASS 是 tk/Tk，且没有
    /// `wm class` 子命令；Python 侧拿到的 winfo_id 是子窗口 ID，改它无效，
    /// 必须在 Rust 侧对 wmctrl 看到的顶层窗口 ID 直接写 WM_CLASS 属性。
    pub fn set_wm_class(&self, id: &str, name: &str, class: &str) -> Result<()> {
        let wid = u32::from_str_radix(id.trim_start_matches("0x"), 16)?;
        // WM_CLASS 是两条 NUL 结尾的 STRING，连写后以双 NUL 结束。
        let mut data = Vec::new();
        data.extend_from_slice(name.as_bytes());
        data.push(0);
        data.extend_from_slice(class.as_bytes());
        data.push(0);
        self.conn.change_property(
            x11rb::protocol::xproto::PropMode::REPLACE,
            wid,
            self.atoms.WM_CLASS,
            u32::from(x11rb::protocol::xproto::AtomEnum::STRING),
            8,
            data.len() as u32,
            &data,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// 几何/状态变更统一走 wmctrl 的 EWMH 请求。
    /// dde-kwin 对直接 configure_window 反应不稳定（最大化窗口完全不理），
    /// 而 wmctrl 实测能精确落到目标槽位，与旧 Python 版行为一致。
    fn wmctrl(&self, args: &[&str]) -> Result<()> {
        let out = std::process::Command::new("wmctrl")
            .args(args)
            .output()
            .context("无法执行 wmctrl，请确认 deb 依赖已安装")?;
        if !out.status.success() {
            anyhow::bail!("wmctrl 执行失败: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// 确保窗口退出 最大化/全屏 状态，带回读校验（KWin 偶尔会漏掉 remove）。
    fn clear_max_states(&self, wid: &str, wid_u32: u32) {
        for round in 0..3 {
            // 已经处于普通状态时不要再发 wmctrl：每次 fork 约 20ms，
            // 再加固定 sleep，N 个窗口的布局会被这些无用等待拖慢数秒。
            if let Ok(cur) = self.read_window(wid_u32) {
                if !cur.maximized_vert && !cur.maximized_horz && !cur.fullscreen {
                    return;
                }
            }
            self.state_each(wid, &["maximized_vert", "maximized_horz", "fullscreen"], false);
            std::thread::sleep(std::time::Duration::from_millis(if round == 0 { 60 } else { 100 }));
        }
    }

    /// 批量增删 EWMH 状态。wmctrl 的 -b 只解析两个属性名，三个及以上会被
    /// 静默忽略（退出码仍为 0），所以按两个一组发送，避免属性丢失又不多起进程。
    fn state_each(&self, wid: &str, props: &[&str], add: bool) {
        let verb = if add { "add" } else { "remove" };
        for pair in props.chunks(2) {
            let spec = format!("{},{}", verb, pair.join(","));
            if let Err(e) = self.wmctrl(&["-i", "-r", wid, "-b", spec.as_str()]) {
                eprintln!("  [geo] {} {} 失败: {}", wid, spec, e);
            }
        }
    }

    // ---------- 属性读取辅助 ----------

    fn prop_bytes(&self, win: XWindow, atom: u32) -> Result<Vec<u8>> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::ANY, 0, u32::MAX)?
            .reply()?;
        Ok(reply.value)
    }

    fn prop_u32(&self, win: XWindow, atom: u32) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::ANY, 0, u32::MAX)?
            .reply()?;
        Ok(reply
            .value32()
            .map(|iter| iter.collect())
            .unwrap_or_default())
    }

    fn prop_string(&self, win: XWindow, atom: u32) -> Result<String> {
        let bytes = self.prop_bytes(win, atom)?;
        Ok(String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_string())
    }

    // ---------- EWMH 读取 ----------

    /// 读取所有工作区（`_NET_WORKAREA`），每个是 {x,y,w,h}。
    pub fn workareas(&self) -> Result<Vec<Rect>> {
        let values = self.prop_u32(self.root, self.atoms._NET_WORKAREA)?;
        let mut areas = Vec::new();
        for chunk in values.chunks(4) {
            if chunk.len() == 4 {
                areas.push(Rect::new(
                    chunk[0] as i32,
                    chunk[1] as i32,
                    chunk[2] as i32,
                    chunk[3] as i32,
                ));
            }
        }
        if areas.is_empty() {
            let s = &self.conn.setup().roots[self.screen];
            areas.push(Rect::new(0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32));
        }
        Ok(areas)
    }

    /// 当前活动窗口的 XID。
    pub fn active_window(&self) -> Result<Option<u32>> {
        let vals = self.prop_u32(self.root, self.atoms._NET_ACTIVE_WINDOW)?;
        Ok(vals.first().filter(|&&v| v != 0).copied())
    }

    /// 枚举可管理窗口，过滤掉弹窗、Dock、桌面、隐藏窗口。
    pub fn managed_windows(&self, settings: &Settings) -> Result<Vec<WindowInfo>> {
        self.managed_windows_inner(settings, false)
    }

    /// 读取 WM_NORMAL_HINTS 里的程序指定最小尺寸（flags & PMinSize）。
    /// 回读几何推断的"最小尺寸"经常把普通宽度误当成下限，必须优先用真实 hint。
    pub fn min_size_of(&self, id: &str) -> Option<(i32, i32)> {
        let wid = u32::from_str_radix(id.trim_start_matches("0x"), 16).ok()?;
        let v = self.prop_u32(wid, self.atoms.WM_NORMAL_HINTS).ok()?;
        // XSizeHints flags: PMinSize = 0x10（0x02 是 USSize，之前写错导致一直读不到）。
        // 数组：flags, x, y, w, h, min_w, min_h, max_w, max_h, ...
        if v.len() < 7 || v[0] & 0x10 == 0 {
            return None;
        }
        let (w, h) = (v[5] as i32, v[6] as i32);
        if w > 0 && h > 0 {
            Some((w, h))
        } else {
            None
        }
    }

    /// 读取程序指定的最大尺寸；min==max 的窗口（如远程控制客户端）无法拉伸，
    /// 布局时要把它当固定尺寸处理，不能按槽位比例分配。
    pub fn max_size_of(&self, id: &str) -> Option<(i32, i32)> {
        let wid = u32::from_str_radix(id.trim_start_matches("0x"), 16).ok()?;
        let v = self.prop_u32(wid, self.atoms.WM_NORMAL_HINTS).ok()?;
        if v.len() < 9 || v[0] & 0x20 == 0 {
            return None;
        }
        let (w, h) = (v[7] as i32, v[8] as i32);
        if w > 0 && h > 0 && w < i32::MAX && h < i32::MAX {
            Some((w, h))
        } else {
            None
        }
    }

    /// 窗口排列面板的候选池：包含所有最小化的窗口。
    /// 用户要求"即便最小化的窗口也要在选择列表显示"，选中后 apply 会先 restore 再摆位。
    pub fn managed_windows_with_hidden(&self, settings: &Settings) -> Result<Vec<WindowInfo>> {
        self.managed_windows_full(settings, true)
    }

    /// EWMH 层叠顺序（从底到顶）。快照靠它记住窗口的上下层关系：
    /// 几何相同但重叠的两个窗口，谁盖住谁只有这个属性能表达。
    pub fn stacking_order(&self) -> Result<Vec<String>> {
        Ok(self
            .prop_u32(self.root, self.atoms._NET_CLIENT_LIST_STACKING)?
            .iter()
            .map(|id| format!("0x{:x}", id))
            .collect())
    }

    /// 按从底到顶的顺序依次 XRaiseWindow，恢复快照保存时的层叠关系。
    /// 不用 wmctrl -a：那会同时改焦点，把最后 raised 的窗口顶到前台抢走用户输入。
    pub fn raise_in_stacking_order(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            let wid = u32::from_str_radix(id.trim_start_matches("0x"), 16)?;
            self.conn
                .configure_window(wid, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn managed_windows_full(
        &self,
        settings: &Settings,
        include_hidden: bool,
    ) -> Result<Vec<WindowInfo>> {
        let client_ids = self.prop_u32(self.root, self.atoms._NET_CLIENT_LIST)?;
        let mut result = Vec::new();
        for &wid in &client_ids {
            let win = match self.read_window(wid) {
                Ok(w) => w,
                Err(e) => {
                    crate::vlog!("读取窗口 0x{:x} 失败: {}", wid, e);
                    continue;
                }
            };
            // 只过滤 Dock/桌面/弹窗等非普通窗口，不因 hidden 排除。
            if !self.is_shape_manageable(&win) {
                continue;
            }
            if !settings.all_desktops {
                let current = self
                    .prop_u32(self.root, self.atoms._NET_CURRENT_DESKTOP)?
                    .first()
                    .copied()
                    .unwrap_or(0) as i32;
                if win.desktop >= 0 && win.desktop != current {
                    continue;
                }
            }
            if !win.hidden {
                self.hidden_by_us.lock().unwrap().remove(&win.id);
            }
            let _ = include_hidden;
            result.push(win);
        }
        Ok(result)
    }

    fn managed_windows_inner(
        &self,
        settings: &Settings,
        include_self_hidden: bool,
    ) -> Result<Vec<WindowInfo>> {
        let client_ids = self.prop_u32(self.root, self.atoms._NET_CLIENT_LIST)?;
        let mut result = Vec::new();

        for &wid in &client_ids {
            let win = match self.read_window(wid) {
                Ok(w) => w,
                Err(e) => {
                    crate::vlog!("读取窗口 0x{:x} 失败: {}", wid, e);
                    continue;
                }
            };

            let mut keep = self.is_manageable(&win);
            if !keep && win.hidden && include_self_hidden && self.self_hidden(&win.id) {
                // 是之前被本程序收起的：放回候选池
                keep = true;
            }
            if !keep {
                continue;
            }
            if !win.hidden {
                // 用户手动恢复了，记录作废
                self.hidden_by_us.lock().unwrap().remove(&win.id);
            }
            if !settings.all_desktops {
                let current = self
                    .prop_u32(self.root, self.atoms._NET_CURRENT_DESKTOP)?
                    .first()
                    .copied()
                    .unwrap_or(0) as i32;
                if win.desktop >= 0 && win.desktop != current {
                    continue;
                }
            }
            result.push(win);
        }
        Ok(result)
    }

    fn is_manageable(&self, win: &WindowInfo) -> bool {
        if win.hidden {
            return false;
        }
        self.is_shape_manageable(win)
    }

    /// 只看窗口"形状/类型"是否可管理，不检查 hidden（最小化窗口也返回 true）。
    fn is_shape_manageable(&self, win: &WindowInfo) -> bool {
        // 排除 DDE Shell 自带的 Dock / 桌面窗口
        let c = win.class.to_lowercase();
        let t = win.title.to_lowercase();
        if c.contains("dde-shell") || t == "桌面" || t.contains("dock") {
            return false;
        }
        // 排除本软件自己的面板/对话框窗口。面板标题固定为
        // 「轻格分窗 - 管理面板」，保存/加载等对话框为「轻格分窗 - xxx」。
        // 不排除的话，面板打开后会进入窗口候选池；用户点“应用布局”或后台
        // 快捷键触发 apply 时，面板会被当成普通窗口摆进槽位甚至最小化，
        // 表现为“面板点开后消失/根本打不开”。
        if t.starts_with("轻格分窗 - ") || t == "轻格分窗" {
            return false;
        }
        // 过滤 transient 弹窗
        if self
            .prop_u32(u32::from_str_radix(win.id.trim_start_matches("0x"), 16).unwrap_or(0), self.atoms.WM_TRANSIENT_FOR)
            .map(|v| !v.is_empty() && v[0] != 0)
            .unwrap_or(false)
        {
            return false;
        }
        // 过滤非 normal/dialog/utility 类型
        let types = self
            .prop_u32(u32::from_str_radix(win.id.trim_start_matches("0x"), 16).unwrap_or(0), self.atoms._NET_WM_WINDOW_TYPE)
            .unwrap_or_default();
        if !types.is_empty() {
            let allowed = [
                self.atoms._NET_WM_WINDOW_TYPE_NORMAL,
                self.atoms._NET_WM_WINDOW_TYPE_DIALOG,
                self.atoms._NET_WM_WINDOW_TYPE_UTILITY,
            ];
            if !types.iter().any(|t| allowed.contains(t)) {
                return false;
            }
        }
        true
    }

    fn read_window(&self, wid: u32) -> Result<WindowInfo> {
        let geom = self.conn.get_geometry(wid)?.reply()?;
        let coords = self.conn.translate_coordinates(wid, self.root, 0, 0)?.reply()?;

        let class = self
            .prop_bytes(wid, self.atoms.WM_CLASS)
            .map(|b| {
                let parts: Vec<&[u8]> = b.split(|c| *c == 0).filter(|s| !s.is_empty()).collect();
                parts
                    .get(1)
                    .or_else(|| parts.first())
                    .map(|s| String::from_utf8_lossy(s).to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let title = self
            .prop_string(wid, self.atoms._NET_WM_NAME)
            .or_else(|_| self.prop_string(wid, self.atoms.WM_NAME))
            .unwrap_or_default();

        let desktop = self
            .prop_u32(wid, self.atoms._NET_WM_DESKTOP)?
            .first()
            .map(|&v| v as i32)
            .unwrap_or(0);

        let pid = self
            .prop_u32(wid, self.atoms._NET_WM_PID)?
            .first()
            .copied()
            .unwrap_or(0) as i32;

        let frame_extents = self.prop_u32(wid, self.atoms._NET_FRAME_EXTENTS)?;
        let frame = if frame_extents.len() >= 4 {
            FrameExtents {
                left: frame_extents[0] as i32,
                right: frame_extents[1] as i32,
                top: frame_extents[2] as i32,
                bottom: frame_extents[3] as i32,
            }
        } else {
            FrameExtents::default()
        };

        // CSD 窗口（Chromium/Edge/大象/Electron）用 _GTK_FRAME_EXTENTS 报告四周
        // 的透明阴影边：可见内容 = 窗口几何 − gtk_frame。dde-kwin 平铺这类窗口时
        // 会保留这块透明边，如果按格子直接设窗口几何，底部/右侧就会露出壁纸，
        // 看起来像「没严丝合缝」。读出来，写几何时反向补偿。
        let gtk_extents = self.prop_u32(wid, self.atoms._GTK_FRAME_EXTENTS)?;
        let gtk_frame = if gtk_extents.len() >= 4 {
            FrameExtents {
                left: gtk_extents[0] as i32,
                right: gtk_extents[1] as i32,
                top: gtk_extents[2] as i32,
                bottom: gtk_extents[3] as i32,
            }
        } else {
            FrameExtents::default()
        };

        let state = self.prop_u32(wid, self.atoms._NET_WM_STATE)?;
        let has = |atom: u32| state.contains(&atom);

        Ok(WindowInfo {
            id: format!("0x{:x}", wid),
            desktop,
            pid,
            rect: Rect::new(
                coords.dst_x as i32,
                coords.dst_y as i32,
                geom.width as i32,
                geom.height as i32,
            ),
            class,
            title,
            frame,
            gtk_frame,
            maximized_vert: has(self.atoms._NET_WM_STATE_MAXIMIZED_VERT),
            maximized_horz: has(self.atoms._NET_WM_STATE_MAXIMIZED_HORZ),
            fullscreen: has(self.atoms._NET_WM_STATE_FULLSCREEN),
            hidden: has(self.atoms._NET_WM_STATE_HIDDEN),
        })
    }

    // ---------- 几何操作 ----------

    /// 应用几何。settings.frame_compensation 为 true 时，rect 表示外框目标，
    /// 会自动减去边框得到客户端几何。
    ///
    /// dde-kwin 的两个脾气必须照顾：
    /// 1. 窗口处于最大化/全屏时完全忽略 `-e`，而调用方手里的 WindowInfo 可能是
    ///    刷新前的快照（maximized 标志已过期），所以这里**无条件**取消这三种状态；
    /// 2. 取消最大化带回弹动画，动画期间发出的 `-e` 会被丢弃；只 sleep 一次不够可靠，
    ///    改成"设置 → 回读校验 → 不一致再试"，最多 4 轮。
    pub fn apply_geometry(
        &self,
        win: &WindowInfo,
        rect: Rect,
        settings: &Settings,
    ) -> Result<()> {
        let wid = win.id.as_str();
        let wid_u32 = u32::from_str_radix(wid.trim_start_matches("0x"), 16)?;

        // 清掉会阻止几何变更的状态。调用方手里的 WindowInfo 可能是旧快照，
        // 以"马上回读到的当前状态"为准：确实还最大化/全屏才发请求。
        // 普通窗口直接跳过，省掉每窗口 3 次 wmctrl + 固定等待。
        let need_clear = self
            .read_window(wid_u32)
            .map(|cur| cur.maximized_vert || cur.maximized_horz || cur.fullscreen)
            .unwrap_or(true);
        if need_clear {
            self.clear_max_states(wid, wid_u32);
            // 刚解除最大化时 KWin 会跑还原动画，动画期间发出的 -e 会被丢弃。
            std::thread::sleep(std::time::Duration::from_millis(120));
        }

        // 解除最大化 / 窗口刚映射后，_NET_FRAME_EXTENTS、_GTK_FRAME_EXTENTS
        // 都可能变化。继续用调用方手里的旧 win 换算客户端几何，会让 spec 与
        // 回读校验用的是两套边框，表现为「个别窗口偏十几像素、再点一次才对」。
        let live = self.read_window(wid_u32).unwrap_or_else(|_| win.clone());
        let (x, y, w, h) = target_geometry(rect, &live, settings.frame_compensation);
        // 不能用固定下限 160x100：小屏或分屏后的槽位可能比这还窄，
        // 硬顶会把窗口撑过槽位、和邻居重叠。布局层已保证槽位 >=1 且尊重
        // 窗口自身 min，这里只做防御性的正值保护。
        let spec = format!("0,{},{},{},{}", x, y, w, h);

        let mut prev: Option<Rect> = None;
        for attempt in 0..5u32 {
            // 第一轮失败多半是 dde-kwin 的「分屏/快速平铺」钉住了几何。
            // 这种状态不会出现在 _NET_WM_STATE 里，只能靠 add→remove 最大化把它顶出来。
            if attempt == 1 {
                crate::vlog!("  [geo] {} 几何被锁定，尝试解除分屏/平铺状态", wid);
                self.state_each(wid, &["maximized_vert", "maximized_horz"], true);
                std::thread::sleep(std::time::Duration::from_millis(240));
                self.clear_max_states(wid, wid_u32);
                std::thread::sleep(std::time::Duration::from_millis(240));
            }
            if let Err(e) = self.wmctrl(&["-i", "-r", wid, "-e", spec.as_str()]) {
                eprintln!("  [geo] {} 第{}轮 -e 失败: {}", wid, attempt, e);
            }
            std::thread::sleep(std::time::Duration::from_millis(90 + attempt as u64 * 70));
            // 回读：位置和尺寸都到位才算成功，否则重发（动画/状态竞争时有用）
            if let Ok(cur) = self.read_window(wid_u32) {
                let expected = self.expected_client_rect(&cur, rect, settings);
                if let Ok(actual) = self.actual_rect(&cur) {
                    crate::vlog!(
                        "  [geo] {} 第{}轮 期望={:?} 实际={:?} state(max={},hidden={})",
                        wid, attempt, expected, actual,
                        cur.maximized_vert || cur.maximized_horz, cur.hidden
                    );
                    if (actual.x - expected.x).abs() <= 2
                        && (actual.y - expected.y).abs() <= 2
                        && (actual.width - expected.width).abs() <= 2
                        && (actual.height - expected.height).abs() <= 2
                    {
                        return Ok(());
                    }
                    // 连续两轮几何一模一样，说明不是"还没处理完"，而是被最小尺寸
                    // 或 WM 规则顶住了，再重试也没用，交给上层 reflow。
                    if prev == Some(actual) {
                        return Ok(());
                    }
                    prev = Some(actual);
                }
            }
        }
        Ok(())
    }

    /// 给最小化/未映射窗口直接写 X 几何。
    ///
    /// 恢复快照时，如果某个窗口在保存时就是最小化的，加载布局必须保留这个
    /// 状态。此时不能调用 `apply_geometry()`（它走 wmctrl EWMH 请求，可能把
    /// 窗口解除最小化甚至抢焦点），而是直接用 ConfigureWindow 改 X 几何。
    /// 对未映射窗口发 ConfigureRequest 不会映射、不会激活它。
    pub fn configure_hidden_geometry(
        &self,
        win: &WindowInfo,
        rect: Rect,
        settings: &Settings,
    ) -> Result<()> {
        let wid = u32::from_str_radix(win.id.trim_start_matches("0x"), 16)?;
        let (x, y, w, h) = target_geometry(rect, win, settings.frame_compensation);
        self.conn
            .configure_window(
                wid,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(w.max(1) as u32)
                    .height(h.max(1) as u32),
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    /// 按窗口 ID 字符串读取窗口信息（供尺寸守护等外部模块使用）。
    pub fn window_by_id(&self, id: &str) -> Result<WindowInfo> {
        let wid = u32::from_str_radix(id.trim_start_matches("0x"), 16)?;
        self.read_window(wid)
    }

    /// 请求改变窗口尺寸，但**不动位置**。
    ///
    /// 走 ICCCM 的合成 ConfigureRequest（发给 root，由 WM 处理），
    /// 而不是 wmctrl -e / XResizeWindow：
    /// 后两者会让 dde-kwin 取消正在进行的交互式拖拽，窗口当场脱离鼠标；
    /// 合成 ConfigureRequest 被 WM 当作普通客户端请求处理，
    /// 拖拽会话不中断，窗口继续跟着鼠标走。
    pub fn request_resize(&self, win: &WindowInfo, width: i32, height: i32) -> Result<()> {
        let wid = u32::from_str_radix(win.id.trim_start_matches("0x"), 16)?;
        // 位置沿用当前值：只改尺寸，避免把窗口从鼠标下面拽走
        let coords = self.conn.translate_coordinates(wid, self.root, 0, 0)?.reply()?;
        let event = ConfigureRequestEvent {
            response_type: CONFIGURE_REQUEST_EVENT,
            stack_mode: StackMode::ABOVE,
            sequence: 0,
            parent: self.root,
            window: wid,
            sibling: 0,
            x: coords.dst_x as i16,
            y: coords.dst_y as i16,
            width: width.clamp(1, 32767) as u16,
            height: height.clamp(1, 32767) as u16,
            border_width: 0,
            value_mask: ConfigWindow::WIDTH | ConfigWindow::HEIGHT,
        };
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// 读取窗口实际客户端几何。
    pub fn actual_rect(&self, win: &WindowInfo) -> Result<Rect> {
        let wid = u32::from_str_radix(win.id.trim_start_matches("0x"), 16)?;
        let geom = self.conn.get_geometry(wid)?.reply()?;
        let coords = self.conn.translate_coordinates(wid, self.root, 0, 0)?.reply()?;
        Ok(Rect::new(
            coords.dst_x as i32,
            coords.dst_y as i32,
            geom.width as i32,
            geom.height as i32,
        ))
    }

    /// 期望写给 X11 的几何（与 apply_geometry 用同一套补偿公式）。
    /// actual_rect() 回读的就是这个几何，二者必须一致，否则校验永远不通过。
    pub fn expected_client_rect(&self, win: &WindowInfo, rect: Rect, settings: &Settings) -> Rect {
        let (x, y, w, h) = target_geometry(rect, win, settings.frame_compensation);
        Rect::new(x, y, w, h)
    }

    /// 纠正几何偏差：比较实际几何与期望几何，偏差 >2px 时重新设置。
    pub fn correct_geometry(
        &self,
        win: &WindowInfo,
        rect: Rect,
        settings: &Settings,
    ) -> Result<bool> {
        let expected = self.expected_client_rect(win, rect, settings);
        let actual = self.actual_rect(win)?;
        if (actual.x - expected.x).abs() <= 2
            && (actual.y - expected.y).abs() <= 2
            && (actual.width - expected.width).abs() <= 2
            && (actual.height - expected.height).abs() <= 2
        {
            return Ok(false);
        }
        // 走完整的 apply_geometry：它带"解除分屏锁定 + 回读重试"，
        // 单发一次 -e 在 dde-kwin 上很容易被吞。
        self.apply_geometry(win, rect, settings)?;
        Ok(true)
    }

    /// 最小化窗口：发送 WM_CHANGE_STATE(IconicState) 到 root，等价 XIconifyWindow。
    /// KWin 不允许外部直接设置 _NET_WM_STATE_HIDDEN（wmctrl add,hidden 实测无效），
    /// 必须走 ICCCM 的 IconicState 请求。
    pub fn minimize(&self, win: &WindowInfo) -> Result<()> {
        let wid = u32::from_str_radix(win.id.trim_start_matches("0x"), 16)?;

        // 直接对「平铺/最大化」窗口发 IconicState，dde-kwin 会把它顶成最大化并抢焦点，
        // 结果反而盖在布局上面。所以先把这些状态摘掉，恢复成普通窗口再收起。
        if win.maximized_vert || win.maximized_horz || win.fullscreen {
            self.clear_max_states(win.id.as_str(), wid);
            std::thread::sleep(std::time::Duration::from_millis(150));
        }

        let wm_change_state = self.conn.intern_atom(false, b"WM_CHANGE_STATE")?.reply()?.atom;
        for attempt in 0..3u32 {
            let event = ClientMessageEvent::new(
                32,
                wid,
                wm_change_state,
                ClientMessageData::from([3u32, 0, 0, 0, 0]),
            );
            self.conn.send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(120));
            // 回读确认真的进去了
            if let Ok(cur) = self.read_window(wid) {
                if cur.hidden {
                    self.hidden_by_us.lock().unwrap().insert(win.id.clone());
                    return Ok(());
                }
                if attempt == 1 {
                    // 第二轮回读还露着，说明它又被顶成最大化：摘掉状态再收一次
                    crate::vlog!("  [min] {} 收起后未进入隐藏态，重试", win.id);
                    self.state_each(
                        win.id.as_str(),
                        &["maximized_vert", "maximized_horz", "fullscreen"],
                        false,
                    );
                }
            }
        }
        anyhow::bail!("窗口未进入隐藏状态")
    }

    /// 激活窗口。
    pub fn activate(&self, win: &WindowInfo) -> Result<()> {
        let wid = u32::from_str_radix(win.id.trim_start_matches("0x"), 16)?;
        let data = [1u32, 0, 0, 0, 0]; // _NET_ACTIVE_WINDOW, source=1
        let event = ClientMessageEvent::new(
            32,
            wid,
            self.atoms._NET_ACTIVE_WINDOW,
            x11rb::protocol::xproto::ClientMessageData::from(data),
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// 聚焦窗口对应的 XID（用于按焦点选工作区）。
    pub fn active_window_info(&self) -> Result<Option<WindowInfo>> {
        match self.active_window()? {
            Some(wid) => Ok(self.read_window(wid).ok()),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win_with(frame: FrameExtents, gtk: FrameExtents) -> WindowInfo {
        WindowInfo {
            id: "0x1".into(),
            desktop: 0,
            pid: 0,
            rect: Rect::new(0, 0, 1, 1),
            class: String::new(),
            title: String::new(),
            frame,
            gtk_frame: gtk,
            maximized_vert: false,
            maximized_horz: false,
            fullscreen: false,
            hidden: false,
        }
    }

    #[test]
    fn target_geometry_csd_expands_transparent_frame() {
        // Edge 类 CSD：目标可见区 (960,0,960,600)，gtk=(16,16,10,32)
        // 写 X 应外扩成 (944,-10,992,642)，可见内容才精确落回目标格。
        let w = win_with(FrameExtents::default(), FrameExtents {
            left: 16, right: 16, top: 10, bottom: 32,
        });
        assert_eq!(
            target_geometry(Rect::new(960, 0, 960, 600), &w, true),
            (944, -10, 992, 642)
        );
        // 关闭补偿时不改几何。
        assert_eq!(
            target_geometry(Rect::new(960, 0, 960, 600), &w, false),
            (960, 0, 960, 600)
        );
    }

    #[test]
    fn target_geometry_normal_window_subtracts_frame() {
        let w = win_with(FrameExtents { left: 1, right: 1, top: 30, bottom: 0 },
                         FrameExtents::default());
        assert_eq!(
            target_geometry(Rect::new(0, 0, 800, 600), &w, true),
            (1, 30, 798, 570)
        );
    }

    #[test]
    fn target_geometry_of_frame_rect_is_original_x_geometry() {
        // 快照保存的是可见矩形（frame_rect），几何应用层的目标必须能回到
        // 原始 X 客户端几何：这证明恢复时直接传 frame_rect 即可，不需要逆变换。
        let mut normal = win_with(
            FrameExtents { left: 1, right: 1, top: 30, bottom: 0 },
            FrameExtents::default(),
        );
        normal.rect = Rect::new(100, 40, 800, 600);
        let (x, y, w, h) = target_geometry(normal.frame_rect(), &normal, true);
        assert_eq!((x, y, w, h), (100, 40, 800, 600));

        let mut csd = win_with(
            FrameExtents::default(),
            FrameExtents { left: 16, right: 16, top: 10, bottom: 32 },
        );
        csd.rect = Rect::new(960, 0, 960, 600);
        let (x, y, w, h) = target_geometry(csd.frame_rect(), &csd, true);
        assert_eq!((x, y, w, h), (960, 0, 960, 600));
    }
}
