//! 数据模型：矩形、窗口信息、快照记录。

use serde::{Deserialize, Serialize};

/// 屏幕矩形（x, y, width, height）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// 窗口边框（来自 _NET_FRAME_EXTENTS）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct FrameExtents {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

impl FrameExtents {
    pub fn is_zero(&self) -> bool {
        self.left == 0 && self.right == 0 && self.top == 0 && self.bottom == 0
    }
}

/// X11 窗口信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    /// 窗口 ID（十六进制字符串，如 "0x9c00004"）。
    pub id: String,
    pub desktop: i32,
    pub pid: i32,
    /// 客户端几何（wmctrl 报告的 x/y/w/h）。
    pub rect: Rect,
    pub class: String,
    pub title: String,
    pub frame: FrameExtents,
    /// CSD 窗口（Chromium/Electron 等）报告的透明阴影边，来自
    /// `_GTK_FRAME_EXTENTS`(left,right,top,bottom)。这类窗口的「窗口几何」比
    /// 可见内容大一圈：可见内容 = 几何 − gtk_frame。要让可见内容严丝合缝落进
    /// 格子，必须把外框放大 gtk_frame 并把原点反向偏移。普通窗口为全 0。
    pub gtk_frame: FrameExtents,
    pub maximized_vert: bool,
    pub maximized_horz: bool,
    pub fullscreen: bool,
    pub hidden: bool,
}

impl WindowInfo {
    /// 用户可见的内容矩形。
    ///
    /// 普通窗口＝外框（客户端几何 + 服务端边框）；CSD 窗口（无边框但带
    /// `_GTK_FRAME_EXTENTS` 透明阴影）＝窗口几何减去透明边。面板显示、快照
    /// 存取、工作区归属都用这个，保证「看到的」与「存下来的」一致。
    pub fn frame_rect(&self) -> Rect {
        let mut r = Rect::new(
            self.rect.x - self.frame.left,
            self.rect.y - self.frame.top,
            self.rect.width + self.frame.left + self.frame.right,
            self.rect.height + self.frame.top + self.frame.bottom,
        );
        if !self.gtk_frame.is_zero() {
            r = Rect::new(
                r.x + self.gtk_frame.left,
                r.y + self.gtk_frame.top,
                r.width - self.gtk_frame.left - self.gtk_frame.right,
                r.height - self.gtk_frame.top - self.gtk_frame.bottom,
            );
        }
        r
    }

    /// 按窗口中心点选择所属工作区。
    pub fn area_index(&self, areas: &[Rect]) -> usize {
        let (cx, cy) = self.frame_rect().center();
        for (i, area) in areas.iter().enumerate() {
            if area.contains(cx, cy) {
                return i;
            }
        }
        // 中心落在所有可用区之外（跨屏拖动、副屏位于主屏负坐标等）时，
        // 选中心距离最近的一块，而不是无条件回主屏——否则多屏布局会把
        // 副屏窗口全部搬到主屏。
        areas
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| {
                let (ax, ay) = a.center();
                (cx - ax).abs().saturating_add((cy - ay).abs())
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
}

/// 旧快照没有 stack 字段时的默认值：未知层序，加载时不动当前层叠。
fn default_stack() -> i32 {
    -1
}

/// 快照中的单个窗口记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotWindow {
    pub id: String,
    pub class: String,
    pub title: String,
    /// 保存瞬间的进程号。窗口重启后 XID 会变、标题也可能变，PID+class 是
    /// 同一进程最稳的二次身份；旧快照没有这个字段，serde default 为 0。
    #[serde(default)]
    pub pid: i32,
    pub desktop: i32,
    /// 保存瞬间的可见几何（`frame_rect()`，普通窗口含装饰、CSD 窗口去透明边）。
    pub rect: Rect,
    /// 相对所属工作区的万分比几何，用于跨分辨率还原。
    pub rel: Rect,
    pub area_index: usize,
    pub maximized_vert: bool,
    pub maximized_horz: bool,
    pub fullscreen: bool,
    /// 保存瞬间窗口是否处于最小化状态。
    /// 旧配置没有这个字段，serde default 为 false（按以前的语义当可见处理）。
    #[serde(default)]
    pub hidden: bool,
    /// 保存瞬间在 EWMH `_NET_CLIENT_LIST_STACKING` 中的次序
    /// （0 = 最底，越大越靠上）。旧快照没有该字段，default = -1 表示未知，
    /// 还原时跳过层叠恢复，不改变用户当前层序。
    #[serde(default = "default_stack")]
    pub stack: i32,
}

/// 命名快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub windows: Vec<SnapshotWindow>,
    pub created_at: String,
}

/// 布局类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutKind {
    Quarter,
    TopBottom,
    LeftRight,
    Left1Right2,
    Top1Bottom2,
    Left2Right1,
    Top2Bottom1,
    Top3Bottom3,
    CenterFloat,
}

impl LayoutKind {
    /// 稳定英文标识，供窗口选择脚本绘制布局预览。
    pub fn slug(self) -> &'static str {
        match self {
            Self::Quarter => "quarter",
            Self::TopBottom => "top_bottom",
            Self::LeftRight => "left_right",
            Self::Left1Right2 => "left1_right2",
            Self::Top1Bottom2 => "top1_bottom2",
            Self::Left2Right1 => "left2_right1",
            Self::Top2Bottom1 => "top2_bottom1",
            Self::Top3Bottom3 => "top3_bottom3",
            Self::CenterFloat => "center_float",
        }
    }

    pub fn all() -> [Self; 9] {
        [
            Self::Quarter,
            Self::TopBottom,
            Self::LeftRight,
            Self::Left1Right2,
            Self::Top1Bottom2,
            Self::Left2Right1,
            Self::Top2Bottom1,
            Self::Top3Bottom3,
            Self::CenterFloat,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win_at(x: i32, y: i32) -> WindowInfo {
        WindowInfo {
            id: "0x1".into(),
            desktop: 0,
            pid: 0,
            rect: Rect::new(x, y, 100, 100),
            class: String::new(),
            title: String::new(),
            frame: FrameExtents::default(),
            gtk_frame: FrameExtents::default(),
            maximized_vert: false,
            maximized_horz: false,
            fullscreen: false,
            hidden: false,
        }
    }

    #[test]
    fn area_index_prefers_containing_area_then_nearest() {
        let areas = [Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1280, 800)];
        assert_eq!(win_at(500, 300).area_index(&areas), 0);
        assert_eq!(win_at(2300, 300).area_index(&areas), 1);
        // 中心落在所有工作区外时按最近中心归属，不能一律回主屏。
        assert_eq!(win_at(-200, 300).area_index(&areas), 0);
        assert_eq!(win_at(3300, 300).area_index(&areas), 1);
    }

    #[test]
    fn csd_frame_rect_is_visible_content() {
        // CSD 窗口：X 几何含 GTK 透明边，frame_rect 应回可见内容矩形。
        // 这样快照存的是可见区，restore 外扩透明边后能精确回到原几何。
        let mut w = win_at(960, 0);
        w.rect = Rect::new(960, 0, 960, 600);
        w.gtk_frame = FrameExtents { left: 16, right: 16, top: 10, bottom: 32 };
        assert_eq!(w.frame_rect(), Rect::new(976, 10, 928, 558));
        // 普通窗口仍按外框语义。
        let mut n = win_at(0, 0);
        n.rect = Rect::new(0, 10, 800, 600);
        n.frame = FrameExtents { left: 1, right: 1, top: 30, bottom: 0 };
        assert_eq!(n.frame_rect(), Rect::new(-1, -20, 802, 630));
    }

    #[test]
    fn old_snapshot_without_hidden_field_loads_as_visible() {
        let json = r#"{
            "id":"0x1","class":"demo","title":"Demo","desktop":0,
            "rect":{"x":0,"y":0,"width":100,"height":100},
            "rel":{"x":0,"y":0,"width":10000,"height":10000},
            "area_index":0,"maximized_vert":false,"maximized_horz":false,
            "fullscreen":false
        }"#;
        let rec: SnapshotWindow = serde_json::from_str(json).unwrap();
        assert!(!rec.hidden);
        assert_eq!(rec.stack, -1, "旧快照无层叠字段时应保持当前层序");
    }
}
