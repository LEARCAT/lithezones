//! 快照：保存当前布局 / 按相对比例或绝对坐标还原。

use crate::config::Settings;
use crate::layout::normalize_id;
use crate::model::{Rect, Snapshot, SnapshotWindow, WindowInfo};
use crate::x11::X11;
use anyhow::Result;
use chrono::Local;
use std::thread;
use std::time::Duration;

/// 保存当前布局快照。
pub fn capture(
    x11: &X11,
    name: String,
    windows: &[WindowInfo],
    areas: &[Rect],
    _settings: &Settings,
) -> Result<Snapshot> {
    // EWMH 给出的全局层叠顺序（底→顶）。只记录相对次序，不需要连续编号；
    // 读不到时留 -1，加载端就不动层序。
    let stacking = x11.stacking_order().unwrap_or_default();
    let stack_rank = |id: &str| -> i32 {
        let id = normalize_id(id);
        stacking
            .iter()
            .position(|x| normalize_id(x) == id)
            .map(|i| i as i32)
            .unwrap_or(-1)
    };
    let mut records = Vec::new();
    for win in windows {
        let idx = win.area_index(areas);
        let area = areas.get(idx).copied().unwrap_or(areas[0]);
        let rect = win.frame_rect();
        // 相对比例用万分比存储，跨分辨率还原时按比例换算
        let rel = Rect::new(
            ((rect.x - area.x) as f64 / area.width as f64 * 10000.0).round() as i32,
            ((rect.y - area.y) as f64 / area.height as f64 * 10000.0).round() as i32,
            (rect.width as f64 / area.width as f64 * 10000.0).round() as i32,
            (rect.height as f64 / area.height as f64 * 10000.0).round() as i32,
        );
        records.push(SnapshotWindow {
            id: normalize_id(&win.id),
            class: win.class.clone(),
            title: win.title.clone(),
            pid: win.pid,
            desktop: win.desktop,
            rect,
            rel,
            area_index: idx,
            maximized_vert: win.maximized_vert,
            maximized_horz: win.maximized_horz,
            fullscreen: win.fullscreen,
            // 保存时窗口本来就是最小化的，加载后必须继续保持最小化，
            // 不能因为"恢复布局"把它顶到前台。
            hidden: win.hidden,
            stack: stack_rank(&win.id),
        });
    }
    Ok(Snapshot {
        name,
        windows: records,
        created_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    })
}

/// 从快照记录算出「目标可见矩形」。
///
/// capture 写入的 `rect/rel` 都是 `frame_rect()` 语义：普通窗口含 WM 装饰，
/// CSD 窗口去掉 GTK 透明阴影边。几何应用函数 `apply_geometry()` 内部已经
/// 用 `target_geometry()` 把可见矩形换算成 X 客户端几何，所以这里绝不能再
/// 做逆变换，否则会二次补偿（普通窗口多缩一圈、CSD 窗口多扩一圈）。
fn snapshot_visible_rect(rec: &SnapshotWindow, area: Rect, relative: bool) -> Rect {
    if relative {
        Rect::new(
            area.x + (rec.rel.x as f64 / 10000.0 * area.width as f64) as i32,
            area.y + (rec.rel.y as f64 / 10000.0 * area.height as f64) as i32,
            (rec.rel.width as f64 / 10000.0 * area.width as f64) as i32,
            (rec.rel.height as f64 / 10000.0 * area.height as f64) as i32,
        )
    } else {
        rec.rect
    }
}

/// 把 (保存时的层叠次序, 窗口 ID) 整理成从底到顶的 raise 顺序。
fn ordered_stack_ids(mut ranked: Vec<(i32, String)>) -> Vec<String> {
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, id)| id).collect()
}

fn rect_distance(a: Rect, b: Rect) -> i64 {
    (a.x - b.x).abs() as i64
        + (a.y - b.y).abs() as i64
        + (a.width - b.width).abs() as i64
        + (a.height - b.height).abs() as i64
}

fn same_class(rec: &SnapshotWindow, win: &WindowInfo) -> bool {
    rec.class.is_empty() || win.class.is_empty() || rec.class == win.class
}

fn same_title(rec: &SnapshotWindow, win: &WindowInfo) -> bool {
    rec.title.is_empty() || win.title.is_empty() || rec.title == win.title
}

/// PID 未知（旧快照或系统没暴露 _NET_WM_PID）时不参与判别。
fn pid_mismatch(rec: &SnapshotWindow, win: &WindowInfo) -> i32 {
    if rec.pid <= 0 || win.pid <= 0 || rec.pid == win.pid {
        0
    } else {
        1
    }
}

/// 桌面号也只作软偏好：用户换过工作区时仍允许按其它线索找回。
fn desktop_mismatch(rec: &SnapshotWindow, win: &WindowInfo) -> i32 {
    if rec.desktop < 0 || win.desktop < 0 || rec.desktop == win.desktop {
        0
    } else {
        1
    }
}

/// 按 XID + class 找同一个窗口；XID 复用或同类多窗口时，再用 PID、桌面、
/// 保存几何依次消歧。只看第一个匹配会随机把新窗口摆进旧槽位。
fn by_id_and_class<'a>(rec: &SnapshotWindow, current: &'a [WindowInfo]) -> Option<&'a WindowInfo> {
    current
        .iter()
        .filter(|w| normalize_id(&w.id) == rec.id && same_class(rec, w))
        .min_by_key(|w| {
            (
                pid_mismatch(rec, w),
                desktop_mismatch(rec, w),
                rect_distance(w.frame_rect(), rec.rect),
            )
        })
}

/// 按 class 找窗口；同类窗口多个时，优先 PID 相同、标题相同、桌面相同，
/// 最后才用保存几何距离。浏览器/终端常开着多个同 class 窗口，只看标题会配错。
fn by_class<'a>(
    rec: &SnapshotWindow,
    current: &'a [WindowInfo],
    target: Rect,
) -> Option<&'a WindowInfo> {
    current
        .iter()
        .filter(|w| same_class(rec, w))
        .min_by_key(|w| {
            (
                pid_mismatch(rec, w),
                if same_title(rec, w) { 0 } else { 1 },
                desktop_mismatch(rec, w),
                rect_distance(w.frame_rect(), target),
            )
        })
}

/// 还原快照。返回还原的窗口数。
pub fn restore(
    x11: &X11,
    snapshot: &Snapshot,
    current: &[WindowInfo],
    areas: &[Rect],
    settings: &Settings,
) -> Result<usize> {
    let relative = settings.snapshot_restore_mode == "relative";
    let mut count = 0;
    // 可见窗口在全部落位后统一回读纠正一次：窗口 A 的恢复/动画可能让
    // KWin 丢掉紧随其后的窗口 B 的几何请求，逐窗口 set-and-forget 做不到
    // 百分百复现。最大化/全屏状态要在纠正之后再加，避免 WM 反过来改几何。
    let mut final_pass: Vec<(String, Rect)> = Vec::new();
    let mut post_states: Vec<(String, bool, bool, bool)> = Vec::new();
    // 层叠顺序（底→顶）。只在几何/状态都恢复完后再统一设置，避免中间
    // 的 restore/maximize 请求把刚 raised 的窗口又压下去。
    let mut final_stack: Vec<(i32, String)> = Vec::new();

    for rec in &snapshot.windows {
        let area_idx = rec.area_index.min(areas.len().saturating_sub(1));
        let area = areas.get(area_idx).copied().unwrap_or(areas[0]);
        let visible = snapshot_visible_rect(rec, area, relative);

        // ① XID + class：最可靠。旧代码只看 XID，XID 被复用时会把新窗口
        //    摆进旧槽位，用户就看到"加载后完全不是保存时的样子"。
        let found = by_id_and_class(rec, current);
        // ② 候选池在窗口隐藏时可能读不到它；快照里有保存时的 XID，
        //    直接按 XID 再读一次，仍核对 class 防止复用。
        // 声明在外层：候选池外直读的窗口需要在本轮循环结束前一直存活。
        let direct: Option<WindowInfo>;
        let win = match found {
            Some(w) => w,
            None => {
                direct = x11.window_by_id(&rec.id).ok().filter(|w| {
                    normalize_id(&w.id) == rec.id && same_class(rec, w)
                });
                match direct.as_ref() {
                    Some(w) => {
                        crate::vlog!("  [restore] {} 从候选池外按 XID 找回", rec.id);
                        w
                    }
                    None => {
                        // ③ class + title；多个同 class 窗口时用几何距离消歧。
                        match by_class(rec, current, visible) {
                            Some(w) => {
                                crate::vlog!("  [restore] {} 按 class/title 找回", rec.id);
                                w
                            }
                            None => {
                                crate::vlog!("  [restore] {} 未找到匹配窗口，跳过", rec.id);
                                continue;
                            }
                        }
                    }
                }
            }
        };

        crate::vlog!(
            "  [restore] {} visible={:?} 保存时hidden={} 当前hidden={}",
            win.id,
            visible,
            rec.hidden,
            win.hidden
        );

        // 保存时就是最小化的窗口：只把几何写回去，绝不取消最小化。
        // 这里不能走 x11.restore()（wmctrl -a 会解除最小化并抢焦点），
        // 也不能走 apply_geometry()（会清状态/发 EWMH 请求，可能把窗口顶出来）。
        // 对未映射窗口直接 ConfigureWindow，X11 不会映射、激活它。
        if rec.hidden {
            if let Err(e) = x11.configure_hidden_geometry(win, visible, settings) {
                crate::vlog!("  [restore] {} 隐藏态几何写入失败: {}", win.id, e);
            } else {
                count += 1;
            }
            if !win.hidden {
                // 快照里是最小化的，但窗口现在露着：改完几何后重新收起。
                let cur = x11.window_by_id(&win.id).unwrap_or_else(|_| win.clone());
                if let Err(e) = x11.minimize(&cur) {
                    crate::vlog!("  [restore] {} 重新最小化失败: {}", win.id, e);
                }
            }
            continue;
        }

        // 保存时可见、现在被最小化（用户自己最小化/被布局收起）的窗口，
        // 加载布局必须先取消最小化再摆位，否则几何写到位了也看不见。
        let mut geom_win = win.clone();
        if geom_win.hidden {
            if let Err(e) = x11.restore(&geom_win) {
                crate::vlog!("  [restore] {} 取消最小化失败: {}", geom_win.id, e);
            }
            // restore() 会改变实际 frame / _GTK_FRAME_EXTENTS，最小化时读到的
            // 边框是旧值；直接按新 ID 重读，避免用旧补偿把窗口顶偏。
            geom_win = x11.window_by_id(&geom_win.id).unwrap_or(geom_win);
        }
        // apply_geometry 会把可见矩形换算成 X 客户端几何，这里必须直接传 visible。
        if x11.apply_geometry(&geom_win, visible, settings).is_ok() {
            count += 1;
        }
        final_pass.push((geom_win.id.clone(), visible));
        if rec.stack >= 0 {
            final_stack.push((rec.stack, geom_win.id.clone()));
        }
        if rec.maximized_vert || rec.maximized_horz || rec.fullscreen {
            post_states.push((
                geom_win.id.clone(),
                rec.maximized_vert,
                rec.maximized_horz,
                rec.fullscreen,
            ));
        }
    }

    // 等 WM 处理完这一批几何变更，再逐个回读纠正残留偏差。
    thread::sleep(Duration::from_millis(140));
    for (id, visible) in &final_pass {
        let cur = match x11.window_by_id(id) {
            Ok(w) => w,
            Err(_) => continue,
        };
        if cur.hidden || cur.maximized_vert || cur.maximized_horz || cur.fullscreen {
            continue;
        }
        if let Ok(true) = x11.correct_geometry(&cur, *visible, settings) {
            crate::vlog!("  [restore] {} 回读纠正几何 -> {:?}", id, visible);
        }
    }

    // 最后恢复最大化/全屏：这些状态的几何由 WM 控制，必须在几何纠正之后加。
    for (id, mv, mh, fs) in post_states {
        if let Err(e) = x11.apply_saved_states(&id, mv, mh, fs) {
            crate::vlog!("  [restore] {} 还原最大化/全屏状态失败: {}", id, e);
        }
    }

    // 层叠关系：按保存时的从底到顶顺序依次 raise。旧快照 stack=-1 时
    // final_stack 为空，完全不改变当前层序，保持向后兼容。
    if !final_stack.is_empty() {
        let ids = ordered_stack_ids(final_stack);
        if let Err(e) = x11.raise_in_stacking_order(&ids) {
            crate::vlog!("  [restore] 恢复层叠顺序失败: {}", e);
        } else {
            crate::vlog!("  [restore] 已恢复 {} 个窗口的层叠顺序", ids.len());
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FrameExtents;
    use crate::x11::target_geometry;

    fn win(rect: Rect, frame: FrameExtents, gtk_frame: FrameExtents) -> WindowInfo {
        WindowInfo {
            id: "0x1".into(),
            desktop: 0,
            pid: 0,
            rect,
            class: String::new(),
            title: String::new(),
            frame,
            gtk_frame,
            maximized_vert: false,
            maximized_horz: false,
            fullscreen: false,
            hidden: false,
        }
    }

    fn rec(rect: Rect, rel: Rect) -> SnapshotWindow {
        SnapshotWindow {
            id: "0x1".into(),
            class: String::new(),
            title: String::new(),
            pid: 0,
            desktop: 0,
            rect,
            rel,
            area_index: 0,
            maximized_vert: false,
            maximized_horz: false,
            fullscreen: false,
            hidden: false,
            stack: -1,
        }
    }

    #[test]
    fn stacking_ids_are_ordered_bottom_to_top() {
        let ids = ordered_stack_ids(vec![
            (7, "0x3".into()),
            (2, "0x1".into()),
            (5, "0x2".into()),
        ]);
        assert_eq!(ids, vec!["0x1", "0x2", "0x3"]);
    }

    #[test]
    fn snapshot_visible_rect_roundtrips_to_x_geometry() {
        let area = Rect::new(0, 0, 1920, 1030);

        // 普通窗口：capture 存含装饰的可见外框，几何应用层再减掉边框，
        // 应精确回到保存时的 X 客户端几何，不能二次缩小。
        let normal = win(
            Rect::new(100, 40, 800, 600),
            FrameExtents { left: 1, right: 1, top: 30, bottom: 0 },
            FrameExtents::default(),
        );
        let visible = normal.frame_rect();
        let saved = rec(visible, Rect::default());
        let goal = snapshot_visible_rect(&saved, area, false);
        assert_eq!(goal, visible);
        let (x, y, w, h) = target_geometry(goal, &normal, true);
        assert_eq!(Rect::new(x, y, w, h), normal.rect);

        // CSD 窗口：capture 存去掉透明边后的可见内容，几何应用层再外扩
        // GTK 透明边，也应精确回到保存时的 X 几何，不能二次外扩。
        let csd = win(
            Rect::new(960, 0, 960, 600),
            FrameExtents::default(),
            FrameExtents { left: 16, right: 16, top: 10, bottom: 32 },
        );
        let visible2 = csd.frame_rect();
        let saved2 = rec(visible2, Rect::default());
        let goal2 = snapshot_visible_rect(&saved2, area, false);
        assert_eq!(goal2, visible2);
        let (x, y, w, h) = target_geometry(goal2, &csd, true);
        assert_eq!(Rect::new(x, y, w, h), csd.rect);
    }

    #[test]
    fn relative_snapshot_visible_rect_scales_to_area() {
        let area = Rect::new(1920, 100, 1280, 800);
        let saved = rec(
            Rect::default(),
            Rect::new(2500, 1250, 5000, 5000),
        );
        assert_eq!(
            snapshot_visible_rect(&saved, area, true),
            Rect::new(2240, 200, 640, 400)
        );
    }

    fn named(id: &str, class: &str, title: &str, rect: Rect) -> WindowInfo {
        let mut w = win(rect, FrameExtents::default(), FrameExtents::default());
        w.id = id.to_string();
        w.class = class.to_string();
        w.title = title.to_string();
        w
    }

    #[test]
    fn restore_match_rejects_reused_xid() {
        // XID 0x1 已被别的程序复用：不能因为 id 相同就摆错窗口。
        let mut saved = rec(Rect::new(200, 200, 800, 600), Rect::default());
        saved.id = "0x1".into();
        saved.class = "browser".into();
        saved.title = "文档".into();
        let pool = [
            named("0x1", "terminal", "终端", Rect::new(0, 0, 100, 100)),
            named("0x2", "browser", "文档", Rect::new(200, 200, 800, 600)),
        ];
        assert!(by_id_and_class(&saved, &pool).is_none());
        let picked = by_class(&saved, &pool, saved.rect).unwrap();
        assert_eq!(picked.id, "0x2");
    }

    #[test]
    fn restore_match_prefers_pid_over_stale_title() {
        // 浏览器重开后标题从「主页」变成「新标签页」，但 PID 相同的仍是它。
        let mut saved = rec(Rect::new(900, 100, 600, 400), Rect::default());
        saved.class = "browser".into();
        saved.title = "主页".into();
        saved.pid = 4242;
        let mut right = named("0x9", "browser", "新标签页", Rect::new(900, 100, 600, 400));
        right.pid = 4242;
        let mut wrong = named("0x8", "browser", "主页", Rect::new(100, 100, 600, 400));
        wrong.pid = 1111;
        let pool = [wrong, right];
        let picked = by_class(&saved, &pool, saved.rect).unwrap();
        assert_eq!(picked.id, "0x9");
    }

    #[test]
    fn restore_match_uses_geometry_to_disambiguate_same_class() {
        // 同类同标题窗口多个时，选离保存几何最近的那个。
        let mut saved = rec(Rect::new(900, 100, 600, 400), Rect::default());
        saved.class = "browser".into();
        saved.title = "主页".into();
        let pool = [
            named("0x1", "browser", "主页", Rect::new(0, 0, 600, 400)),
            named("0x2", "browser", "主页", Rect::new(900, 100, 600, 400)),
        ];
        let picked = by_class(&saved, &pool, saved.rect).unwrap();
        assert_eq!(picked.id, "0x2");
    }
}
