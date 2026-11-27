#!/usr/bin/env python3
"""窗口排列：左侧选布局 + 排列预览，右侧实时缩略图卡片选窗口。

用法: select_windows.py --layout=<slug> --theme=<light|dark> 'id|x|y|w|h|name' ...
输出: 第一行 "layout:<slug>"，随后每行一个勾选窗口 id（按排列顺序）；
      点「取消」或关闭窗口则退出码 1、无输出。
"""
import sys
import ctypes
import ctypes.util
import subprocess
import tkinter as tk
from PIL import Image, ImageDraw, ImageFont, ImageTk, ImageGrab

RESAMPLE = getattr(Image, "LANCZOS", getattr(Image, "ANTIALIAS", 3))


# ---------- 系统字体 ----------
def _fc(pattern, field):
    try:
        return subprocess.check_output(
            ["fc-match", "-f", "%{" + field + "}", pattern],
            text=True, timeout=2).strip()
    except Exception:
        return ""


FONT_FAMILY = _fc("sans-serif:lang=zh-cn", "family[0]") or "Sans"
FONT_FILE = _fc("sans-serif:lang=zh-cn", "file")
FONT_BOLD_FILE = _fc("sans-serif:lang=zh-cn:style=Bold", "file") or FONT_FILE


def tf(size, bold=False):
    return (FONT_FAMILY, size, "bold") if bold else (FONT_FAMILY, size)


_FONT_CACHE = {}


def pf(size, bold=False):
    key = (size, bold)
    if key not in _FONT_CACHE:
        path = FONT_BOLD_FILE if bold else FONT_FILE
        try:
            _FONT_CACHE[key] = ImageFont.truetype(path, size) if path else ImageFont.load_default()
        except Exception:
            _FONT_CACHE[key] = ImageFont.load_default()
    return _FONT_CACHE[key]


def fit_text(draw, text, font, max_width):
    if draw.textlength(text, font=font) <= max_width:
        return text
    out = text
    while out and draw.textlength(out + "…", font=font) > max_width:
        out = out[:-1]
    return out + "…"


# ---------- 主题配色（与 deepin 系统观感一致） ----------
THEMES = {
    "light": dict(bg="#f5f6f8", panel="#ffffff", text="#1f2329", sub="#646a73",
                  border="#dfe2e8", accent="#2d7ff9", soft="#eaf2ff", muted="#eceef1",
                  tile_shape="#cfd6e0", shadow="#c9cdd4", scroll="#a9b2be", on="#ffffff",
                  warn="#d93026", tile="#f7f9fc"),
    "dark": dict(bg="#1e2023", panel="#2b2e33", text="#e8eaed", sub="#9aa0a6",
                 border="#41454b", accent="#5b9bff", soft="#263b5c", muted="#3a3e44",
                 tile_shape="#545c66", shadow="#141619", scroll="#6b7280", on="#ffffff",
                 warn="#ff6b61", tile="#31343a"),
}

LAYOUTS = [
    ("quarter", "四分网格"), ("top_bottom", "上下对半"), ("left_right", "左右对半"),
    ("left1_right2", "左1右2"), ("top1_bottom2", "上1下2"), ("center_float", "居中浮动"),
]

SLOTS = {
    "quarter": [(0, 0, .5, .5), (.5, 0, .5, .5), (0, .5, .5, .5), (.5, .5, .5, .5)],
    "top_bottom": [(0, 0, 1, .5), (0, .5, 1, .5)],
    "left_right": [(0, 0, .5, 1), (.5, 0, .5, 1)],
    "left1_right2": [(0, 0, 1 / 3, 1), (1 / 3, 0, 2 / 3, .5), (1 / 3, .5, 2 / 3, .5)],
    "top1_bottom2": [(0, 0, 1, 1 / 3), (0, 1 / 3, .5, 2 / 3), (.5, 1 / 3, .5, 2 / 3)],
    "center_float": [(0.18, 0.14, 0.64, 0.72)],
}
LAYOUT_NAMES = dict(LAYOUTS)


def nslots(slug):
    return len(SLOTS.get(slug, SLOTS["quarter"]))


# ---------- 参数 ----------
layout_slug = "quarter"
theme_name = "light"
grid_rows, grid_cols = 4, 5  # 默认 4 行 5 列
sort_mode = "default"        # default / title
entries = []
for arg in sys.argv[1:]:
    if arg.startswith("--layout="):
        layout_slug = arg.split("=", 1)[1]
    elif arg.startswith("--theme="):
        theme_name = arg.split("=", 1)[1]
    elif arg.startswith("--rows="):
        try:
            grid_rows = max(1, min(10, int(arg.split("=", 1)[1])))
        except ValueError:
            pass
    elif arg.startswith("--cols="):
        try:
            grid_cols = max(1, min(10, int(arg.split("=", 1)[1])))
        except ValueError:
            pass
    elif arg.startswith("--sort="):
        sort_mode = arg.split("=", 1)[1]
    else:
        parts = arg.split("|", 5)
        if len(parts) != 6:
            continue
        wid, xs, ys, ws, hs, name = parts
        try:
            entries.append((wid, int(xs), int(ys), int(ws), int(hs), name))
        except ValueError:
            pass

T = THEMES.get(theme_name, THEMES["light"])
if layout_slug not in SLOTS:
    layout_slug = "quarter"
if not entries:
    sys.exit(0)


# ---------- XComposite：抓窗口自身像素，不受遮挡影响 ----------
class _XImage(ctypes.Structure):
    _fields_ = [
        ("width", ctypes.c_int), ("height", ctypes.c_int),
        ("xoffset", ctypes.c_int), ("format", ctypes.c_int),
        ("data", ctypes.c_void_p), ("byte_order", ctypes.c_int),
        ("bitmap_unit", ctypes.c_int), ("bitmap_bit_order", ctypes.c_int),
        ("bitmap_pad", ctypes.c_int), ("depth", ctypes.c_int),
        ("bytes_per_line", ctypes.c_int), ("bits_per_pixel", ctypes.c_int),
        ("red_mask", ctypes.c_ulong), ("green_mask", ctypes.c_ulong),
        ("blue_mask", ctypes.c_ulong), ("obdata", ctypes.c_void_p),
        ("f", ctypes.c_void_p),
    ]


_xcomp = None
_error_handler = None  # 必须保持引用，否则回调被 GC 后 X 错误会踩野指针


def _init_xcomp():
    global _xcomp, _error_handler
    if _xcomp is not None:
        return _xcomp
    try:
        x11 = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        xc = ctypes.CDLL(ctypes.util.find_library("Xcomposite") or "libXcomposite.so.1")
        CMP = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
        x11.XSetErrorHandler.argtypes = [CMP]
        _error_handler = CMP(lambda d, e: 0)
        x11.XSetErrorHandler(_error_handler)
        x11.XOpenDisplay.restype = ctypes.c_void_p
        x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        x11.XGetImage.restype = ctypes.POINTER(_XImage)
        x11.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                                  ctypes.c_int, ctypes.c_uint, ctypes.c_uint,
                                  ctypes.c_ulong, ctypes.c_int]
        x11.XDestroyImage.argtypes = [ctypes.POINTER(_XImage)]
        x11.XFreePixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        xc.XCompositeRedirectWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int]
        xc.XCompositeUnredirectWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int]
        xc.XCompositeNameWindowPixmap.restype = ctypes.c_ulong
        xc.XCompositeNameWindowPixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        dpy = x11.XOpenDisplay(None)
        if not dpy:
            return False
        _xcomp = (x11, xc, dpy)
        return True
    except Exception:
        return False


def grab_window_pixmap(wid, w, h):
    if not _init_xcomp():
        return None
    x11, xc, dpy = _xcomp
    try:
        xc.XCompositeRedirectWindow(dpy, wid, 0)
        x11.XSync(dpy, 0)
        pm = xc.XCompositeNameWindowPixmap(dpy, wid)
        x11.XSync(dpy, 0)
        im = None
        if pm:
            xi = x11.XGetImage(dpy, pm, 0, 0, max(1, w), max(1, h), 0x00FFFFFF, 2)
            if xi:
                i = xi.contents
                data = ctypes.string_at(i.data, i.bytes_per_line * i.height)
                im = Image.frombuffer("RGBA", (i.width, i.height), data, "raw",
                                      "BGRA", i.bytes_per_line, 1).convert("RGB")
                x11.XDestroyImage(xi)
            x11.XFreePixmap(dpy, pm)
        xc.XCompositeUnredirectWindow(dpy, wid, 0)
        x11.XSync(dpy, 0)
        return im
    except Exception:
        return None


# 缩略图按用户设定的 (行, 列) 自适应：卡片恰好铺满可视区，不会越出右边界。
def choose_grid(n, cols, rows, avail_w=728, avail_h=470):
    cols = max(1, min(10, cols))
    rows = max(1, min(10, rows))
    pad = 6
    cell_w = max(74, int(avail_w / cols))
    cell_h = max(86, int(avail_h / rows))
    card_w = max(62, cell_w - pad * 2)
    card_h = max(86, cell_h - pad * 2)
    tw = max(50, card_w - 14)
    # 底部给标题+状态两行留足空间（原 48 偏紧，长标题会把状态行挤出边框）
    th = max(40, min(int(tw * 0.55), card_h - 54))
    return cols, tw, th, card_w, th + 54


N = len(entries)
COLS, TW, TH, CARD_W, CARD_H = choose_grid(N, grid_cols, grid_rows)

# 原始缩略图按固定尺寸缓存，行列/尺寸变化时只需重采样，无需重新抓图。
RAW_TW, RAW_TH = 320, 180


def grab_thumb(wid, x, y, w, h):
    scale = min(1.0, RAW_TW / max(1, w), RAW_TH / max(1, h))
    gw, gh = max(1, int(w * scale)), max(1, int(h * scale))
    im = grab_window_pixmap(int(wid, 16), gw, gh)
    if im is None:
        try:
            im = ImageGrab.grab(bbox=(max(0, x), max(0, y),
                                      max(1, x + w), max(1, y + h))).convert("RGB")
        except Exception:
            im = Image.new("RGB", (RAW_TW, RAW_TH), T["muted"])
    return im.resize((RAW_TW, RAW_TH), RESAMPLE) if im.size != (RAW_TW, RAW_TH) else im


def display_order():
    if sort_mode == "title":
        return sorted(range(len(entries)), key=lambda i: entries[i][5].casefold())
    return list(range(len(entries)))


view_order = display_order()


def chosen_indices():
    return [i for i in view_order if sel[i].get()]


# ---------- 圆角卡片 / 布局小图 ----------
def rounded(d, box, r, fill=None, outline=None, width=1):
    d.rounded_rectangle(box, radius=r, fill=fill, outline=outline, width=width)


CARD_SS = 3  # 超采样倍数：数字圆点/勾选/圆角先按 3x 画再缩小，消除毛刺锯齿


def render_card(thumb, selected, number, title):
    s = CARD_SS
    W, H = CARD_W * s, CARD_H * s
    im = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    px = lambda v: int(v * s)

    rounded(d, (px(3), px(4), W - px(1), H - px(1)), px(10), fill=T["shadow"])
    rounded(d, (px(1), px(1), W - px(3), H - px(3)), px(10),
            fill=T["panel"], outline=T["accent"] if selected else T["border"],
            width=px(2) if selected else px(1))

    # 缩略图：先放大到 3x 再贴，圆角遮罩同样超采样
    tw, th = px(TW), px(TH)
    big_thumb = thumb.convert("RGB").resize((tw, th), RESAMPLE)
    mask = Image.new("L", (tw, th), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, tw - 1, th - 1),
                                           radius=px(7), fill=255)
    im.paste(big_thumb, (px(7), px(7)), mask)

    # 左上角序号圆点：实心圆 + 用 mm 锚点精确居中数字，3x 超采样
    if selected and number:
        r = 13
        cx0, cy0 = 7 + r, 7 + r
        d.ellipse((px(cx0 - r), px(cy0 - r), px(cx0 + r), px(cy0 + r)),
                  fill=T["accent"], outline=T["on"], width=px(2))
        d.text((px(cx0), px(cy0)), str(number), font=pf(14 * s, True),
               fill=T["on"], anchor="mm")

    # 右上角勾选：白色对勾画在蓝色实心圆上，抗锯齿
    cx, cy = CARD_W - 24, 7 + 13
    rad = 12
    d.ellipse((px(cx - rad), px(cy - rad), px(cx + rad), px(cy + rad)),
              fill=T["accent"] if selected else T["muted"],
              outline=T["on"] if selected else T["border"], width=px(2))
    if selected:
        d.line((px(cx - 6), px(cy), px(cx - 1), px(cy + 5), px(cx + 7), px(cy - 6)),
               fill=T["on"], width=px(3), joint="curve")

    # 锚点用左上角(lt)，文字高度完全落在卡片内，不再因字体 ascent 越过下边界
    ftitle = pf(11 * s, selected)
    d.text((px(10), px(TH + 10)),
           fit_text(d, title, ftitle, (CARD_W - 20) * s),
           font=ftitle, fill=T["text"] if selected else T["sub"], anchor="lt")
    sub = f"第 {number} 位 · 点击取消" if selected else "点击选择"
    d.text((px(10), px(TH + 28)), sub, font=pf(9 * s),
           fill=T["accent"] if selected else T["sub"], anchor="lt")
    return im.resize((CARD_W, CARD_H), RESAMPLE)


TILE_W, TILE_H = 88, 56


def render_tile(slug, active):
    s = 3
    W, H = TILE_W * s, TILE_H * s
    im = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    p = lambda v: int(v * s)
    rounded(d, (p(1), p(1), W - p(2), H - p(2)), p(8),
            fill=T["soft"] if active else T["tile"],
            outline=T["accent"] if active else T["border"],
            width=p(2) if active else p(1))
    x0, y0, x1, y1 = p(13), p(9), W - p(13), H - p(9)
    for x, y, w, h in SLOTS[slug]:
        rounded(d, (x0 + x * (x1 - x0) + p(1), y0 + y * (y1 - y0) + p(1),
                    x0 + (x + w) * (x1 - x0) - p(1), y0 + (y + h) * (y1 - y0) - p(1)),
                p(2), fill=T["accent"] if active else T["tile_shape"])
    return ImageTk.PhotoImage(im.resize((TILE_W, TILE_H), RESAMPLE))


def render_logo():
    im = Image.new("RGBA", (32, 32), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    for x, y in ((2, 2), (18, 2), (2, 18), (18, 18)):
        rounded(d, (x, y, x + 12, y + 12), 3, fill=T["accent"])
    return ImageTk.PhotoImage(im)


class ThinScrollbar(tk.Canvas):
    """细圆角滚动条：覆盖式、默认隐藏，悬停/滚动/拖动时才显示（系统自动隐藏效果）。"""

    def __init__(self, master, view, width=10):
        super().__init__(master, width=width, bg=T["panel"],
                         highlightthickness=0, bd=0)
        self.view = view
        self.thumb = None
        self.dragging = False
        self.drag_off = 0
        self.visible = False
        self._hide_job = None
        self.bind("<Configure>", lambda ev: self.redraw())
        self.bind("<ButtonPress-1>", self.on_press)
        self.bind("<B1-Motion>", self.on_drag)
        self.bind("<ButtonRelease-1>", self.on_release)
        self.bind("<Enter>", lambda ev: self.show())
        self.bind("<Leave>", lambda ev: self.schedule_hide())

    def parts(self):
        first, last = self.view.yview()
        h = self.winfo_height()
        if last - first >= 0.9999 or h <= 0:
            return None
        th = max(28, int(h * (last - first)))
        return h, th, int(first * (h - th))

    def show(self):
        if self._hide_job:
            self.after_cancel(self._hide_job)
            self._hide_job = None
        if not self.visible:
            self.visible = True
        self.redraw()

    def schedule_hide(self):
        if self.dragging:
            return
        if self._hide_job:
            self.after_cancel(self._hide_job)
        self._hide_job = self.after(700, self.hide)

    def hide(self):
        if self.dragging:
            return
        self.visible = False
        self.redraw()

    def redraw(self, hover=False):
        self.delete("all")
        # 默认隐藏：既不可滚动，或未进入悬停/滚动状态，都不画滑块
        if not self.visible and not (hover or self.dragging):
            return
        p = self.parts()
        if not p:
            return
        h, th, y = p
        color = T["accent"] if (hover or self.dragging) else T["scroll"]
        im = Image.new("RGBA", (8, th), (0, 0, 0, 0))
        ImageDraw.Draw(im).rounded_rectangle((0, 0, 7, th - 1), radius=4, fill=color)
        self.thumb = ImageTk.PhotoImage(im)
        self.create_image(1, y, image=self.thumb, anchor="nw")

    def on_press(self, ev):
        p = self.parts()
        if not p:
            return
        h, th, y = p
        if y <= ev.y <= y + th:
            self.drag_off = ev.y - y
        else:
            self.drag_off = th / 2
            self.view.yview_moveto(max(0, (ev.y - th / 2) / max(1, h - th)))
        self.dragging = True
        self.redraw(True)

    def on_drag(self, ev):
        if not self.dragging:
            return
        p = self.parts()
        if not p:
            return
        h, th, _ = p
        frac = (ev.y - self.drag_off) / max(1, h - th)
        self.view.yview_moveto(min(1, max(0, frac)))
        self.redraw(True)

    def on_release(self, ev):
        self.dragging = False
        self.redraw(False)


# 在弹窗前抓图（窗口还没被对话框遮挡）
base = [grab_thumb(e[0], e[1], e[2], e[3], e[4]) for e in entries]

# ---------- 主窗口 ----------
root = tk.Tk()
root.withdraw()
SW, SH = root.winfo_screenwidth(), root.winfo_screenheight()
root.title("轻格分窗 - 窗口排列")
WIN_W, WIN_H = 1120, min(SH - 80, 700)
root.geometry(f"{WIN_W}x{WIN_H}+{(SW - WIN_W) // 2}+{(SH - WIN_H) // 2}")
root.resizable(False, False)
root.configure(bg=T["bg"])
root.attributes("-topmost", True)
root.lift()
root.focus_force()

# 顶部标题栏
hdr = tk.Frame(root, bg=T["panel"], height=70)
hdr.pack(fill="x", side="top")
hdr.pack_propagate(False)
logo = render_logo()
tk.Label(hdr, image=logo, bg=T["panel"]).pack(side="left", padx=(22, 12))
htext = tk.Frame(hdr, bg=T["panel"])
htext.pack(side="left", pady=12)
tk.Label(htext, text="窗口排列", bg=T["panel"], fg=T["text"],
         font=tf(16, True)).pack(anchor="w")
tk.Label(htext, text="左侧选布局、右侧选窗口 · 蓝色区块即窗口的最终落位",
         bg=T["panel"], fg=T["sub"], font=tf(10)).pack(anchor="w", pady=(3, 0))
tk.Label(hdr, text="LitheZones v0.4.6", bg=T["panel"], fg=T["sub"],
         font=tf(9)).pack(side="right", padx=22)

body = tk.Frame(root, bg=T["bg"])
body.pack(fill="both", expand=True, padx=16, pady=(12, 8))


def section(parent, text, top=12):
    tk.Label(parent, text=text, bg=T["panel"], fg=T["accent"],
             font=tf(11, True)).pack(anchor="w", padx=16, pady=(top, 6))


# ---- 左侧：布局选择 + 排列预览 ----
left = tk.Frame(body, bg=T["panel"], width=320,
                highlightthickness=1, highlightbackground=T["border"])
left.pack(side="left", fill="y", padx=(0, 14))
left.pack_propagate(False)
section(left, "① 选择布局", top=12)

tiles_frame = tk.Frame(left, bg=T["panel"])
tiles_frame.pack(padx=12)
tile_items = []
tile_photos = []
for i, (slug, name) in enumerate(LAYOUTS):
    cell = tk.Frame(tiles_frame, bg=T["panel"])
    cell.grid(row=i // 3, column=i % 3, padx=4, pady=4)
    imgl = tk.Label(cell, bg=T["panel"])
    imgl.pack()
    cap = tk.Label(cell, text=name, bg=T["panel"], font=tf(10))
    cap.pack()
    for wd in (imgl, cap):
        wd.bind("<Button-1>", lambda ev, s=slug: set_layout(s))
    tile_items.append((slug, imgl, cap))
    tile_photos.append(None)

section(left, "② 排列预览", top=14)
PV_W, PV_H = 284, 140
pv = tk.Label(left, bg=T["panel"], width=PV_W, height=PV_H)
pv.pack(padx=16)
hint = tk.Label(left, text="", bg=T["panel"], fg=T["sub"], font=tf(10),
                wraplength=282, justify="left")
hint.pack(anchor="w", padx=16, pady=(6, 2))
tk.Label(left, text="● 蓝色 = 已选窗口的落位     ○ 灰色 = 待填充",
         bg=T["panel"], fg=T["sub"], font=tf(9)).pack(anchor="w", padx=16, pady=(4, 0))

tips = tk.Frame(left, bg=T["soft"])
tips.pack(fill="x", padx=16, pady=(12, 10))
tk.Label(tips, text="操作提示", bg=T["soft"], fg=T["accent"],
         font=tf(10, True)).pack(anchor="w", padx=10, pady=(9, 4))
tk.Label(tips,
         text="1. 先选布局，再点卡片勾选窗口\n"
              "2. 勾选数不能超过布局区块数\n"
              "3. 切换布局自动重排，超出区块的\n"
              "   窗口自动取消勾选",
         bg=T["soft"], fg=T["sub"], font=tf(9), justify="left",
         wraplength=260).pack(anchor="w", padx=10, pady=(0, 10))

PV_PHOTO = [None]


def draw_preview(count):
    """排列预览：与上方布局小图一致的圆角矩形，3x 超采样抗锯齿。"""
    s = 3
    im = Image.new("RGBA", (PV_W * s, PV_H * s), T["panel"])
    d = ImageDraw.Draw(im)
    p = lambda v: int(v * s)
    bx0, by0, bx1, by1 = p(12), p(10), p(PV_W - 12), p(PV_H - 10)
    rounded(d, (bx0, by0, bx1, by1), p(10),
            fill=T["tile"], outline=T["border"], width=p(1))
    bw, bh = bx1 - bx0, by1 - by0
    gap = p(3)
    for k, (x, y, w, h) in enumerate(SLOTS[layout_slug]):
        x0 = bx0 + int(x * bw) + gap
        y0 = by0 + int(y * bh) + gap
        x1 = bx0 + int((x + w) * bw) - gap
        y1 = by0 + int((y + h) * bh) - gap
        on = k < count
        rounded(d, (x0, y0, x1, y1), p(6),
                fill=T["accent"] if on else T["muted"],
                outline=T["accent"] if on else T["border"],
                width=p(2) if on else p(1))
        d.text(((x0 + x1) / 2, (y0 + y1) / 2), str(k + 1),
               font=pf(11 * s, True), fill=T["on"] if on else T["sub"],
               anchor="mm")
    PV_PHOTO[0] = ImageTk.PhotoImage(im.resize((PV_W, PV_H), RESAMPLE))
    pv.config(image=PV_PHOTO[0])


# ---- 右侧：窗口缩略图卡片墙 ----
right = tk.Frame(body, bg=T["panel"],
                 highlightthickness=1, highlightbackground=T["border"])
right.pack(side="left", fill="both", expand=True)
rh = tk.Frame(right, bg=T["panel"])
rh.pack(fill="x", padx=16, pady=(14, 6))

# 右侧工具条：行列数可调 + 标题排序，便于快速定位窗口
ctrl = tk.Frame(rh, bg=T["panel"])
ctrl.pack(side="right")


def small_btn(text, cmd, width=2):
    return tk.Button(ctrl, text=text, command=cmd, width=width, font=tf(9),
                     bg=T["muted"], fg=T["text"], activebackground=T["border"],
                     relief="flat", bd=0, padx=2, pady=1)


tk.Label(ctrl, text="列", bg=T["panel"], fg=T["sub"], font=tf(9)).pack(
    side="left", padx=(2, 3))
small_btn("−", lambda: change_grid(dcols=-1)).pack(side="left")
cols_lbl = tk.Label(ctrl, text=str(grid_cols), bg=T["panel"], fg=T["accent"],
                    font=tf(10, True), width=2)
cols_lbl.pack(side="left")
small_btn("+", lambda: change_grid(dcols=1)).pack(side="left", padx=(0, 10))
tk.Label(ctrl, text="行", bg=T["panel"], fg=T["sub"], font=tf(9)).pack(
    side="left", padx=(0, 3))
small_btn("−", lambda: change_grid(drows=-1)).pack(side="left")
rows_lbl = tk.Label(ctrl, text=str(grid_rows), bg=T["panel"], fg=T["accent"],
                    font=tf(10, True), width=2)
rows_lbl.pack(side="left")
small_btn("+", lambda: change_grid(drows=1)).pack(side="left", padx=(0, 10))
sort_btn = tk.Button(ctrl,
                     text="排序：标题" if sort_mode == "title" else "排序：默认",
                     command=lambda: cycle_sort(), font=tf(9), bg=T["soft"], fg=T["accent"],
                     activebackground=T["border"], relief="flat", bd=0,
                     padx=6, pady=2)
sort_btn.pack(side="left")

tk.Label(rh, text="选择窗口", bg=T["panel"], fg=T["text"],
         font=tf(12, True)).pack(side="left")
counter = tk.Label(rh, text="", bg=T["soft"], fg=T["accent"],
                   font=tf(10, True), padx=8, pady=2)
counter.pack(side="left", padx=10)
tk.Label(rh, text="点击卡片勾选", bg=T["panel"], fg=T["sub"],
         font=tf(9)).pack(side="left")

cwrap = tk.Frame(right, bg=T["panel"])
cwrap.pack(fill="both", expand=True, padx=(10, 6), pady=4)
cw = tk.Canvas(cwrap, bg=T["panel"], highlightthickness=0)
sb = ThinScrollbar(cwrap, cw)
cw.configure(yscrollcommand=lambda first, last: (sb.redraw(), sb.show() if sb.dragging else None))
sb.pack(side="right", fill="y")
cw.pack(side="left", fill="both", expand=True)
inner = tk.Frame(cw, bg=T["panel"])
cw.create_window((0, 0), window=inner, anchor="nw")


def wheel(ev):
    if getattr(ev, "delta", 0):
        step = -3 if ev.delta > 0 else 3
    else:
        step = -3 if ev.num == 4 else 3
    cw.yview_scroll(step, "units")
    sb.show()
    sb.schedule_hide()


# 鼠标进入卡片区时短暂显示滚动条，离开后由 schedule_hide 自动隐藏
for wd in (cwrap, cw, inner):
    wd.bind("<Enter>", lambda ev: (sb.show(), sb.schedule_hide()))
    wd.bind("<MouseWheel>", wheel)
    wd.bind("<Button-4>", wheel)
    wd.bind("<Button-5>", wheel)
n0 = nslots(layout_slug)
sel = [tk.BooleanVar(value=(i < n0)) for i in range(len(entries))]
cards = []
photos = [[None, None] for _ in entries]


def sync_scroll():
    """窗口映射后再算滚动区域；宽度锁定为画布宽，避免横向越界。"""
    iw = max(cw.winfo_width() - 4, inner.winfo_width())
    cw.configure(scrollregion=(0, 0, iw, inner.winfo_height()))
    sb.redraw()


def build_cards():
    for wd in list(inner.winfo_children()):
        wd.destroy()
    cards.clear()
    for pos, eidx in enumerate(view_order):
        card = tk.Frame(inner, bg=T["panel"])
        card.grid(row=pos // COLS, column=pos % COLS, padx=6, pady=6, sticky="nw")
        lbl = tk.Label(card, bg=T["panel"])
        lbl.pack()
        for wd in (card, lbl):
            wd.bind("<Button-1>", lambda ev, idx=eidx: toggle(idx))
            wd.bind("<MouseWheel>", wheel)
            wd.bind("<Button-4>", wheel)
            wd.bind("<Button-5>", wheel)
        cards.append((card, lbl, eidx))
    inner.configure(width=max(1, cw.winfo_width() - 4))
    inner.update_idletasks()
    sync_scroll()


def apply_grid():
    """按画布实际尺寸与当前行列设置重算卡片尺寸，确保最后一列不越界。"""
    global COLS, TW, TH, CARD_W, CARD_H
    aw, ah = cw.winfo_width(), cw.winfo_height()
    if aw < 100 or ah < 100:
        aw, ah = 728, 470
    COLS, TW, TH, CARD_W, CARD_H = choose_grid(
        N, grid_cols, grid_rows, aw - 6, ah - 6)
    build_cards()
    refresh()

# ---------- 确定 / 取消 ----------
confirmed = False
result = []


def on_ok():
    global confirmed
    confirmed = True
    chosen = chosen_indices()
    result.append("layout:" + layout_slug)
    result.extend(entries[i][0] for i in chosen)
    root.destroy()


def on_cancel():
    root.destroy()


# ---------- 底部操作栏 ----------
footer = tk.Frame(root, bg=T["panel"], height=58)
footer.pack(fill="x", side="bottom")
footer.pack_propagate(False)
status = tk.Label(footer, text="", bg=T["panel"], fg=T["sub"],
                  font=tf(10), anchor="w")
status.pack(side="left", padx=20, fill="x", expand=True)


def btn(text, command, primary=False):
    return tk.Button(footer, text=text, command=command, font=tf(10, primary),
                     bg=T["accent"] if primary else T["muted"],
                     fg=T["on"] if primary else T["text"],
                     activebackground=T["accent"] if primary else T["border"],
                     relief="flat", bd=0, padx=14, pady=6)


# side="right" 依次 packing：应用布局最靠右，最终从左到右为 全选/清空/取消/应用布局
btn("应用布局", on_ok, primary=True).pack(side="right", padx=(4, 20))
btn("取消", on_cancel).pack(side="right", padx=4)
btn("清空", lambda: set_all(False)).pack(side="right", padx=4)
btn("全选", lambda: set_all(True)).pack(side="right", padx=4)


def set_status(text, warn=False):
    status.config(text=text, fg=T["warn"] if warn else T["sub"])


def refresh():
    chosen = chosen_indices()
    order = {idx: k + 1 for k, idx in enumerate(chosen)}
    for k, (slug, imgl, cap) in enumerate(tile_items):
        active = slug == layout_slug
        tile_photos[k] = render_tile(slug, active)
        imgl.config(image=tile_photos[k])
        cap.config(fg=T["accent"] if active else T["sub"],
                   font=tf(10, active))
    for _, lbl, eidx in cards:
        on = sel[eidx].get()
        photos[eidx][0] = ImageTk.PhotoImage(
            render_card(base[eidx], False, 0, entries[eidx][5]))
        photos[eidx][1] = ImageTk.PhotoImage(
            render_card(base[eidx], True, order.get(eidx, 0), entries[eidx][5]))
        lbl.config(image=photos[eidx][1 if on else 0])
    draw_preview(len(chosen))
    counter.config(text=f"{len(chosen)} / {nslots(layout_slug)}")
    hint.config(text=f"已选 {len(chosen)} 个窗口，按卡片序号依次落入对应区块。")


def set_layout(slug):
    global layout_slug
    limit = nslots(slug)
    chosen = chosen_indices()
    if len(chosen) > limit:
        for i in chosen[limit:]:
            sel[i].set(False)
        set_status(f"已切换到「{LAYOUT_NAMES[slug]}」，超出区块数，自动保留前 {limit} 个窗口",
                   warn=True)
    else:
        set_status(f"已切换到「{LAYOUT_NAMES[slug]}」，窗口按新布局自动排列")
    layout_slug = slug
    refresh()


def toggle(i):
    if sel[i].get():
        sel[i].set(False)
        set_status("")
    elif len(chosen_indices()) >= nslots(layout_slug):
        set_status(f"「{LAYOUT_NAMES[layout_slug]}」只有 {nslots(layout_slug)} 个区块，"
                   "请先取消一个窗口", warn=True)
        return
    else:
        sel[i].set(True)
        set_status("")
    refresh()


def set_all(on):
    limit = nslots(layout_slug)
    if on:
        for i in range(len(entries)):
            sel[i].set(False)
        for i in view_order[:limit]:
            sel[i].set(True)
        set_status(f"该布局最多 {limit} 个窗口，已选中前 {min(limit, len(entries))} 个",
                   warn=len(entries) > limit)
    else:
        for i in range(len(entries)):
            sel[i].set(False)
        set_status("")
    refresh()


def change_grid(dcols=0, drows=0):
    global grid_cols, grid_rows
    if dcols:
        grid_cols = max(1, min(10, grid_cols + dcols))
    if drows:
        grid_rows = max(1, min(10, grid_rows + drows))
    cols_lbl.config(text=str(grid_cols))
    rows_lbl.config(text=str(grid_rows))
    apply_grid()
    set_status(f"卡片网格已调整为 {grid_rows} 行 × {grid_cols} 列")


def cycle_sort():
    global sort_mode, view_order
    sort_mode = "title" if sort_mode == "default" else "default"
    sort_btn.config(text="排序：标题" if sort_mode == "title" else "排序：默认")
    view_order = display_order()
    apply_grid()
    set_status("已按窗口标题排序，序号同步更新" if sort_mode == "title"
               else "已恢复默认窗口顺序")


root.protocol("WM_DELETE_WINDOW", on_cancel)
root.bind("<Escape>", lambda ev: on_cancel())
root.bind("<Return>", lambda ev: on_ok())
root.deiconify()
root.update_idletasks()
apply_grid()
root.after(80, sync_scroll)
root.mainloop()

if not confirmed:
    sys.exit(1)
for line in result:
    print(line)
