//! 布局计算与执行：六种快速布局、实际几何回读纠正、多余窗口收起。

use crate::config::Settings;
use crate::model::{FrameExtents, LayoutKind, Rect, WindowInfo};
use crate::x11::X11;
use anyhow::Result;
use std::thread;
use std::time::Duration;

/// 未设置上限时用一个足够大、但不会在相加时溢出的值。
const INF: i32 = i32::MAX / 4;

/// 单个窗口的尺寸约束，来自 WM_NORMAL_HINTS。
/// 只看 min 不够：微信等窗口 min==max（280x380），分给它的格子再大也拉不动，
/// 结果就是"窗口按格子左上角摆着、旁边留一大块空白"。必须同时把 max 纳入水填，
/// 让邻居吃掉它用不完的空间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeLimit {
    pub min_w: i32,
    pub min_h: i32,
    pub max_w: i32,
    pub max_h: i32,
}

impl SizeLimit {
    pub fn free() -> Self {
        Self { min_w: 0, min_h: 0, max_w: INF, max_h: INF }
    }

    pub fn new(min: (i32, i32), max: Option<(i32, i32)>) -> Self {
        let (max_w, max_h) = max.unwrap_or((INF, INF));
        Self {
            min_w: min.0.max(0),
            min_h: min.1.max(0),
            max_w: max_w.max(1).min(INF),
            max_h: max_h.max(1).min(INF),
        }
    }

}

fn sat_add(a: i32, b: i32) -> i32 {
    a.saturating_add(b).min(INF)
}

/// 按布局类型和工作区计算槽位（外框几何）。
pub fn slots_for(kind: LayoutKind, area: Rect) -> Vec<Rect> {
    let x = area.x;
    let y = area.y;
    let w = area.width;
    let h = area.height;
    let half_w = w / 2;
    let half_h = h / 2;
    let third_w = w / 3;

    match kind {
        LayoutKind::Quarter => vec![
            Rect::new(x, y, half_w, half_h),
            Rect::new(x + half_w, y, w - half_w, half_h),
            Rect::new(x, y + half_h, half_w, h - half_h),
            Rect::new(x + half_w, y + half_h, w - half_w, h - half_h),
        ],
        LayoutKind::TopBottom => vec![
            Rect::new(x, y, w, half_h),
            Rect::new(x, y + half_h, w, h - half_h),
        ],
        LayoutKind::LeftRight => vec![
            Rect::new(x, y, half_w, h),
            Rect::new(x + half_w, y, w - half_w, h),
        ],
        LayoutKind::Left1Right2 => vec![
            Rect::new(x, y, third_w, h),
            Rect::new(x + third_w, y, w - third_w, half_h),
            Rect::new(x + third_w, y + half_h, w - third_w, h - half_h),
        ],
        // 上1下2：上下各半高，上排占满整宽，下排切左右两列
        LayoutKind::Top1Bottom2 => vec![
            Rect::new(x, y, w, half_h),
            Rect::new(x, y + half_h, half_w, h - half_h),
            Rect::new(x + half_w, y + half_h, w - half_w, h - half_h),
        ],
        // 左2右1：左右各半宽，左侧切上下两行，右侧占满整列
        LayoutKind::Left2Right1 => vec![
            Rect::new(x, y, half_w, half_h),
            Rect::new(x, y + half_h, half_w, h - half_h),
            Rect::new(x + half_w, y, w - half_w, h),
        ],
        // 上2下1：上下各半高，上侧切左右两列，下侧占满整行
        LayoutKind::Top2Bottom1 => vec![
            Rect::new(x, y, half_w, half_h),
            Rect::new(x + half_w, y, w - half_w, half_h),
            Rect::new(x, y + half_h, w, h - half_h),
        ],
        // 上3下3：上下各半，每半三等列；余数补到第三列，保证铺满无缝
        LayoutKind::Top3Bottom3 => {
            let top_h = half_h;
            let bottom_h = h - half_h;
            let c0 = third_w;
            let c1 = third_w;
            let c2 = w - 2 * third_w;
            vec![
                Rect::new(x, y, c0, top_h),
                Rect::new(x + c0, y, c1, top_h),
                Rect::new(x + c0 + c1, y, c2, top_h),
                Rect::new(x, y + top_h, c0, bottom_h),
                Rect::new(x + c0, y + top_h, c1, bottom_h),
                Rect::new(x + c0 + c1, y + top_h, c2, bottom_h),
            ]
        }
        LayoutKind::CenterFloat => {
            // 不能用绝对下限 max(400)/max(300)：小屏、竖屏或分屏后的可用区
            // 可能连 400 宽都没有，会把浮动窗口顶出工作区。先按可用区比例，
            // 再夹到可用区内（四周至少留 1px，避免盖住 dock）。
            let max_w = (w - 2).max(1);
            let max_h = (h - 2).max(1);
            let fw = (w * 2 / 3).min(1100).clamp(1, max_w);
            let fh = (h * 2 / 3).min(750).clamp(1, max_h);
            vec![Rect::new(
                x + (w - fw) / 2,
                y + (h - fh) / 2,
                fw,
                fh,
            )]
        }
    }
}

/// 槽位切分方向。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

/// 布局槽位的二叉树。Split 沿 axis 以 first_frac 切分，叶子序号=目标窗口序号。
/// 用树而不是"按窗口数均分"，才能在窗口数变化时仍然保持预设布局的形状
/// （左1右2 仍是左侧一栏+右侧两行，四分网格仍是 2×2，而不是变成 N 条横带）。
enum SlotTree {
    Leaf(usize),
    Split {
        axis: Axis,
        first_frac: f64,
        first: Box<SlotTree>,
        second: Box<SlotTree>,
    },
}

/// 沿同一方向把 count 个叶子顺序均分。
fn chain(axis: Axis, count: usize, start: usize) -> SlotTree {
    if count <= 1 {
        return SlotTree::Leaf(start);
    }
    SlotTree::Split {
        axis,
        first_frac: 1.0 / count as f64,
        first: Box::new(SlotTree::Leaf(start)),
        second: Box::new(chain(axis, count - 1, start + 1)),
    }
}

/// 根据实际窗口数构造保留布局特征的槽位树。
fn build_tree(kind: LayoutKind, count: usize) -> SlotTree {
    // 四分网格最多 4 格、上3下3 最多 6 格，多余窗口在 apply 里收起。
    let cap = match kind {
        LayoutKind::Quarter => 4,
        LayoutKind::Top3Bottom3 => 6,
        _ => usize::MAX,
    };
    let count = count.min(cap).max(1);

    match kind {
        LayoutKind::CenterFloat => SlotTree::Leaf(0),
        LayoutKind::LeftRight => chain(Axis::X, count, 0),
        LayoutKind::TopBottom => chain(Axis::Y, count, 0),
        LayoutKind::Quarter => match count {
            1 => SlotTree::Leaf(0),
            2 => chain(Axis::X, 2, 0),
            3 => SlotTree::Split {
                axis: Axis::X,
                first_frac: 0.5,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(chain(Axis::Y, 2, 1)),
            },
            // 4 格时按行优先：左上、右上、左下、右下，和原 slots_for 顺序一致。
            _ => SlotTree::Split {
                axis: Axis::Y,
                first_frac: 0.5,
                first: Box::new(chain(Axis::X, 2, 0)),
                second: Box::new(chain(Axis::X, 2, 2)),
            },
        },
        LayoutKind::Left1Right2 => match count {
            1 => SlotTree::Leaf(0),
            2 => SlotTree::Split {
                axis: Axis::X,
                first_frac: 1.0 / 3.0,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(SlotTree::Leaf(1)),
            },
            3 => SlotTree::Split {
                axis: Axis::X,
                first_frac: 1.0 / 3.0,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(chain(Axis::Y, 2, 1)),
            },
            _ => SlotTree::Split {
                axis: Axis::X,
                first_frac: 1.0 / 3.0,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(chain(Axis::Y, count - 1, 1)),
            },
        },
        // 上1下2：上排半高占满宽，下排半高切左右两列
        LayoutKind::Top1Bottom2 => match count {
            1 => SlotTree::Leaf(0),
            2 => SlotTree::Split {
                axis: Axis::Y,
                first_frac: 0.5,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(SlotTree::Leaf(1)),
            },
            3 => SlotTree::Split {
                axis: Axis::Y,
                first_frac: 0.5,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(chain(Axis::X, 2, 1)),
            },
            _ => SlotTree::Split {
                axis: Axis::Y,
                first_frac: 0.5,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(chain(Axis::X, count - 1, 1)),
            },
        },
        // 左2右1：左右各半宽，左侧占两行，右侧整列。
        LayoutKind::Left2Right1 => match count {
            1 => SlotTree::Leaf(0),
            2 => SlotTree::Split {
                axis: Axis::X,
                first_frac: 0.5,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(SlotTree::Leaf(1)),
            },
            _ => SlotTree::Split {
                axis: Axis::X,
                first_frac: 0.5,
                first: Box::new(chain(Axis::Y, 2, 0)),
                second: Box::new(chain(Axis::Y, count - 2, 2)),
            },
        },
        // 上2下1：上下各半高，上侧占两列，下侧整行。
        LayoutKind::Top2Bottom1 => match count {
            1 => SlotTree::Leaf(0),
            2 => SlotTree::Split {
                axis: Axis::Y,
                first_frac: 0.5,
                first: Box::new(SlotTree::Leaf(0)),
                second: Box::new(SlotTree::Leaf(1)),
            },
            _ => SlotTree::Split {
                axis: Axis::Y,
                first_frac: 0.5,
                first: Box::new(chain(Axis::X, 2, 0)),
                second: Box::new(chain(Axis::X, count - 2, 2)),
            },
        },
        // 上3下3：每行最多 3 格，先填满上排再填下排；最多 6 格（入口已限制）。
        LayoutKind::Top3Bottom3 => {
            let top = count.min(3);
            let bottom = count.saturating_sub(3);
            if bottom == 0 {
                chain(Axis::X, top, 0)
            } else {
                SlotTree::Split {
                    axis: Axis::Y,
                    first_frac: 0.5,
                    first: Box::new(chain(Axis::X, 3, 0)),
                    second: Box::new(chain(Axis::X, bottom, 3)),
                }
            }
        }
    }
}

/// 子树在某个方向上的 (最小, 最大) 尺寸：
/// 沿切分方向相加，垂直方向 min 取最大、max 取最小。
fn extent(tree: &SlotTree, axis: Axis, limits: &[SizeLimit]) -> (i32, i32) {
    match tree {
        SlotTree::Leaf(i) => limits
            .get(*i)
            .map(|l| {
                if axis == Axis::X {
                    (l.min_w, l.max_w)
                } else {
                    (l.min_h, l.max_h)
                }
            })
            .unwrap_or((0, INF)),
        SlotTree::Split {
            axis: a,
            first,
            second,
            ..
        } => {
            let (fw, fmaxw) = extent(first, Axis::X, limits);
            let (fh, fmaxh) = extent(first, Axis::Y, limits);
            let (sw, smaxw) = extent(second, Axis::X, limits);
            let (sh, smaxh) = extent(second, Axis::Y, limits);
            if axis == *a {
                (sat_add(fw, sw), sat_add(fmaxw, smaxw))
            } else if axis == Axis::X {
                // 垂直切分时两块共用同一宽度：下限取最大保证都放得下，
                // 上限也取最大——固定尺寸窗口可以不满格，可伸缩的邻居照样能吃满。
                (fw.max(sw), fmaxw.max(smaxw))
            } else {
                (fh.max(sh), fmaxh.max(smaxh))
            }
        }
    }
}

/// 把槽位树落到具体几何。切分点先按预设比例，再被每棵子树的
/// [min, max] 可行区间夹住：固定尺寸窗口只拿自己用得上的那块，
/// 剩下的空间自然留给可伸缩的邻居。
fn emit_slots(tree: &SlotTree, area: Rect, limits: &[SizeLimit], out: &mut Vec<Rect>) {
    match tree {
        SlotTree::Leaf(i) => {
            let l = limits.get(*i).copied().unwrap_or_else(SizeLimit::free);
            // 固定尺寸窗口拉不满整个格子时，把目标矩形收成它真实能占的面积；
            // 其余空间留白，但不会被误报成"窗口超标"。
            out.push(Rect::new(
                area.x,
                area.y,
                area.width.min(l.max_w).max(1),
                area.height.min(l.max_h).max(1),
            ));
        }
        SlotTree::Split {
            axis,
            first_frac,
            first,
            second,
        } => {
            let total = if *axis == Axis::X {
                area.width
            } else {
                area.height
            };
            let (m1, max1) = extent(first, *axis, limits);
            let (m2, max2) = extent(second, *axis, limits);
            // a 既要 >= 第一棵子树下限、又不能挤掉第二棵子树下限；
            // 同时第一棵子树吃不完(max1)、第二棵子树最多吃 max2。
            let lo = m1.max(total.saturating_sub(max2));
            let hi = max1.min(total.saturating_sub(m2));
            let desired = (total as f64 * first_frac).round() as i32;
            let a = if lo <= hi {
                desired.clamp(lo, hi)
            } else {
                // 约束本身无解（例如两个固定窗口加起来超过屏幕）：
                // 优先保住下限，避免窗口互相压住；溢出交给上层提示。
                let mut v = m1.clamp(1, total.saturating_sub(1).max(1));
                if max1 < INF {
                    v = v.min(max1).max(1);
                }
                v
            };
            // 约束无解时（小屏 + 多个带下限的窗口）a 可能等于 total，
            // 第二块必须保持正尺寸，否则后面的 Rect 会带负宽/负高，
            // apply_geometry 再把它顶成固定 160x100，整排窗口就叠在一起。
            let b = total.saturating_sub(a).max(1);
            let (r1, r2) = if *axis == Axis::X {
                (
                    Rect::new(area.x, area.y, a, area.height),
                    Rect::new(area.x + a, area.y, b, area.height),
                )
            } else {
                (
                    Rect::new(area.x, area.y, area.width, a),
                    Rect::new(area.x, area.y + a, area.width, b),
                )
            };
            emit_slots(first, r1, limits, out);
            emit_slots(second, r2, limits, out);
        }
    }
}

/// 按布局形状与窗口数生成固定网格。
///
/// 网格只由布局类型和窗口数决定，**不看窗口自身的尺寸约束**：
/// 先按预设比例切好每一格，再把每个窗口按自己的 min/max 落进格子里。
/// 这样小窗不会把邻居"挤"变形，撑破格子的大窗也不会改写别人的边界。
pub fn grid_slots(kind: LayoutKind, area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    if kind == LayoutKind::CenterFloat {
        return slots_for(kind, area);
    }
    // 全 free 约束下 emit_slots 退化为纯比例切分（extent 的 min=0/max=INF，
    // 切分点就是 first_frac），不再因任何窗口的尺寸而挪动格线。
    let free = vec![SizeLimit::free(); count];
    let mut out = Vec::with_capacity(count);
    let tree = build_tree(kind, count);
    emit_slots(&tree, area, &free, &mut out);
    out
}

/// 兼容旧调用方（诊断 CLI）：只用 limits 的长度决定网格数。
pub fn reflow_slots(kind: LayoutKind, area: Rect, limits: &[SizeLimit]) -> Vec<Rect> {
    grid_slots(kind, area, limits.len())
}

/// 把一个窗口按自身尺寸约束落进固定格子，返回传给 apply_geometry 的外框几何。
///
/// 逐轴独立判定三条规则：
///   1. 格子放得下、窗口也拉得动（min ≤ 格子 ≤ max）→ 填满整格；
///   2. 窗口最小尺寸 > 格子 → 用最小尺寸，从格子左上角起浮窗显示，
///      允许溢出格子、允许和邻居堆叠（不居中，否则会压到更多邻居）；
///   3. 窗口上限 < 格子（含 min==max 的固定尺寸窗口）→ 保持该尺寸，在格子里居中。
///
/// `frame` 在设置启用边框补偿时为窗口真实边框，否则传全 0（此时 rect 即客户端几何）。
pub fn fit_slot(slot: Rect, limit: SizeLimit, frame: FrameExtents) -> Rect {
    let avail_x = (slot.width - frame.left - frame.right).max(1);
    let avail_y = (slot.height - frame.top - frame.bottom).max(1);

    // 返回 (目标尺寸, 客户端起点坐标)
    let fit_axis = |avail: i32, min: i32, max: i32, origin: i32| -> (i32, i32) {
        // min>max 的异常 hint 以 min 为准，避免 clamp  panic / 尺寸为 0
        let max = max.max(min);
        if min > avail {
            // 规则 2：撑破格子，左上角浮窗（允许溢出与堆叠）
            (min, origin)
        } else if max < avail {
            // 规则 3：小窗/固定尺寸，居中
            (max, origin + (avail - max) / 2)
        } else {
            // 规则 1：填满
            (avail, origin)
        }
    };

    let (w, client_x) = fit_axis(avail_x, limit.min_w, limit.max_w, slot.x + frame.left);
    let (h, client_y) = fit_axis(avail_y, limit.min_h, limit.max_h, slot.y + frame.top);

    Rect::new(
        client_x - frame.left,
        client_y - frame.top,
        w + frame.left + frame.right,
        h + frame.top + frame.bottom,
    )
}

/// 执行布局。返回 (成功数, 收起数)。
///
/// 所有窗口都参与布局：网格按布局形状与窗口数固定划分，每个窗口再按自身
/// 尺寸约束（`fit_slot`）落位——自由窗口填满格子，撑破格子的用最小尺寸
/// 左上角浮窗（允许堆叠），小窗/固定尺寸格内居中。固定尺寸窗口不再被最小化。
///
/// 多显示器/多工作区：每个窗口按自己的中心点归属到对应可用区，
/// 各组用**自己那块可用空间**独立布局。旧实现只取第一个窗口的区，
/// 会把另一块屏幕上的窗口硬塞进主屏，小屏上尤其明显。
pub fn apply(
    x11: &X11,
    kind: LayoutKind,
    windows: &[WindowInfo],
    focus_id: Option<&str>,
    areas: &[Rect],
    settings: &Settings,
) -> Result<(usize, usize)> {
    if windows.is_empty() || areas.is_empty() {
        return Ok((0, 0));
    }

    let mut groups: Vec<(usize, Vec<&WindowInfo>)> = Vec::new();
    for w in windows {
        let idx = w.area_index(areas);
        if let Some(g) = groups.iter_mut().find(|g| g.0 == idx) {
            g.1.push(w);
        } else {
            groups.push((idx, vec![w]));
        }
    }
    // 快捷键要求焦点窗口优先，把焦点所在组排到最前处理。
    if let Some(fid) = focus_id {
        if let Some(i) = groups
            .iter()
            .position(|(_, ws)| ws.iter().any(|w| w.id == fid))
        {
            let g = groups.remove(i);
            groups.insert(0, g);
        }
    }

    let mut applied_total = 0;
    let mut minimized_total = 0;
    for (idx, ws) in groups {
        let group: Vec<WindowInfo> = ws.iter().map(|w| (**w).clone()).collect();
        let area = areas.get(idx).copied().unwrap_or(areas[0]);
        let (a, m) = apply_in_area(x11, kind, &group, focus_id, area, settings)?;
        applied_total += a;
        minimized_total += m;
    }

    // 收起所有「没参与本次布局」的可见窗口：否则它们会浮在刚排好的网格上，
    // 视觉上表现为"各种排列都不严丝合缝"。放在全部落位之后再收，避免
    // 一次多个收起动画让 kwin 忙不过来、把紧随其后的几何请求丢掉。
    // 居中浮动的既有语义是"其它窗口留原位"，所以那种布局不动它们。
    if settings.minimize_extra_windows && kind != LayoutKind::CenterFloat {
        let chosen: std::collections::HashSet<&str> =
            windows.iter().map(|w| w.id.as_str()).collect();
        if let Ok(all) = x11.managed_windows(settings) {
            for w in all {
                if w.hidden || chosen.contains(w.id.as_str()) {
                    continue;
                }
                match x11.minimize(&w) {
                    Ok(()) => minimized_total += 1,
                    Err(e) => eprintln!("  [min-extra] {} 收起失败: {}", w.id, e),
                }
            }
        }
    }

    Ok((applied_total, minimized_total))
}

/// 在单块可用区内执行布局。返回 (成功数, 收起数)。
///
/// `focus_id=Some(..)` 时把焦点窗口放到第一个槽位（快捷键的语义）；
/// `focus_id=None` 时严格保留传入 `windows` 的顺序（面板里用户勾选的卡片序号）。
fn apply_in_area(
    x11: &X11,
    kind: LayoutKind,
    windows: &[WindowInfo],
    focus_id: Option<&str>,
    area: Rect,
    settings: &Settings,
) -> Result<(usize, usize)> {
    if windows.is_empty() {
        return Ok((0, 0));
    }

    // 目标窗口：只有快捷键才把焦点窗口提到第一位。
    // 面板 apply 传 None：用户勾选的卡片序号必须与预览里的 1/2/3/4 一一对应。
    let mut targets: Vec<&WindowInfo> = Vec::new();
    if let Some(fid) = focus_id {
        if let Some(w) = windows.iter().find(|w| w.id == fid) {
            targets.push(w);
        }
    }
    for w in windows {
        if !targets.iter().any(|t| t.id == w.id) {
            targets.push(w);
        }
    }

    // 网格只由布局形状 + 本次勾选的窗口数决定，绝不受窗口 min/max 影响。
    // 0.5.26 曾改成「约束自适应」，切分点会被 min/max 夹走：窗口刚打开时
    // WM_NORMAL_HINTS 还没稳定（浏览器尤其常见），第一次排出来的格线会和
    // 第二次不一样，用户看到的就是「偶尔某个窗口没严丝合缝，再点一次才好」。
    // 0.5.50 回到纯几何网格；约束只在下面 fit_slot 里决定单个窗口怎么落格。
    let slots = grid_slots(kind, area, targets.len());
    let target_count = targets.len().min(slots.len());
    let mut minimized = 0;

    // 边框补偿开关：和 x11.apply_geometry 内部保持一致，
    // 否则 fit_slot 算出的外框会被 apply_geometry 二次减边框。
    let frame_of = |w: &WindowInfo| {
        if settings.frame_compensation {
            w.frame
        } else {
            FrameExtents::default()
        }
    };

    // 先排布要显示的窗口，多余窗口留到最后再收起。
    // 反过来做的话，一次 5 个收起动画会让 KWin 忙不过来，把紧随其后的几何请求全丢掉。
    let mut applied = 0;
    let mut fitted_rects: Vec<(String, Rect)> = Vec::new();
    // 从最小化恢复的窗口：刚 map 出来时 _GTK_FRAME_EXTENTS 还在变化，
    // 回读阶段的 correct_geometry 会用不稳定的边框把它二次补偿到屏幕外。
    // 这些窗口由 apply_geometry 已正确落位，回读纠正阶段直接跳过。
    let mut restored_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (win, slot) in targets.iter().zip(slots.iter()).take(target_count) {
        // 最小化的窗口先放回屏幕再摆位。
        // 关键：restore() 会改变窗口的实际 frame / _GTK_FRAME_EXTENTS，
        // 最小化时读到的 win.frame 往往是旧值（例如 CSD 窗口的装饰外扩），
        // 继续用它做边框补偿会让窗口偏到屏幕外（实测 Browser 落到 -16,-10）。
        // 因此恢复后必须按同一 ID 重新读取窗口信息，用最新几何补偿。
        let live: WindowInfo = if win.hidden {
            if let Err(e) = x11.restore(win) {
                eprintln!("  [restore] {} 恢复失败: {}", win.id, e);
            }
            restored_ids.insert(normalize_id(&win.id));
            x11.window_by_id(&win.id).unwrap_or_else(|_| (*win).clone())
        } else {
            (*win).clone()
        };
        // 约束也按恢复后的窗口重算：CSD 窗口最小化时的 hint 可能不准。
        let live_limit = SizeLimit::new(
            x11.min_size_of(&live.id).unwrap_or((0, 0)),
            x11.max_size_of(&live.id),
        );
        let goal = fit_slot(*slot, live_limit, frame_of(&live));
        crate::vlog!(
            "落位 {} {}：格={:?} 约束={:?} -> 目标={:?}",
            live.id, live.title, *slot, live_limit, goal
        );
        if x11.apply_geometry(&live, goal, settings).is_ok() {
            applied += 1;
        }
        fitted_rects.push((live.id.clone(), goal));
    }

    // 多余窗口收起（已经收着的直接跳过，别重复发请求）
    if settings.minimize_extra_windows && slots.len() > 1 && targets.len() > slots.len() {
        for w in targets.iter().skip(slots.len()) {
            if w.hidden {
                continue;
            }
            match x11.minimize(w) {
                Ok(()) => minimized += 1,
                Err(e) => eprintln!("  [min] {} 收起失败: {}", w.id, e),
            }
        }
    }

    // 等待 WM 处理几何变更，再回读纠正位置偏差。
    // 这里只把窗口拉回 fit_slot 算出的目标，不再"实测下限->水填重排"：
    // 网格已定死，撑破的窗口就按 min 浮窗，小窗就居中，邻居不让它挪格线。
    thread::sleep(Duration::from_millis(120));
    if let Ok(refreshed) = x11.managed_windows(settings) {
        for (id, goal) in &fitted_rects {
            if let Some(cur) = refreshed
                .iter()
                .find(|w| normalize_id(&w.id) == normalize_id(id))
            {
                // 刚从最小化恢复的 CSD 窗口不参与回读纠正，避免二次补偿跑到屏幕外。
                if !restored_ids.contains(&normalize_id(id)) {
                    let _ = x11.correct_geometry(cur, *goal, settings);
                }
                let expected = x11.expected_client_rect(cur, *goal, settings);
                let actual = x11.actual_rect(cur).unwrap_or(cur.rect);
                crate::vlog!(
                    "落位回读 {} {}：目标客户端={:?} 实际={:?}{}",
                    cur.id, cur.title, expected, actual,
                    if restored_ids.contains(&normalize_id(id)) { " (跳过纠正)" } else { "" }
                );
            }
        }
    }

    Ok((applied, minimized))
}

pub fn normalize_id(id: &str) -> String {
    let hex = id.trim_start_matches("0x").trim_start_matches('0');
    format!("0x{}", if hex.is_empty() { "0" } else { hex })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(100, 200, 1800, 900)
    }

    fn free(n: usize) -> Vec<SizeLimit> {
        vec![SizeLimit::free(); n]
    }

    #[test]
    fn preset_counts_match_original_slots() {
        let a = area();
        // 用 all() 自动覆盖全部布局，新增布局不会漏测。
        for kind in LayoutKind::all() {
            let n = slots_for(kind, a).len();
            assert_eq!(reflow_slots(kind, a, &free(n)), slots_for(kind, a), "{:?}", kind);
        }
    }

    #[test]
    fn new_layouts_preset_geometry() {
        let a = area(); // x=100 y=200 w=1800 h=900 -> third_w=600 half_w=900 half_h=450 third_h=300
        // 左2右1：左右各 900 宽，左侧切上下两行，右侧整列
        assert_eq!(
            grid_slots(LayoutKind::Left2Right1, a, 3),
            vec![
                Rect::new(100, 200, 900, 450),
                Rect::new(100, 650, 900, 450),
                Rect::new(1000, 200, 900, 900),
            ]
        );
        // 上2下1：上下各 450 高，上侧切左右两列，下侧整行
        assert_eq!(
            grid_slots(LayoutKind::Top2Bottom1, a, 3),
            vec![
                Rect::new(100, 200, 900, 450),
                Rect::new(1000, 200, 900, 450),
                Rect::new(100, 650, 1800, 450),
            ]
        );
        // 上3下3：上下各 450，每行三列各 600
        assert_eq!(
            grid_slots(LayoutKind::Top3Bottom3, a, 6),
            vec![
                Rect::new(100, 200, 600, 450), Rect::new(700, 200, 600, 450), Rect::new(1300, 200, 600, 450),
                Rect::new(100, 650, 600, 450), Rect::new(700, 650, 600, 450), Rect::new(1300, 650, 600, 450),
            ]
        );
    }

    #[test]
    fn new_layouts_scale_and_cap() {
        let a = area();
        // 两窗时对半布局退化为等分两块
        assert_eq!(
            grid_slots(LayoutKind::Left2Right1, a, 2),
            vec![Rect::new(100, 200, 900, 900), Rect::new(1000, 200, 900, 900)]
        );
        assert_eq!(
            grid_slots(LayoutKind::Top2Bottom1, a, 2),
            vec![Rect::new(100, 200, 1800, 450), Rect::new(100, 650, 1800, 450)]
        );
        // 上3下3 按行填充：3 窗一行三列；4 窗上三下一；6 窗满；超过 6 仍只给 6 格
        assert_eq!(grid_slots(LayoutKind::Top3Bottom3, a, 3).len(), 3);
        assert_eq!(grid_slots(LayoutKind::Top3Bottom3, a, 4).len(), 4);
        assert_eq!(grid_slots(LayoutKind::Top3Bottom3, a, 9).len(), 6);
        let four = grid_slots(LayoutKind::Top3Bottom3, a, 4);
        assert_eq!(four[3], Rect::new(100, 650, 1800, 450));
    }

    #[test]
    fn left1_right2_keeps_shape_for_two_and_three() {
        let a = area();
        let two = reflow_slots(LayoutKind::Left1Right2, a, &free(2));
        assert_eq!(two[0], Rect::new(100, 200, 600, 900));
        assert_eq!(two[1], Rect::new(700, 200, 1200, 900));
        let three = reflow_slots(LayoutKind::Left1Right2, a, &free(3));
        assert_eq!(three, slots_for(LayoutKind::Left1Right2, a));
    }

    #[test]
    fn top1_bottom2_keeps_shape_for_two_and_three() {
        let a = area();
        let two = reflow_slots(LayoutKind::Top1Bottom2, a, &free(2));
        assert_eq!(two[0], Rect::new(100, 200, 1800, 450));
        assert_eq!(two[1], Rect::new(100, 650, 1800, 450));
        let three = reflow_slots(LayoutKind::Top1Bottom2, a, &free(3));
        assert_eq!(three, slots_for(LayoutKind::Top1Bottom2, a));
    }

    #[test]
    fn quarter_keeps_grid_shape() {
        let a = area();
        let two = reflow_slots(LayoutKind::Quarter, a, &free(2));
        assert_eq!(two[0], Rect::new(100, 200, 900, 900));
        assert_eq!(two[1], Rect::new(1000, 200, 900, 900));
        let three = reflow_slots(LayoutKind::Quarter, a, &free(3));
        assert_eq!(three[0], Rect::new(100, 200, 900, 900));
        assert_eq!(three[1], Rect::new(1000, 200, 900, 450));
        assert_eq!(three[2], Rect::new(1000, 650, 900, 450));
    }

    #[test]
    fn split_layouts_scale_to_window_count() {
        let a = area();
        let three = reflow_slots(LayoutKind::LeftRight, a, &free(3));
        assert_eq!(three[0], Rect::new(100, 200, 600, 900));
        assert_eq!(three[1], Rect::new(700, 200, 600, 900));
        assert_eq!(three[2], Rect::new(1300, 200, 600, 900));
        let three = reflow_slots(LayoutKind::TopBottom, a, &free(3));
        assert_eq!(three[0], Rect::new(100, 200, 1800, 300));
        assert_eq!(three[1], Rect::new(100, 500, 1800, 300));
        assert_eq!(three[2], Rect::new(100, 800, 1800, 300));
    }

    #[test]
    fn grid_ignores_window_limits() {
        // 网格只由布局形状+窗口数决定；无论窗口有 min/max 还是固定尺寸，
        // 格线都不移动。旧水填会为这些约束挪格线，新规则禁止。
        let a = area();
        let constrained = vec![
            SizeLimit::new((900, 0), Some((1200, INF))),
            SizeLimit::new((280, 380), Some((280, 380))),
            SizeLimit::new((800, 600), None),
        ];
        assert_eq!(grid_slots(LayoutKind::Left1Right2, a, 3), slots_for(LayoutKind::Left1Right2, a));
        assert_eq!(reflow_slots(LayoutKind::Left1Right2, a, &constrained), slots_for(LayoutKind::Left1Right2, a));

        let q = Rect::new(100, 200, 1800, 1030);
        // 3 窗时四分布局是「左侧一整列 + 右侧上下两格」，和 Python 预览一致
        assert_eq!(grid_slots(LayoutKind::Quarter, q, 3), vec![
            Rect::new(100, 200, 900, 1030),
            Rect::new(1000, 200, 900, 515),
            Rect::new(1000, 715, 900, 515),
        ]);
        assert_eq!(reflow_slots(LayoutKind::Quarter, q, &constrained).len(), 3);
    }

    #[test]
    fn fit_slot_free_window_fills_grid() {
        // 规则 1：没有实际约束（min=0/max=INF）→ 填满整格。
        let slot = Rect::new(100, 200, 900, 600);
        assert_eq!(fit_slot(slot, SizeLimit::free(), FrameExtents::default()), slot);
    }

    #[test]
    fn fit_slot_min_larger_than_slot_floats_at_top_left() {
        // 规则 2：最小尺寸超格子 → 用 min 尺寸、左上角对齐，允许溢出压邻居。
        let slot = Rect::new(100, 200, 400, 300);
        let limit = SizeLimit::new((800, 600), None);
        assert_eq!(
            fit_slot(slot, limit, FrameExtents::default()),
            Rect::new(100, 200, 800, 600)
        );
    }

    #[test]
    fn fit_slot_small_window_centers_in_grid() {
        // 规则 3：窗口小、拉不满格子（min==max 固定尺寸）→ 保持尺寸、格内居中。
        let slot = Rect::new(100, 200, 900, 600);
        let limit = SizeLimit::new((280, 380), Some((280, 380)));
        assert_eq!(
            fit_slot(slot, limit, FrameExtents::default()),
            Rect::new(100 + (900 - 280) / 2, 200 + (600 - 380) / 2, 280, 380)
        );
    }

    #[test]
    fn fit_slot_max_cap_centers_in_grid() {
        // 规则 3 的另一种：没有 min 但有 max 上限，格子更大 → 按 max 居中。
        let slot = Rect::new(0, 0, 1000, 800);
        let limit = SizeLimit::new((0, 0), Some((600, 400)));
        assert_eq!(
            fit_slot(slot, limit, FrameExtents::default()),
            Rect::new((1000 - 600) / 2, (800 - 400) / 2, 600, 400)
        );
    }

    #[test]
    fn fit_slot_mixed_axes() {
        // 宽被 min 撑破（左上角）、高被 max 卡住（居中）：两轴独立判定。
        let slot = Rect::new(10, 20, 500, 500);
        let limit = SizeLimit::new((800, 0), Some((INF, 200)));
        assert_eq!(
            fit_slot(slot, limit, FrameExtents::default()),
            Rect::new(10, 20 + (500 - 200) / 2, 800, 200)
        );
    }

    #[test]
    fn fit_slot_honors_frame_compensation() {
        // 有边框时可用区要减去边框，居中/撑破都基于客户端可用区。
        let slot = Rect::new(100, 200, 900, 600);
        let frame = FrameExtents { left: 1, right: 1, top: 30, bottom: 1 };
        let free = fit_slot(slot, SizeLimit::free(), frame);
        // 自由窗口填满：外框应该回到 slot 本身
        assert_eq!(free, slot);
        // 固定小窗在客户端可用区内居中；280x380 是客户端尺寸，
        // 外框要加回左右/上下边框（+2 宽、+31 高），起点 = slot+left+偏移-left
        let fixed = SizeLimit::new((280, 380), Some((280, 380)));
        assert_eq!(
            fit_slot(slot, fixed, frame),
            Rect::new(100 + (898 - 280) / 2, 200 + (568 - 380) / 2, 280 + 2, 380 + 31)
        );
    }

    #[test]
    fn every_preset_fits_any_workarea_size() {
        // 不绑定任何一台机器的分辨率：小屏、竖屏、常见桌面、超宽屏都验证。
        let sizes = [
            (640, 480), (800, 600), (1024, 768), (1366, 768),
            (1920, 1080), (2560, 1440), (3440, 1440),
        ];
        let kinds = LayoutKind::all();
        for (w, h) in sizes {
            let a = Rect::new(0, 0, w, h);
            for kind in kinds {
                // 1..=6 覆盖上3下3 的 5/6 格；更多窗口会被封顶到 6 格
                for n in 1..=6 {
                    let slots = if kind == LayoutKind::CenterFloat {
                        slots_for(kind, a)
                    } else {
                        reflow_slots(kind, a, &free(n))
                    };
                    assert!(!slots.is_empty(), "{:?} {}x{}", kind, w, h);
                    for r in &slots {
                        assert!(r.width >= 1 && r.height >= 1, "{:?} {}x{} {:?}", kind, w, h, r);
                        assert!(r.x >= a.x && r.y >= a.y, "{:?} {}x{} {:?}", kind, w, h, r);
                        // 溢出最多 1px（约束无解时末块保底），不能整块跑出工作区。
                        assert!(
                            r.x + r.width <= a.x + a.width + 1
                                && r.y + r.height <= a.y + a.height + 1,
                            "{:?} {}x{} {:?}", kind, w, h, r
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn narrow_workarea_keeps_slots_contiguous_without_fixed_floor() {
        // 800 宽排 8 个窗口，每格 100px，小于旧 apply_geometry 的硬编码 160px。
        // 布局层必须给出连续、正尺寸的槽位，不能靠把窗口顶大来"填满"。
        let a = Rect::new(0, 0, 800, 600);
        let slots = reflow_slots(LayoutKind::LeftRight, a, &free(8));
        assert_eq!(slots.len(), 8);
        for (i, r) in slots.iter().enumerate() {
            assert!(r.width >= 1 && r.height == 600);
            if i > 0 {
                assert_eq!(r.x, slots[i - 1].x + slots[i - 1].width);
            }
        }
        let last = slots.last().unwrap();
        assert_eq!(last.x + last.width, a.x + a.width);
    }

}
