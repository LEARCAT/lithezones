#!/usr/bin/env python3
"""轻格分窗 - 统一面板：窗口排列 / 布局管理在同一窗口，左上角标签切换。

Rust 通过 --tab=window|layout 指定初始标签，并同时传入窗口列表与已保存布局。
窗口内所有操作通过 stdin/stdout 逐行命令与 Rust 通信：
  apply<TAB><layout_slug><TAB><wid>...   -> ok/err 消息
  save<TAB><name>                        -> ok/err 消息
  load<TAB><index>                       -> ok/err 消息
  rename<TAB><index><TAB><newname>       -> ok/err 消息
  delete<TAB><index>                     -> ok/err 消息
  close                                  -> 无应答，Rust 结束通信循环
"""
import sys
import os
import json
import time
import queue
import threading
import ctypes
import ctypes.util
import subprocess

US = "\x1f"
RS = "\x1e"

# Pillow 必须自包含：deepin 系统 python3 不带 PIL，用户目录里的副本也可能
# 缺 pillow.libs 私有库（libtiff/libjpeg 等）导致 _imaging 导入失败，面板
# 直接打不开。启动时优先把脚本同目录的 vendor/ 放到 sys.path 最前面。
_HERE = os.path.dirname(os.path.abspath(__file__))
_VENDOR = os.path.join(_HERE, "vendor")
if os.path.isdir(_VENDOR):
    sys.path.insert(0, _VENDOR)


# ---------- 系统字体 ----------
def _fc(pattern, field):
    try:
        return subprocess.check_output(
            ["fc-match", "-f", "%{" + field + "}", pattern],
            text=True, timeout=2).strip()
    except Exception:
        return ""


def _gsettings(schema, key):
    try:
        return subprocess.check_output(
            ["gsettings", "get", schema, key], text=True, timeout=2).strip()
    except Exception:
        return ""


def _gsettings_str(schema, key):
    raw = _gsettings(schema, key)
    if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in ("'", '"'):
        return raw[1:-1]
    return raw


def _gsettings_num(schema, key, default):
    try:
        value = float(_gsettings(schema, key))
        return value if value > 0 else default
    except (TypeError, ValueError):
        return default


# Deepin 控制中心「字体」页实际读写的是 com.deepin.dde.appearance。
# 面板跟随它的字体名、标准字号与紧凑字号；取不到时退回 XSettings，再退回 fontconfig。
#
# 这些值原先每次启动面板都要 fork 4~6 个子进程（gsettings 约 60ms、fc-match
# 约 250ms），冷启动被拖到 2 秒。现在结果缓存在配置目录，只有 dconf 数据库
# 变化时（用户在控制中心改了字体）或缓存超过 7 天才重新查询。
_FONT_CACHE_PATH = os.path.join(os.path.expanduser("~/.config/lithezones"), "font-cache.json")
_DCONF_DB = os.path.join(
    os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "dconf", "user")


def _dconf_stamp():
    try:
        return os.stat(_DCONF_DB).st_mtime_ns
    except OSError:
        return 0


def _query_font_config():
    family = (
        _gsettings_str("com.deepin.dde.appearance", "font-standard")
        or _gsettings_str("com.deepin.xsettings", "qt-font-name")
        or _fc("sans-serif:lang=zh-cn", "family[0]")
        or "Sans"
    )
    size = _gsettings_num(
        "com.deepin.dde.appearance", "font-size",
        10.5,
    )
    compact = _gsettings_num("com.deepin.dde.appearance", "compact-font-size", 9.0)
    # PIL 不能靠 family 名做中文回退：系统字体若是 Noto Sans，它的 Regular/Bold
    # 文件都不含中文字形，直接拿来画会出方框。缩略图统一走 fontconfig 的中文
    # sans-serif 回退（通常是思源黑体），Tk 控件本身仍用系统字体名，由 Tk 负责回退。
    regular = _fc("sans-serif:lang=zh-cn", "file") or _fc(
        f"{family}:lang=zh-cn", "file")
    bold = _fc("sans-serif:lang=zh-cn:style=Bold", "file") or regular
    return {
        "dconf": _dconf_stamp(), "ts": time.time(),
        "family": family, "size": size, "compact": compact,
        "regular": regular, "bold": bold,
    }


def _font_config():
    if os.environ.get("LITHEZONES_FONT_REFRESH") != "1":
        try:
            with open(_FONT_CACHE_PATH, encoding="utf-8") as f:
                c = json.load(f)
            if (c.get("dconf") == _dconf_stamp()
                    and time.time() - float(c.get("ts", 0)) < 7 * 86400):
                return c
        except Exception:
            pass
    c = _query_font_config()
    try:
        os.makedirs(os.path.dirname(_FONT_CACHE_PATH), exist_ok=True)
        tmp = "%s.%d.tmp" % (_FONT_CACHE_PATH, os.getpid())
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(c, f)
        os.replace(tmp, _FONT_CACHE_PATH)
    except Exception:
        pass
    return c


_FCFG = _font_config()

# 托盘启动时用这个轻量模式预热字体缓存，避免用户第一次打开面板时
# 同步等待 gsettings/fc-match 的一秒左右子进程开销。
if "--warm-fonts" in sys.argv:
    sys.exit(0)

import tkinter as tk
from PIL import Image, ImageDraw, ImageFont, ImageTk, ImageGrab

RESAMPLE = getattr(Image, "LANCZOS", getattr(Image, "ANTIALIAS", 3))

FONT_FAMILY = _FCFG.get("family") or "Sans"
SYS_FONT_SIZE = float(_FCFG.get("size") or 10.5)
SYS_COMPACT_SIZE = float(_FCFG.get("compact") or 9.0)
# 以 10.5 为基准：用户调大/调小系统字体时，面板里所有字号同比缩放。
FONT_SCALE = SYS_FONT_SIZE / 10.5
FONT_FILE = _FCFG.get("regular") or ""
FONT_BOLD_FILE = _FCFG.get("bold") or FONT_FILE


def _system_size(legacy):
    """把脚本里的 9/10/11/12 号映射到当前系统字号。"""
    if legacy <= 9:
        return SYS_COMPACT_SIZE * FONT_SCALE
    return SYS_FONT_SIZE + (legacy - 10)


def _px_size(legacy, scale=1):
    return max(1, int(_system_size(legacy) * scale + 0.5))


def tf(size, bold=False):
    real = _px_size(size)
    return (FONT_FAMILY, real, "bold") if bold else (FONT_FAMILY, real)


_FONT_CACHE = {}


def pf(size, bold=False, scale=1):
    """PIL 字体：size 是脚本基准字号，scale 是缩略图超采样倍率。"""
    px = _px_size(size, scale)
    key = (px, bold)
    if key not in _FONT_CACHE:
        path = FONT_BOLD_FILE if bold else FONT_FILE
        try:
            _FONT_CACHE[key] = ImageFont.truetype(path, px) if path else ImageFont.load_default()
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


# ---------- 主题配色 ----------
THEMES = {
    "light": dict(bg="#f5f6f8", panel="#ffffff", text="#1f2329", sub="#646a73",
                  border="#dfe2e8", accent="#2d7ff9", accent_hover="#1b66d9",
                  soft="#eaf2ff", muted="#eceef1",
                  tile_shape="#cfd6e0", shadow="#c9cdd4", scroll="#a9b2be", on="#ffffff",
                  warn="#d93026", tile="#f7f9fc"),
    "dark": dict(bg="#1e2023", panel="#2b2e33", text="#e8eaed", sub="#9aa0a6",
                 border="#41454b", accent="#5b9bff", accent_hover="#7ab0ff",
                 soft="#263b5c", muted="#3a3e44",
                 tile_shape="#545c66", shadow="#141619", scroll="#6b7280", on="#ffffff",
                 warn="#ff6b61", tile="#31343a"),
}

# 主题色预设：键写入 panel-prefs.json，不影响 config.json 的 light/dark/auto。
ACCENTS = {
    "blue": ("蔚蓝", "#2d7ff9", "#1b66d9"),
    "green": ("青绿", "#00a870", "#008f5f"),
    "purple": ("紫罗兰", "#8e5cf7", "#7645d8"),
    "orange": ("暖橙", "#f5a623", "#d88c12"),
    "red": ("朱红", "#e54545", "#c93636"),
    "cyan": ("青蓝", "#00a0b0", "#008a99"),
}


def _hex_rgb(value):
    value = value.lstrip("#")
    return tuple(int(value[i:i + 2], 16) for i in (0, 2, 4))


def _blend_hex(a, b, ratio):
    """按 ratio 把 b 混进 a，用来从主题色生成柔和背景和边框。"""
    ar, ag, ab = _hex_rgb(a)
    br, bg, bb = _hex_rgb(b)
    return "#%02x%02x%02x" % tuple(
        round(a + (b - a) * ratio)
        for a, b in ((ar, br), (ag, bg), (ab, bb))
    )


def palette_for(base_name, accent_key):
    """返回一套完整配色；base_name 决定深浅，accent_key 决定强调色。"""
    base = THEMES.get(base_name, THEMES["light"])
    label, accent, hover = ACCENTS.get(accent_key, ACCENTS["blue"])
    out = dict(base)
    out["accent"] = accent
    out["accent_hover"] = hover
    out["soft"] = _blend_hex(base["panel"], accent, 0.12 if base_name != "dark" else 0.22)
    out["tile_shape"] = _blend_hex(base["border"], accent, 0.45)
    out["tile"] = _blend_hex(base["panel"], accent, 0.04 if base_name != "dark" else 0.08)
    out["accent_label"] = label
    return out

LAYOUTS = [
    ("quarter", "四分网格"), ("top_bottom", "上下对半"), ("left_right", "左右对半"),
    ("left1_right2", "左1右2"), ("top1_bottom2", "上1下2"),
    ("left2_right1", "左2右1"), ("top2_bottom1", "上2下1"), ("top3_bottom3", "上3下3"),
    ("center_float", "居中浮动"),
]
SLOTS = {
    "quarter": [(0, 0, .5, .5), (.5, 0, .5, .5), (0, .5, .5, .5), (.5, .5, .5, .5)],
    "top_bottom": [(0, 0, 1, .5), (0, .5, 1, .5)],
    "left_right": [(0, 0, .5, 1), (.5, 0, .5, 1)],
    "left1_right2": [(0, 0, 1 / 3, 1), (1 / 3, 0, 2 / 3, .5), (1 / 3, .5, 2 / 3, .5)],
    "top1_bottom2": [(0, 0, 1, .5), (0, .5, .5, .5), (.5, .5, .5, .5)],
    "left2_right1": [(0, 0, .5, .5), (0, .5, .5, .5), (.5, 0, .5, 1)],
    "top2_bottom1": [(0, 0, .5, .5), (.5, 0, .5, .5), (0, .5, 1, .5)],
    "top3_bottom3": [
        (0, 0, 1 / 3, .5), (1 / 3, 0, 1 / 3, .5), (2 / 3, 0, 1 / 3, .5),
        (0, .5, 1 / 3, .5), (1 / 3, .5, 1 / 3, .5), (2 / 3, .5, 1 / 3, .5),
    ],
    "center_float": [(0.18, 0.14, 0.64, 0.72)],
}
LAYOUT_NAMES = dict(LAYOUTS)


def slot_fractions(slug, count):
    """按实际勾选数量返回布局块，和 Rust grid_slots 的网格形状保持一致。

    网格只由布局形状+勾选数量决定；每个窗口自身的尺寸约束如何落进格子，
    是 Rust fit_slot 的事（自由填满 / min 撑破左上角浮窗 / 小窗居中）。"""
    count = max(1, count)
    if slug == "center_float":
        return SLOTS[slug]
    if slug == "left_right":
        return [(i / count, 0, 1 / count, 1) for i in range(count)]
    if slug == "top_bottom":
        return [(0, i / count, 1, 1 / count) for i in range(count)]
    if slug == "quarter":
        if count == 1:
            return [(0, 0, 1, 1)]
        if count == 2:
            return [(0, 0, .5, 1), (.5, 0, .5, 1)]
        if count == 3:
            return [(0, 0, .5, 1), (.5, 0, .5, .5), (.5, .5, .5, .5)]
        return SLOTS[slug]
    if slug == "left1_right2":
        if count == 1:
            return [(0, 0, 1, 1)]
        if count == 2:
            return [(0, 0, 1 / 3, 1), (1 / 3, 0, 2 / 3, 1)]
        if count == 3:
            return SLOTS[slug]
        return [(0, 0, 1 / 3, 1)] + [
            (1 / 3, i / (count - 1), 2 / 3, 1 / (count - 1))
            for i in range(count - 1)
        ]
    if slug == "top1_bottom2":
        if count == 1:
            return [(0, 0, 1, 1)]
        if count == 2:
            return [(0, 0, 1, .5), (0, .5, 1, .5)]
        if count == 3:
            return SLOTS[slug]
        return [(0, 0, 1, .5)] + [
            (i / (count - 1), .5, 1 / (count - 1), .5)
            for i in range(count - 1)
        ]
    if slug == "left2_right1":
        if count == 1:
            return [(0, 0, 1, 1)]
        if count == 2:
            return [(0, 0, .5, 1), (.5, 0, .5, 1)]
        if count == 3:
            return SLOTS[slug]
        # 左侧固定两行，右侧随窗口数均分（与 Rust build_tree 一致）
        return [(0, 0, .5, .5), (0, .5, .5, .5)] + [
            (.5, i / (count - 2), .5, 1 / (count - 2))
            for i in range(count - 2)
        ]
    if slug == "top2_bottom1":
        if count == 1:
            return [(0, 0, 1, 1)]
        if count == 2:
            return [(0, 0, 1, .5), (0, .5, 1, .5)]
        if count == 3:
            return SLOTS[slug]
        return [(0, 0, .5, .5), (.5, 0, .5, .5)] + [
            (i / (count - 2), .5, 1 / (count - 2), .5)
            for i in range(count - 2)
        ]
    if slug == "top3_bottom3":
        count = min(6, count)  # 最多 6 格，多余窗口由 Rust 侧收起
        if count <= 3:
            return [(i / count, 0, 1 / count, 1) for i in range(count)]
        top = [(i / 3, 0, 1 / 3, .5) for i in range(3)]
        bottom = [(i / (count - 3), .5, 1 / (count - 3), .5)
                  for i in range(count - 3)]
        return top + bottom
    return SLOTS.get(slug, SLOTS["quarter"])


def nslots(slug):
    return len(SLOTS.get(slug, SLOTS["quarter"]))


# 面板显示偏好：列表/微缩图模式、微缩图行列数。用户每次切换或调整都写盘，
# 下次打开面板按上次的设置还原；Rust 目前不传这些参数，命令行参数仍可覆盖。
_PANEL_PREFS_PATH = os.path.join(os.path.expanduser("~/.config/lithezones"),
                                 "panel-prefs.json")


def _read_panel_prefs():
    try:
        with open(_PANEL_PREFS_PATH, encoding="utf-8") as f:
            data = json.load(f)
        return data if isinstance(data, dict) else {}
    except Exception:
        return {}


_prefs = _read_panel_prefs()


def _pref_grid(key, default):
    try:
        return max(1, min(10, int(_prefs.get(key, default))))
    except (TypeError, ValueError):
        return default


def _save_panel_prefs():
    data = _read_panel_prefs()
    data.update({"list_mode": bool(list_mode),
                 "grid_rows": grid_rows, "grid_cols": grid_cols,
                 "accent": accent_name})
    try:
        os.makedirs(os.path.dirname(_PANEL_PREFS_PATH), exist_ok=True)
        tmp = "%s.%d.%d.tmp" % (_PANEL_PREFS_PATH, os.getpid(), time.time_ns())
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f)
        os.replace(tmp, _PANEL_PREFS_PATH)
    except Exception:
        pass


# ---------- 参数 ----------
initial_tab = "window"
layout_slug = "quarter"
theme_name = "light"
accent_name = _prefs.get("accent", "blue")
if accent_name not in ACCENTS:
    accent_name = "blue"
version = "0.5.54"
grid_rows = _pref_grid("grid_rows", 4)
grid_cols = _pref_grid("grid_cols", 5)
sort_mode = "default"
list_mode = bool(_prefs.get("list_mode", False))
REFRESH_INTERVAL = 1.5
current_count = 0
current_rects = ""
entries = []
items = []

for arg in sys.argv[1:]:
    if arg.startswith("--tab="):
        _t = arg.split("=", 1)[1]
        initial_tab = _t if _t in ("window", "layout", "hotkeys", "theme") else "window"
    elif arg.startswith("--layout="):
        layout_slug = arg.split("=", 1)[1]
    elif arg.startswith("--theme="):
        theme_name = arg.split("=", 1)[1]
    elif arg.startswith("--accent="):
        _a = arg.split("=", 1)[1]
        accent_name = _a if _a in ACCENTS else accent_name
    elif arg.startswith("--version="):
        version = arg.split("=", 1)[1]
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
    elif arg.startswith("--count="):
        try:
            current_count = max(0, int(arg.split("=", 1)[1]))
        except ValueError:
            pass
    elif arg.startswith("--current-rects="):
        current_rects = arg.split("=", 1)[1]
    elif arg.startswith("--item="):
        fields = arg.split("=", 1)[1].split(US)
        while len(fields) < 4:
            fields.append("")
        try:
            count = int(fields[2])
        except ValueError:
            count = 0
        items.append(dict(name=fields[0], created=fields[1], count=count, rects=fields[3]))
    else:
        # 格式：id|x|y|w|h|显示名|hidden。split 限 6 段，显示名里可以含 '|'。
        parts = arg.split("|", 6)
        if len(parts) == 6:
            # 兼容旧版 Rust 未传 hidden 字段的启动参数。
            parts.append("0")
        if len(parts) != 7:
            continue
        wid, xs, ys, ws, hs, name, hidden = parts
        try:
            entries.append((wid, int(xs), int(ys), int(ws), int(hs), name,
                            hidden == "1"))
        except ValueError:
            pass

T = palette_for(theme_name, accent_name)
if layout_slug not in SLOTS:
    layout_slug = "quarter"
sel_idx = 0 if items else -1


def token(s):
    """协议字段不能含制表符/换行。"""
    return str(s).replace("\t", " ").replace("\n", " ").replace("\r", " ")


# stdin 由单独的线程统一读取：除命令应答外，Rust 还会在面板已打开时
# 主动发 tab\t<window|layout>，用来切换标签而不是再开一个窗口。
_pending = threading.Lock()
_reply_q = queue.Queue()
_ctrl_q = queue.Queue()
# 后台刷新线程抓完图和窗口列表后投递到这里，Tk 主线程负责更新控件。
_refresh_q = queue.Queue()
_refresh_busy = [False]
_closing = [False]
_entries_sig = ""
# 应用/加载后延迟最小化的「代次」。用户若在定时器触发前从任务栏/快捷键
# 重新打开面板，_poll_ctrl 会把代次 +1，过期的定时器就不再最小化，
# 避免「面板弹出后又被旧定时器收起」。
_minimize_gen = [0]


def _stdin_loop():
    for line in sys.stdin:
        line = line.rstrip("\r\n")
        if line.startswith("tab\t"):
            _ctrl_q.put(line.split("\t", 1)[1])
        elif _pending.locked():
            _reply_q.put(line)


def send(*fields):
    """向 Rust 主程序发一条命令并等待应答，返回 (成功, 消息)。"""
    with _pending:
        print("\t".join(token(f) for f in fields), flush=True)
        try:
            reply = _reply_q.get(timeout=30)
        except queue.Empty:
            return False, "与后台程序通信中断"
    parts = reply.split("\t", 1)
    return parts[0] == "ok", parts[1] if len(parts) > 1 else ""


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
_error_handler = None


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


def choose_grid(n, cols, rows, avail_w=728, avail_h=470):
    cols = max(1, min(10, cols))
    rows = max(1, min(10, rows))
    pad = 6
    cell_w = max(74, int(avail_w / cols))
    cell_h = max(86, int(avail_h / rows))
    card_w = max(62, cell_w - pad * 2)
    card_h = max(86, cell_h - pad * 2)
    tw = max(50, card_w - 14)
    th = max(40, min(int(tw * 0.55), card_h - 54))
    return cols, tw, th, card_w, th + 54


N = len(entries)
COLS, TW, TH, CARD_W, CARD_H = choose_grid(N, grid_cols, grid_rows)
RAW_TW, RAW_TH = 320, 180


# 启动缩略图只在面板出现前抓一次整屏，之后所有窗口卡片从这张图裁剪。
# 旧实现每个窗口各做一次 ImageGrab.grab(bbox=...)（实测单次约 24ms），
# 13 个窗口就是 300ms+；XComposite 路径每个窗口还要 3 次 XSync 往返（~33ms）。
# 整屏只抓一次（~30ms）+ 内存裁剪（<1ms），面板打开明显更快。
_SCREEN_SHOT = None


def _grab_screen_once():
    global _SCREEN_SHOT
    if _SCREEN_SHOT is None:
        try:
            shot = ImageGrab.grab()
            # grab() 通常已是 RGB；无谓的 convert 会多遍历一遍整屏像素（实测 ~3ms）。
            _SCREEN_SHOT = shot if shot.mode == "RGB" else shot.convert("RGB")
        except Exception:
            _SCREEN_SHOT = False
    return _SCREEN_SHOT


def grab_thumb(wid, x, y, w, h, live=False):
    im = None
    if live:
        # 面板开着时会挡住后面的窗口，整屏截图只能拍到面板自己。实时刷新
        # 必须走 XComposite 抓窗口自身像素；在后台线程执行，UI 不会卡住。
        im = grab_window_pixmap(int(wid, 16),
                                max(1, min(RAW_TW, w)), max(1, min(RAW_TH, h)))
        if im is None:
            # 抓不到就保留上一张，不要退回整屏裁剪（会被面板遮挡）。
            return None
    else:
        # XComposite 能在窗口被遮挡时仍抓到自身像素，但它对每个窗口要 3 次 XSync
        # （慢）。面板在弹窗前抓图、窗口基本无遮挡，优先用整屏一次抓取的快照裁剪。
        screen = _grab_screen_once()
        if screen:
            try:
                im = screen.crop((max(0, x), max(0, y),
                                  min(screen.width, max(1, x + w)),
                                  min(screen.height, max(1, y + h))))
            except Exception:
                im = None
        if im is None:
            im = grab_window_pixmap(int(wid, 16),
                                    max(1, min(RAW_TW, w)), max(1, min(RAW_TH, h)))
        if im is None:
            im = Image.new("RGB", (RAW_TW, RAW_TH), T["muted"])
    # 缩略图只有 320x180，BILINEAR 与 LANCZOS 肉眼无差别，但大图缩小快近一倍
    # （1920x1080→320x180：14ms vs 7ms），面板冷启动少掉几十毫秒。
    return im.resize((RAW_TW, RAW_TH), Image.BILINEAR) if im.size != (RAW_TW, RAW_TH) else im


def entries_signature(seq):
    """窗口 ID/几何/标题/最小化状态只要有一个变了就重建卡片。"""
    return RS.join("|".join(str(x) for x in e) for e in seq)


def parse_windows_reply(msg):
    """解析 Rust `windows` 应答：RS 分记录、US 分字段。"""
    result = []
    for rec in (msg or "").split(RS):
        fields = rec.split(US)
        if len(fields) < 6:
            continue
        try:
            wid = fields[0]
            x, y, w, h = (int(fields[1]), int(fields[2]),
                          int(fields[3]), int(fields[4]))
        except ValueError:
            continue
        hidden = len(fields) > 6 and fields[6] == "1"
        result.append((wid, x, y, w, h, fields[5], hidden))
    return result


def grab_live_thumbs(entries_snapshot):
    """后台线程批量抓当前窗口内容；返回 {窗口 ID: PIL.Image}。"""
    out = {}
    for e in entries_snapshot:
        try:
            out[e[0]] = grab_thumb(e[0], e[1], e[2], e[3], e[4], live=True)
        except Exception:
            out[e[0]] = None
    return out


def display_order():
    if sort_mode == "title":
        return sorted(range(len(entries)), key=lambda i: entries[i][5].casefold())
    return list(range(len(entries)))


view_order = display_order()


def chosen_indices():
    # 编号与落位都按用户的点击先后，不按卡片在列表里的显示顺序。
    # pick_order 记录点击顺序；兜底把漏登记的已选窗口按显示顺序补在末尾，
    # 避免任何入口状态不同步时丢窗口。
    picked = [i for i in pick_order if sel[i].get()]
    seen = set(picked)
    for i in view_order:
        if sel[i].get() and i not in seen:
            picked.append(i)
    return picked


# ---------- 圆角卡片 / 布局小图 ----------
def rounded(d, box, r, fill=None, outline=None, width=1):
    d.rounded_rectangle(box, radius=r, fill=fill, outline=outline, width=width)


CARD_SS = 3


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
    tw, th = px(TW), px(TH)
    big_thumb = thumb.convert("RGB").resize((tw, th), RESAMPLE)
    mask = Image.new("L", (tw, th), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, tw - 1, th - 1), radius=px(7), fill=255)
    im.paste(big_thumb, (px(7), px(7)), mask)
    if selected and number:
        r = 13
        cx0, cy0 = 7 + r, 7 + r
        d.ellipse((px(cx0 - r), px(cy0 - r), px(cx0 + r), px(cy0 + r)),
                  fill=T["accent"], outline=T["on"], width=px(2))
        # anchor="mm" 按字体 em 框居中，但思源黑体的数字墨迹在 em 框里偏下
        # （实测约 4px@s=3），视觉上贴圈底。按实际墨迹包围盒反推偏移，
        # 让数字真正落在圆心；跟随系统字体，不硬编码像素。
        num_font = pf(14, True, s)
        num = str(number)
        ink = d.textbbox((0, 0), num, font=num_font, anchor="mm")
        dy = round((ink[1] + ink[3]) / 2)
        d.text((px(cx0), px(cy0) - dy), num, font=num_font,
               fill=T["on"], anchor="mm")
    cx, cy = CARD_W - 24, 7 + 13
    rad = 12
    d.ellipse((px(cx - rad), px(cy - rad), px(cx + rad), px(cy + rad)),
              fill=T["accent"] if selected else T["muted"],
              outline=T["on"] if selected else T["border"], width=px(2))
    if selected:
        d.line((px(cx - 6), px(cy), px(cx - 1), px(cy + 5), px(cx + 7), px(cy - 6)),
               fill=T["on"], width=px(3), joint="curve")
    ftitle = pf(11, selected, s)
    d.text((px(10), px(TH + 10)),
           fit_text(d, title, ftitle, (CARD_W - 20) * s),
           font=ftitle, fill=T["text"] if selected else T["sub"], anchor="lt")
    sub = f"第 {number} 位 · 点击取消" if selected else "点击选择"
    d.text((px(10), px(TH + 28)), sub, font=pf(9, False, s),
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


# ---------- 细圆角滚动条（自动隐藏） ----------
class ThinScrollbar(tk.Canvas):
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
# className 让 WM_CLASS 在窗口创建的那一刻就是 lithezones，而不是默认的 Tk。
# dock 在窗口 map 时就抓取 class 归类应用；若等 Rust 事后改 class，dock
# 往往已经按 tk/Tk 处理，导致任务栏里没有轻格分窗图标（0.5.31 曾能显示）。
root = tk.Tk(className="lithezones")
root.withdraw()
SW, SH = root.winfo_screenwidth(), root.winfo_screenheight()
root.title("轻格分窗 - 管理面板")
# 让任务栏/dock 把面板当成 LitheZones 应用窗口：设置 WM_CLASS 与启动器一致，
# 并用启动器图标（同目录 lithezones-panel.png，由 assets/lithezones.svg 生成）
# 作为窗口图标。否则 WM_CLASS 是默认的 tk/Tk，dock 上要么没图标、要么显示
# 通用 Tk 图标，用户看不到"这个面板属于轻格分窗"。
try:
    _icon_path = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                              "lithezones-panel.png")
    if os.path.exists(_icon_path):
        _panel_icon = tk.PhotoImage(file=_icon_path)
        root.iconphoto(True, _panel_icon)
except Exception:
    pass
# WM_CLASS 由上面的 tk.Tk(className="lithezones") 在窗口创建时就设好，
# 不依赖 Rust 侧后置修改；Tk 的 winfo_id 是子窗口 ID，改它无效。
# 面板尺寸必须跟随屏幕，不能写死 1120：小尺寸显示器、竖屏或缩放后
# 逻辑分辨率变小的桌面，1120 宽的窗口会直接超出屏幕看不全。
WIN_W = min(1120, max(560, SW - 40), SW)
WIN_H = min(min(max(SH - 80, 360), 700), SH)
WIN_X = max(0, (SW - WIN_W) // 2)
WIN_Y = max(0, (SH - WIN_H) // 2)
root.geometry(f"{WIN_W}x{WIN_H}+{WIN_X}+{WIN_Y}")
root.resizable(False, False)
root.configure(bg=T["bg"])
# 不设 -topmost：deepin/kwin 会把置顶窗口的最小化立刻撤销，
# 表现为"点最小化面板又自动弹出来"。置前交给 Rust 侧 activate_panel_by_id。
root.lift()
root.focus_force()

hdr = tk.Frame(root, bg=T["panel"], height=70)
hdr.pack(fill="x", side="top")
hdr.pack_propagate(False)
logo = render_logo()
logo_lbl = tk.Label(hdr, image=logo, bg=T["panel"])
logo_lbl.pack(side="left", padx=(20, 8))
htext = tk.Frame(hdr, bg=T["panel"])
htext.pack(side="left", pady=8)

# 左上角标签：点击切换「窗口排列 / 布局管理」
tabbar = tk.Frame(htext, bg=T["panel"])
tabbar.pack(anchor="w")
tab_btns = {}
for _key, _text in (("window", "窗口排列"), ("layout", "布局管理"),
                    ("hotkeys", "快捷键"), ("theme", "主题")):
    _lbl = tk.Label(tabbar, text=_text, bg=T["panel"])
    _lbl.pack(side="left", padx=(0, 16))
    _lbl.bind("<Button-1>", lambda ev, k=_key: switch_tab(k))
    tab_btns[_key] = _lbl

subtitle = tk.Label(htext, text="", bg=T["panel"], fg=T["sub"], font=tf(9))
subtitle.pack(anchor="w", pady=(3, 0))
tk.Label(hdr, text=f"LitheZones v{version}", bg=T["panel"], fg=T["sub"],
         font=tf(9)).pack(side="right", padx=22)

# 内容区与底部栏用 place 固定到精确像素：pack 在固定尺寸、不可缩放的窗口里
# 切换子页时会沿用旧的当前高度（布局页残留 51px），两页内容区就差 7px。
# place 不参与自动分配，两个标签的 footer 都严格是 y=642、高 58。
FOOTER_H = 58
content = tk.Frame(root, bg=T["bg"])
content.place(x=0, y=70, width=WIN_W, height=WIN_H - 70 - FOOTER_H)


def section(parent, text, top=12):
    tk.Label(parent, text=text, bg=T["panel"], fg=T["accent"],
             font=tf(10, True)).pack(anchor="w", padx=16, pady=(top, 6))


# ================= 窗口排列标签页 =================
body_win = tk.Frame(content, bg=T["bg"])
left = tk.Frame(body_win, bg=T["panel"], width=320,
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
    cap = tk.Label(cell, text=name, bg=T["panel"], font=tf(9))
    cap.pack()
    for wd in (imgl, cap):
        wd.bind("<Button-1>", lambda ev, s=slug: set_layout(s))
    tile_items.append((slug, imgl, cap))
    tile_photos.append(None)

section(left, "② 排列预览", top=14)
PV_W, PV_H = 284, 140
pv = tk.Label(left, bg=T["panel"], width=PV_W, height=PV_H)
pv.pack(padx=16)
hint = tk.Label(left, text="", bg=T["panel"], fg=T["sub"], font=tf(9),
                wraplength=292, justify="left")
hint.pack(anchor="w", padx=14, pady=(6, 2))
tk.Label(left, text="● 高亮 = 已选窗口的落位     ○ 灰色 = 待填充",
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
    key = (layout_slug, count)
    cached = _preview_cache.get(key)
    if cached is not None:
        PV_PHOTO[0] = cached
        pv.config(image=cached)
        return
    s = 3
    im = Image.new("RGBA", (PV_W * s, PV_H * s), T["panel"])
    d = ImageDraw.Draw(im)
    p = lambda v: int(v * s)
    bx0, by0, bx1, by1 = p(12), p(10), p(PV_W - 12), p(PV_H - 10)
    rounded(d, (bx0, by0, bx1, by1), p(10),
            fill=T["tile"], outline=T["border"], width=p(1))
    bw, bh = bx1 - bx0, by1 - by0
    gap = p(3)
    slots = SLOTS[layout_slug] if count <= 0 else slot_fractions(layout_slug, count)
    for k, (x, y, w, h) in enumerate(slots):
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
               font=pf(11, True, s), fill=T["on"] if on else T["sub"], anchor="mm")
    PV_PHOTO[0] = ImageTk.PhotoImage(im.resize((PV_W, PV_H), RESAMPLE))
    _preview_cache[key] = PV_PHOTO[0]
    pv.config(image=PV_PHOTO[0])


right = tk.Frame(body_win, bg=T["panel"],
                 highlightthickness=1, highlightbackground=T["border"])
right.pack(side="left", fill="both", expand=True)
rh = tk.Frame(right, bg=T["panel"])
rh.pack(fill="x", padx=16, pady=(14, 6))

ctrl = tk.Frame(rh, bg=T["panel"])
ctrl.pack(side="right")


def small_btn(text, cmd, width=2):
    return tk.Button(ctrl, text=text, command=cmd, width=width, font=tf(9),
                     bg=T["muted"], fg=T["sub"], activebackground=T["border"],
                     activeforeground=T["sub"], relief="flat", bd=0, padx=2, pady=1)


grid_buttons = []
grid_labels = []

col_word = tk.Label(ctrl, text="列", bg=T["panel"], fg=T["sub"], font=tf(9))
col_word.pack(side="left", padx=(2, 3))
grid_labels.append((col_word, T["sub"]))
_b = small_btn("−", lambda: change_grid(dcols=-1))
_b.pack(side="left")
grid_buttons.append(_b)
cols_lbl = tk.Label(ctrl, text=str(grid_cols), bg=T["panel"], fg=T["accent"],
                    font=tf(9, True), width=2)
cols_lbl.pack(side="left")
grid_labels.append((cols_lbl, T["accent"]))
_b = small_btn("+", lambda: change_grid(dcols=1))
_b.pack(side="left", padx=(0, 10))
grid_buttons.append(_b)
row_word = tk.Label(ctrl, text="行", bg=T["panel"], fg=T["sub"], font=tf(9))
row_word.pack(side="left", padx=(0, 3))
grid_labels.append((row_word, T["sub"]))
_b = small_btn("−", lambda: change_grid(drows=-1))
_b.pack(side="left")
grid_buttons.append(_b)
rows_lbl = tk.Label(ctrl, text=str(grid_rows), bg=T["panel"], fg=T["accent"],
                    font=tf(9, True), width=2)
rows_lbl.pack(side="left")
grid_labels.append((rows_lbl, T["accent"]))
_b = small_btn("+", lambda: change_grid(drows=1))
_b.pack(side="left", padx=(0, 10))
grid_buttons.append(_b)
sort_btn = tk.Button(ctrl,
                     text="排序：标题" if sort_mode == "title" else "排序：默认",
                     command=lambda: cycle_sort(), font=tf(9), bg=T["soft"], fg=T["accent"],
                     activebackground=T["border"], activeforeground=T["sub"],
                     relief="flat", bd=0, padx=6, pady=2)
sort_btn.pack(side="left")

tk.Label(rh, text="选择窗口", bg=T["panel"], fg=T["accent"],
         font=tf(10, True)).pack(side="left")
counter = tk.Label(rh, text="", bg=T["soft"], fg=T["accent"],
                   font=tf(9, True), padx=8, pady=2)
counter.pack(side="left", padx=10)
hint_text = tk.Label(rh, text="点击卡片勾选", bg=T["panel"], fg=T["sub"],
                     font=tf(9))
hint_text.pack(side="left")
mode_btn = tk.Button(rh, text="微缩图模式" if list_mode else "列表模式",
                     command=lambda: toggle_list_mode(),
                     font=tf(9), bg=T["soft"], fg=T["accent"],
                     activebackground=T["border"], activeforeground=T["sub"],
                     relief="flat", bd=0, padx=8, pady=2)
mode_btn.pack(side="left", padx=(12, 0))

cwrap = tk.Frame(right, bg=T["panel"])
cwrap.pack(fill="both", expand=True, padx=(10, 6), pady=4)
cw = tk.Canvas(cwrap, bg=T["panel"], highlightthickness=0)
sb = ThinScrollbar(cwrap, cw)
cw.configure(yscrollcommand=lambda first, last: (sb.redraw(), sb.show() if sb.dragging else None))
sb.pack(side="right", fill="y")
cw.pack(side="left", fill="both", expand=True)
inner = tk.Frame(cw, bg=T["panel"])
# 用 canvas item 的 width 显式撑开嵌入窗口：只设 Frame 的 width 会被
# grid 几何传播按子控件内容缩回去，列表行就铺不满右侧内容区。
_inner_item = cw.create_window((0, 0), window=inner, anchor="nw")


def wheel(ev):
    if getattr(ev, "delta", 0):
        step = -3 if ev.delta > 0 else 3
    else:
        step = -3 if ev.num == 4 else 3
    cw.yview_scroll(step, "units")
    sb.show()
    sb.schedule_hide()


for wd in (cwrap, cw, inner):
    # 滚动条默认隐藏：只在用户真正滚动（滚轮/拖动）或鼠标移到滚动条上时显示，
    # 鼠标进入列表区域不再弹出，避免截图和日常使用时右侧一直挂着一条。
    wd.bind("<MouseWheel>", wheel)
    wd.bind("<Button-4>", wheel)
    wd.bind("<Button-5>", wheel)

# 打开面板默认不勾选任何窗口，让用户自己决定排列哪几个。
sel = [tk.BooleanVar(value=False) for _ in entries]
# 点击顺序：每点选一个窗口就追加到末尾，取消则移除。编号/落位都读这里，
# 与显示排序解耦。
pick_order = []
cards = []
# slot 0/1 = 微缩图卡片未选中/选中；列表模式只有一行标题，不放图片。
photos = [[None, None] for _ in entries]

# 渲染缓存：缩略图本身固定，卡片只有“选中/未选中 + 序号”几种状态，
# 布局图标固定的 9×2 种，预览按“布局 + 勾选数”。全部生成一次后复用，
# 勾选窗口时不再逐张重绘（这是原来点击卡顿的主因）。
_card_cache = {}        # (eidx, selected, number) -> PhotoImage
_tile_photo_cache = {}  # (slug, active) -> PhotoImage
_preview_cache = {}     # (slug, count) -> PhotoImage


def card_photo(eidx, selected, number):
    key = (eidx, selected, number)
    im = _card_cache.get(key)
    if im is None:
        im = ImageTk.PhotoImage(
            render_card(base[eidx], selected, number, entries[eidx][5]))
        _card_cache[key] = im
    return im


def tile_photo(slug, active):
    key = (slug, active)
    im = _tile_photo_cache.get(key)
    if im is None:
        im = render_tile(slug, active)
        _tile_photo_cache[key] = im
    return im


def sync_scroll():
    # 列表行靠 inner 的实际宽度铺满；这里同步 canvas item 的宽度，
    # 避免 Frame 被子控件内容反向压缩。额外 +8 抵消列表行右侧 8px 留白，
    # 使列表行右边框与上方「排序」按钮右缘、canvas 右缘对齐。
    item_w = max(1, cw.winfo_width() + 8)
    cw.itemconfig(_inner_item, width=item_w)
    iw = max(item_w, inner.winfo_width())
    cw.configure(scrollregion=(0, 0, iw, inner.winfo_height()))
    sb.redraw()


def build_cards():
    for wd in list(inner.winfo_children()):
        wd.destroy()
    cards.clear()
    if list_mode:
        # 列表只显示标题：行铺满内容区，标题靠左对齐；行右边框与上方
        # 「排序」按钮右缘对齐，左右各留 8px。
        inner.columnconfigure(0, weight=1)
        for pos, eidx in enumerate(view_order):
            row = tk.Frame(inner, bg=T["panel"],
                           highlightthickness=1, highlightbackground=T["border"])
            row.grid(row=pos, column=0, sticky="ew", padx=(8, 8), pady=3)
            text_lbl = tk.Label(row, text="", anchor="w", justify="left",
                                bg=T["panel"], fg=T["sub"], font=tf(9))
            text_lbl.pack(side="left", padx=(8, 8), pady=5)
            for wd in (row, text_lbl):
                wd.bind("<Button-1>", lambda ev, idx=eidx: toggle(idx))
                wd.bind("<MouseWheel>", wheel)
                wd.bind("<Button-4>", wheel)
                wd.bind("<Button-5>", wheel)
            cards.append((row, text_lbl, eidx))
    else:
        # 列表模式把第 0 列设为 weight=1（让列表行铺满），切回微缩图时
        # 必须归零，否则第 0 列会继续吃掉所有多余宽度，把后面的卡片
        # 整体右推，看起来第一张和后续卡片之间空了一大块。
        inner.columnconfigure(0, weight=0)
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
    inner.update_idletasks()
    sync_scroll()


def apply_grid():
    global COLS, TW, TH, CARD_W, CARD_H
    aw, ah = cw.winfo_width(), cw.winfo_height()
    if aw < 100 or ah < 100:
        aw, ah = 728, 470
    COLS, TW, TH, CARD_W, CARD_H = choose_grid(N, grid_cols, grid_rows, aw - 6, ah - 6)
    # 卡片尺寸变了，旧的渲染缓存全部作废，否则会显示旧尺寸的图。
    _card_cache.clear()
    build_cards()
    refresh_win()


footer_win = tk.Frame(root, bg=T["panel"], height=58)
footer_win.pack_propagate(False)
status_win = tk.Label(footer_win, text="", bg=T["panel"], fg=T["sub"],
                      font=tf(9), anchor="w")
status_win.pack(side="left", padx=20, fill="x", expand=True)


def btn(parent, text, command, primary=False):
    # 字号/字重与左上角 tab 完全一致（tf(10)），只保留配色区分主次；
    # 内边距收窄，避免按钮色块视觉上比纯文字 tab 大一圈。
    return tk.Button(parent, text=text, command=command, font=tf(10),
                     bg=T["accent"] if primary else T["muted"],
                     fg=T["on"] if primary else T["sub"],
                     # 主按钮悬停时背景加深（白字保持），给出明确的动态反馈；
                     # 之前 activebackground 与静止同色，悬停看不出任何变化。
                     activebackground=T["accent_hover"] if primary else T["border"],
                     # Tk 未设置 activeforeground 时悬停会用系统默认黑色。
                     # 次要按钮用主题 sub；主按钮保持白字，
                     # 灰字压在蓝底上对比度太差，视觉上很脏。
                     activeforeground=T["on"] if primary else T["sub"],
                     relief="flat", bd=0, padx=8, pady=4)


def win_btn(text, command, primary=False):
    return btn(footer_win, text, command, primary).pack(side="right", padx=4, pady=10)


apply_btn = btn(footer_win, "应用布局", lambda: None, True)
# 最右按钮的右边缘与内容卡片右边框对齐：body 用 padx=16，right 卡片再带
# 1px 边框，所以内容区右边缘距窗口右边=16px。footer 是 place 占满窗口宽，
# 按钮原 padx=4 会让它比内容卡片右缘再靠右 12px（截图里明显突出一截）。
apply_btn.pack(side="right", padx=(4, 16), pady=10)
win_btn("取消", lambda: close_panel())
win_btn("清空", lambda: set_all(False))
win_btn("全选", lambda: set_all(True))


def set_status(text, warn=False):
    status_win.config(text=text, fg=T["warn"] if warn else T["sub"])


def refresh_win():
    chosen = chosen_indices()
    order = {idx: k + 1 for k, idx in enumerate(chosen)}
    for k, (slug, imgl, cap) in enumerate(tile_items):
        active = slug == layout_slug
        tile_photos[k] = tile_photo(slug, active)
        imgl.config(image=tile_photos[k])
        cap.config(fg=T["accent"] if active else T["sub"], font=tf(9, active))
    if list_mode:
        for row, text_lbl, eidx in cards:
            on = sel[eidx].get()
            number = order.get(eidx, 0) if on else 0
            title = entries[eidx][5]
            row.config(bg=T["soft"] if on else T["panel"],
                       highlightbackground=T["accent"] if on else T["border"])
            # 列表只保留标题；选中项用行底色和标题前缀数字表示落位顺序。
            # 字号与左侧布局名称一致（tf(9)），不再用 tf(10) 的大号字。
            text_lbl.config(text=f"{number}. {title}" if on else title,
                            fg=T["accent"] if on else T["sub"],
                            font=tf(9))
    else:
        for _, lbl, eidx in cards:
            on = sel[eidx].get()
            # 只渲染当前真正会显示的状态。旧实现把未选中窗口也画成「选中且序号0」，
            # 那张图永远不会上屏；13 窗冷启动能省掉一半卡片渲染。
            # toggle/select 都会再走 refresh_win，另一状态在需要时才渲染。
            slot = 1 if on else 0
            photos[eidx][slot] = card_photo(eidx, on, order.get(eidx, 0) if on else 0)
            lbl.config(image=photos[eidx][slot])
    draw_preview(len(chosen))
    counter.config(text=f"{len(chosen)} / {nslots(layout_slug)}")
    hint_text.config(text="点击列表项勾选" if list_mode else "点击卡片勾选")
    hint.config(text=f"已选 {len(chosen)} 个窗口，按序号依次落入对应区块。")


def set_grid_enabled(enabled):
    for wd in grid_buttons:
        wd.config(state="normal" if enabled else "disabled")
    for wd, color in grid_labels:
        wd.config(fg=color if enabled else T["muted"])


def toggle_list_mode():
    global list_mode
    list_mode = not list_mode
    mode_btn.config(text="微缩图模式" if list_mode else "列表模式")
    set_grid_enabled(not list_mode)
    _save_panel_prefs()
    apply_grid()
    set_status("已切换到列表模式，行列设置暂不可用" if list_mode
               else "已恢复微缩图模式，行列设置可用")


def apply_live_refresh(new_entries, thumbs, entries_changed):
    """把后台线程抓到的实时窗口数据合并到界面，保留勾选和点击顺序。"""
    global entries, N, base, sel, pick_order, cards, photos, view_order, _entries_sig
    if entries_changed:
        old_thumbs = {entries[i][0]: base[i] for i in range(len(entries))}
        # 用窗口 ID 而不是数组下标记住勾选：新窗口插入/旧窗口关闭都不会让
        # 用户刚选好的窗口错位。
        old_pick = [entries[i][0] for i in pick_order
                    if i < len(sel) and sel[i].get()]
        entries = new_entries
        N = len(entries)
        id_to_idx = {w[0]: i for i, w in enumerate(entries)}
        sel = [tk.BooleanVar(value=False) for _ in entries]
        pick_order = []
        for wid in old_pick:
            i = id_to_idx.get(wid)
            if i is not None and not sel[i].get():
                sel[i].set(True)
                pick_order.append(i)
        base = [thumbs.get(w[0]) or old_thumbs.get(w[0])
                or Image.new("RGB", (RAW_TW, RAW_TH), T["muted"])
                for w in entries]
        photos = [[None, None] for _ in entries]
        _card_cache.clear()
        view_order = display_order()
        _entries_sig = entries_signature(entries)
        apply_grid()
    else:
        # 窗口集合没变，只更新缩略图像素，不重建控件（保住滚动位置）。
        touched = False
        for i, w in enumerate(entries):
            im = thumbs.get(w[0])
            if im is not None:
                base[i] = im
                touched = True
        if touched:
            _card_cache.clear()
            refresh_win()


def set_layout(slug):
    global layout_slug
    limit = nslots(slug)
    chosen = chosen_indices()
    if len(chosen) > limit:
        for i in chosen[limit:]:
            sel[i].set(False)
            if i in pick_order:
                pick_order.remove(i)
        set_status(f"已切换到「{LAYOUT_NAMES[slug]}」，超出区块数，自动保留前 {limit} 个窗口", warn=True)
    else:
        set_status(f"已切换到「{LAYOUT_NAMES[slug]}」，窗口按新布局自动排列")
    layout_slug = slug
    refresh_win()


def toggle(i):
    if sel[i].get():
        sel[i].set(False)
        if i in pick_order:
            pick_order.remove(i)
        set_status("")
    elif nslots(layout_slug) == 1:
        # 居中浮动只需一个窗口：点另一个时自动取消原先那个，改成用户刚点的。
        # 不再提示「请先取消一个窗口」——那种交互等于强迫用户点两下。
        for k in range(len(sel)):
            sel[k].set(False)
        pick_order.clear()
        pick_order.append(i)
        sel[i].set(True)
        set_status("")
    elif len(chosen_indices()) >= nslots(layout_slug):
        set_status(f"「{LAYOUT_NAMES[layout_slug]}」只有 {nslots(layout_slug)} 个区块，请先取消一个窗口", warn=True)
        return
    else:
        sel[i].set(True)
        pick_order.append(i)
        set_status("")
    refresh_win()


def set_all(on):
    limit = nslots(layout_slug)
    if on:
        for i in range(len(entries)):
            sel[i].set(False)
        pick_order.clear()
        for i in view_order[:limit]:
            sel[i].set(True)
            pick_order.append(i)
        set_status(f"该布局最多 {limit} 个窗口，已选中前 {min(limit, len(entries))} 个",
                   warn=len(entries) > limit)
    else:
        for i in range(len(entries)):
            sel[i].set(False)
        pick_order.clear()
        set_status("")
    refresh_win()


def change_grid(dcols=0, drows=0):
    global grid_cols, grid_rows
    if list_mode:
        return
    if dcols:
        grid_cols = max(1, min(10, grid_cols + dcols))
    if drows:
        grid_rows = max(1, min(10, grid_rows + drows))
    cols_lbl.config(text=str(grid_cols))
    rows_lbl.config(text=str(grid_rows))
    _save_panel_prefs()
    apply_grid()
    set_status(f"卡片网格已调整为 {grid_rows} 行 × {grid_cols} 列")


def cycle_sort():
    global sort_mode, view_order
    sort_mode = "title" if sort_mode == "default" else "default"
    sort_btn.config(text="排序：标题" if sort_mode == "title" else "排序：默认")
    view_order = display_order()
    apply_grid()
    set_status("已按窗口标题排序，编号保持点击顺序不变" if sort_mode == "title" else "已恢复默认窗口顺序")


def do_apply():
    chosen = chosen_indices()
    if not chosen:
        set_status("请先勾选至少一个窗口", True)
        return
    ok, msg = send("apply", layout_slug, *(entries[i][0] for i in chosen))
    if ok:
        set_status((msg or "布局已应用") + " · 面板将最小化")
        # 用户要求：应用布局后面板只收起，不退出。root.destroy() 会走 close
        # 协议结束 Rust 面板线程，托盘再点就要冷启动；这里改成 iconify。
        # 带代次的延迟最小化：若这 150ms 内用户已从任务栏重新打开面板，
        # 过期定时器会被丢弃，不会把刚弹出来的面板又收回去。
        _schedule_minimize()
    else:
        set_status(msg or "应用失败", True)


# 应用布局按钮在 do_apply 定义后绑定
apply_btn.config(command=do_apply)


# ================= 布局管理标签页 =================
body_layout = tk.Frame(content, bg=T["bg"])
# 左右分栏与「窗口排列」保持一致：左栏固定 320、右栏展开。
left2 = tk.Frame(body_layout, bg=T["panel"], width=320,
                 highlightthickness=1, highlightbackground=T["border"])
left2.pack(side="left", fill="both", padx=(0, 14))
left2.pack_propagate(False)
lhead = tk.Frame(left2, bg=T["panel"])
lhead.pack(fill="x", pady=(12, 2))
tk.Label(lhead, text="① 已保存布局", bg=T["panel"], fg=T["accent"],
         font=tf(10, True)).pack(side="left", padx=16)
count_badge = tk.Label(lhead, text="", bg=T["soft"], fg=T["accent"],
                       font=tf(9, True), padx=8, pady=2)
count_badge.pack(side="left", padx=8)

lst = tk.Frame(left2, bg=T["panel"])
lst.pack(fill="both", expand=True, padx=(10, 4), pady=(4, 10))
lcanvas = tk.Canvas(lst, bg=T["panel"], highlightthickness=0)
sb2 = ThinScrollbar(lst, lcanvas)
lcanvas.configure(yscrollcommand=lambda first, last: (sb2.redraw(), sb2.show() if sb2.dragging else None))
sb2.pack(side="right", fill="y")
lcanvas.pack(side="left", fill="both", expand=True)
inner2 = tk.Frame(lcanvas, bg=T["panel"])
lcanvas.create_window((0, 0), window=inner2, anchor="nw")


def wheel2(ev):
    step = -3 if (ev.delta > 0 if getattr(ev, "delta", 0) else ev.num == 4) else 3
    lcanvas.yview_scroll(step, "units")
    sb2.show()
    sb2.schedule_hide()


for wd in (lst, lcanvas, inner2):
    wd.bind("<MouseWheel>", wheel2)
    wd.bind("<Button-4>", wheel2)
    wd.bind("<Button-5>", wheel2)
    wd.bind("<Enter>", lambda ev: (sb2.show(), sb2.schedule_hide()))

right2 = tk.Frame(body_layout, bg=T["panel"],
                  highlightthickness=1, highlightbackground=T["border"])
right2.pack(side="left", fill="both", expand=True)
section(right2, "② 保存当前布局", top=12)
tk.Label(right2, text=f"当前有 {current_count} 个可管理窗口，保存后可随时一键恢复",
         bg=T["panel"], fg=T["sub"], font=tf(9)).pack(anchor="w", padx=16)
entry_row = tk.Frame(right2, bg=T["panel"])
entry_row.pack(fill="x", padx=16, pady=(8, 2))
name_var = tk.StringVar(value=f"布局 {len(items) + 1}")
name_entry = tk.Entry(entry_row, textvariable=name_var, font=tf(9), relief="flat",
                      bg=T["muted"], fg=T["sub"], insertbackground=T["sub"])
name_entry.pack(side="left", fill="x", expand=True, ipady=7)


def action_btn(parent, text, command, primary=False):
    # 非主按钮文字与左上角未选中标签同色（T["sub"]），不要黑色，避免两页不统一。
    return tk.Button(parent, text=text, command=command, font=tf(10, primary),
                     bg=T["accent"] if primary else T["muted"],
                     fg=T["on"] if primary else T["sub"],
                     activebackground=T["accent_hover"] if primary else T["border"],
                     activeforeground=T["on"] if primary else T["sub"],
                     relief="flat", bd=0, padx=12, pady=5)


save_btn = action_btn(entry_row, "保存", lambda: do_save())
save_btn.pack(side="right", padx=(8, 0))

section(right2, "③ 布局操作", top=14)
ops = tk.Frame(right2, bg=T["panel"])
ops.pack(fill="x", padx=16)
load_btn = action_btn(ops, "加载布局", lambda: do_load(), primary=True)
load_btn.pack(side="left", padx=(0, 6))
rename_btn = action_btn(ops, "重命名", lambda: do_rename())
rename_btn.pack(side="left", padx=6)
del_btn = action_btn(ops, "删除布局", lambda: do_delete())
del_btn.pack(side="left", padx=6)

section(right2, "④ 选中布局预览", top=14)
PV2_W, PV2_H = 380, 196
preview = tk.Label(right2, bg=T["panel"], width=PV2_W, height=PV2_H)
preview.pack(padx=16)
PREVIEW = [None]

# 弹簧：把「操作提示」推到右栏底部，和左栏列表底部对齐，
# 与「窗口排列」页右栏（卡片区 expand 撑满）保持同样的高度分布。
tk.Frame(right2, bg=T["panel"]).pack(fill="both", expand=True)

tips2 = tk.Frame(right2, bg=T["soft"])
tips2.pack(fill="x", padx=16, pady=(10, 10))
tk.Label(tips2, text="操作提示", bg=T["soft"], fg=T["accent"],
         font=tf(10, True)).pack(anchor="w", padx=10, pady=(9, 4))
tk.Label(tips2,
         text="· 保存：记录当前所有窗口的位置与大小\n"
              "· 加载：按当前屏幕比例恢复窗口落位\n"
              "· 删除：需连续点击两次确认，避免误删",
         bg=T["soft"], fg=T["sub"], font=tf(9), justify="left",
         wraplength=390).pack(anchor="w", padx=10, pady=(0, 9))


def render_preview(item):
    s = 3
    im = Image.new("RGBA", (PV2_W * s, PV2_H * s), T["panel"])
    d = ImageDraw.Draw(im)
    p = lambda v: int(v * s)
    bx0, by0, bx1, by1 = p(12), p(10), p(PV2_W - 12), p(PV2_H - 10)
    rounded(d, (bx0, by0, bx1, by1), p(10),
            fill=T["tile"], outline=T["border"], width=p(1))
    if not item or not item.get("rects"):
        d.text(((bx0 + bx1) / 2, (by0 + by1) / 2), "暂无几何预览",
               font=pf(12, False, s), fill=T["sub"], anchor="mm")
    else:
        bw, bh = bx1 - bx0, by1 - by0
        gap = p(3)
        for k, chunk in enumerate(item["rects"].split(";")):
            nums = chunk.split(",")
            if len(nums) != 4:
                continue
            try:
                x, y, w, h = (int(v) for v in nums)
            except ValueError:
                continue
            x, y = min(10000, max(0, x)), min(10000, max(0, y))
            w = min(10000 - x, max(0, w))
            h = min(10000 - y, max(0, h))
            if w <= 0 or h <= 0:
                continue
            x0 = bx0 + int(x / 10000 * bw) + gap
            y0 = by0 + int(y / 10000 * bh) + gap
            x1 = bx0 + int((x + w) / 10000 * bw) - gap
            y1 = by0 + int((y + h) / 10000 * bh) - gap
            if x1 <= x0 or y1 <= y0:
                continue
            rounded(d, (x0, y0, x1, y1), p(6), fill=T["accent"],
                    outline=T["accent"], width=p(2))
            d.text(((x0 + x1) / 2, (y0 + y1) / 2), str(k + 1),
                   font=pf(11, True, s), fill=T["on"], anchor="mm")
    return im.resize((PV2_W, PV2_H), RESAMPLE)


def refresh_preview():
    item = items[sel_idx] if 0 <= sel_idx < len(items) else None
    PREVIEW[0] = ImageTk.PhotoImage(render_preview(item))
    preview.config(image=PREVIEW[0])


def bind_row(wd, cb):
    wd.bind("<Button-1>", cb)
    wd.bind("<Double-Button-1>", lambda ev: do_load())
    for ch in wd.winfo_children():
        bind_row(ch, cb)


def refresh_list(with_preview=True):
    for wd in list(inner2.winfo_children()):
        wd.destroy()
    count_badge.config(text=f"{len(items)} 个")
    for i, item in enumerate(items):
        on = i == sel_idx
        bg = T["soft"] if on else T["panel"]
        row = tk.Frame(inner2, bg=bg)
        row.pack(fill="x", padx=6, pady=4)
        tk.Frame(row, bg=T["accent"] if on else T["panel"], width=3).pack(side="left", fill="y")
        txt = tk.Frame(row, bg=bg)
        txt.pack(side="left", fill="x", padx=10, pady=8)
        tk.Label(txt, text=item["name"], bg=bg,
                 fg=T["accent"] if on else T["sub"],
                 font=tf(10, True)).pack(anchor="w")
        tk.Label(txt, text=f'{item["created"]}  ·  {item["count"]} 个窗口',
                 bg=bg, fg=T["sub"], font=tf(9)).pack(anchor="w", pady=(3, 0))
        bind_row(row, lambda ev, idx=i: select(idx))
    if not items:
        tk.Label(inner2, text="暂无保存的布局\n在右侧输入名称，点击「保存」即可记录当前布局",
                 bg=T["panel"], fg=T["sub"], font=tf(9),
                 justify="center").pack(pady=50)
    inner2.update_idletasks()
    iw = max(lcanvas.winfo_width() - 4, inner2.winfo_width())
    lcanvas.configure(scrollregion=(0, 0, iw, inner2.winfo_height()))
    sb2.redraw()
    state = "normal" if items else "disabled"
    for b in (load_btn, rename_btn, del_btn):
        b.config(state=state,
                 fg=T["on"] if (state == "normal" and b is load_btn) else T["sub"])
    if with_preview:
        refresh_preview()


def select(idx):
    global sel_idx
    sel_idx = idx
    refresh_list()


footer_layout = tk.Frame(root, bg=T["panel"], height=58)
footer_layout.pack_propagate(False)
status_layout = tk.Label(footer_layout, text="", bg=T["panel"], fg=T["sub"],
                         font=tf(9), anchor="w")
status_layout.pack(side="left", padx=20, fill="x", expand=True)
# 与窗口排列页一致：最右「关闭」按钮右缘对齐内容卡片右边框（距窗口右边 16px）。
# 原来是 padx=20，按钮比内容卡片右缘缩进来一截（截图里明显偏左）。
action_btn(footer_layout, "关闭", lambda: close_panel()).pack(
    side="right", padx=(4, 16), pady=10)


def lstatus(msg, warn=False):
    status_layout.config(text=msg, fg=T["warn"] if warn else T["sub"])


def do_save():
    name = name_var.get().strip()
    if not name:
        lstatus("请先输入布局名称", True)
        return
    ok, msg = send("save", name)
    if ok:
        # Rust 回传的是「提示\t窗口数\t相对几何」，必须用保存瞬间实测数据，
        # 不能再用面板启动时传入的 current_count/current_rects（面板开着时
        # 用户可能已经应用过布局，旧预览会与真正写进配置里的几何不一致）。
        parts = msg.split("\t", 2)
        shown = parts[0] if parts and parts[0] else "已保存"
        count = current_count
        rects = current_rects
        if len(parts) >= 2:
            try:
                count = int(parts[1])
            except ValueError:
                pass
        if len(parts) >= 3:
            rects = parts[2]
        items.append(dict(name=name, created="刚刚", count=count, rects=rects))
        select(len(items) - 1)
        name_var.set(f"布局 {len(items) + 1}")
        name_entry.focus_set()
    else:
        refresh_list()
    lstatus(msg.split("\t", 1)[0] if msg else "", not ok)


def do_load():
    if not (0 <= sel_idx < len(items)):
        return
    ok, msg = send("load", str(sel_idx))
    lstatus(msg, not ok)
    if ok:
        # 和应用布局一致：加载后只把管理面板收起来，不退出程序。
        _schedule_minimize()


def do_rename():
    if not (0 <= sel_idx < len(items)):
        return
    result = ask_name("重命名布局", items[sel_idx]["name"])
    if result is None:
        return
    result = result.strip()
    if not result or result == items[sel_idx]["name"]:
        return
    ok, msg = send("rename", str(sel_idx), result)
    if ok:
        items[sel_idx]["name"] = result
        refresh_list()
    lstatus(msg, not ok)


del_armed = [False, -1]


def disarm_delete():
    del_armed[0], del_armed[1] = False, -1
    del_btn.config(text="删除布局")


def do_delete():
    if not (0 <= sel_idx < len(items)):
        return
    if not del_armed[0] or del_armed[1] != sel_idx:
        del_armed[0], del_armed[1] = True, sel_idx
        del_btn.config(text="再点一次确认")
        lstatus("再点一次「删除布局」确认删除，4 秒内有效", True)
        root.after(4000, disarm_delete)
        return
    name = items[sel_idx]["name"]
    ok, msg = send("delete", str(sel_idx))
    disarm_delete()
    if ok:
        items.pop(sel_idx)
        select(min(sel_idx, len(items) - 1))
        name_var.set(f"布局 {len(items) + 1}")
    else:
        refresh_list()
    lstatus(msg or f"已删除「{name}」", not ok)


def ask_name(title, initial):
    dlg = tk.Toplevel(root)
    dlg.withdraw()
    dlg.title("轻格分窗 - " + title)
    w, h = 420, 186
    dlg.geometry(f"{w}x{h}+{root.winfo_x() + (WIN_W - w) // 2}+"
                 f"{root.winfo_y() + (WIN_H - h) // 2}")
    dlg.resizable(False, False)
    dlg.configure(bg=T["bg"])
    dlg.attributes("-topmost", True)
    dlg.transient(root)
    dlg.grab_set()
    dh = tk.Frame(dlg, bg=T["panel"], height=52)
    dh.pack(fill="x", side="top")
    dh.pack_propagate(False)
    tk.Label(dh, text=title, bg=T["panel"], fg=T["sub"],
             font=tf(11, True)).pack(side="left", padx=20, pady=14)
    db = tk.Frame(dlg, bg=T["panel"])
    db.pack(fill="both", expand=True, padx=16, pady=(10, 6))
    var = tk.StringVar(value=initial)
    ent = tk.Entry(db, textvariable=var, font=tf(9), relief="flat",
                   bg=T["muted"], fg=T["sub"], insertbackground=T["sub"])
    ent.pack(fill="x", ipady=7, padx=6)
    df = tk.Frame(dlg, bg=T["panel"], height=48)
    df.pack(fill="x", side="bottom")
    df.pack_propagate(False)
    answer = [None]

    def finish(v):
        answer[0] = v
        dlg.destroy()

    tk.Button(df, text="取消", command=lambda: finish(None), font=tf(9),
              bg=T["muted"], fg=T["sub"], activeforeground=T["sub"],
              relief="flat", bd=0).pack(side="right", padx=16, pady=10)
    tk.Button(df, text="确定", command=lambda: finish(var.get().strip()),
              font=tf(9, True), bg=T["accent"], fg=T["on"],
              activebackground=T["accent_hover"], activeforeground=T["on"],
              relief="flat", bd=0).pack(
                  side="right", padx=4, pady=10)
    ent.bind("<Return>", lambda ev: finish(var.get().strip()))
    dlg.bind("<Escape>", lambda ev: finish(None))
    dlg.protocol("WM_DELETE_WINDOW", lambda: finish(None))
    dlg.deiconify()
    ent.focus_set()
    ent.select_range(0, "end")
    root.wait_window(dlg)
    return answer[0]


# ================= 快捷键标签页 =================
# 面板入口键默认从 Rust 读回（下面 refresh_hotkeys()）。改键走 set_hotkey
# 协议：Rust 校验、写盘、热重载 XGrabKey，无需重启程序。
body_hk = tk.Frame(content, bg=T["bg"])
footer_hk = tk.Frame(root, bg=T["panel"], height=58)
footer_hk.pack_propagate(False)
status_hk = tk.Label(footer_hk, text="", bg=T["panel"], fg=T["sub"],
                     font=tf(9), anchor="w")
status_hk.pack(side="left", padx=20, fill="x", expand=True)

hk_left = tk.Frame(body_hk, bg=T["panel"], width=320,
                   highlightthickness=1, highlightbackground=T["border"])
hk_left.pack(side="left", fill="both", padx=(0, 14))
hk_left.pack_propagate(False)
hk_right = tk.Frame(body_hk, bg=T["panel"],
                    highlightthickness=1, highlightbackground=T["border"])
hk_right.pack(side="left", fill="both", expand=True)

section(hk_left, "① 可修改的快捷键", top=12)
tk.Label(hk_left,
         text="点击「修改」，然后按下新的组合键。\n"
              "必须带 Ctrl / Alt / Shift / Super\n"
              "中的至少一个，再按一个字母、数字或 F 键。",
         bg=T["panel"], fg=T["sub"], font=tf(9), justify="left",
         anchor="w").pack(anchor="w", padx=16, pady=(0, 10))

HK_LABELS = [
    ("panel_window", "打开窗口排列"),
    ("panel_layout", "打开布局管理"),
]
hk_bindings = {name: "" for name, _ in HK_LABELS}
hk_btns = {}
capturing = [None]  # 正在捕获的 name；None 表示未在捕获


def _fmt_binding(ev):
    """把 Tk 按键事件转成 Rust parse_binding 认识的组合串。"""
    state = ev.state
    mods = []
    # X11 掩码：Shift=1 Control=4 Mod1(Alt)=8 Mod4(Super)=64
    if state & 0x01:
        mods.append("Shift")
    if state & 0x04:
        mods.append("Ctrl")
    if state & 0x08:
        mods.append("Alt")
    if state & 0x40:
        mods.append("Super")
    key = ev.keysym
    # 数字小键盘 KP_1 → 1；大小写字母统一小写；功能键保留 F1 形式。
    if key.startswith("KP_") and key[3:].isdigit():
        key = key[3:]
    elif len(key) == 1 and key.isalnum():
        # 单个字母或数字都合法（Alt+1 / Ctrl+9 等）。
        key = key.lower()
    elif len(key) > 1 and key.upper().startswith("F") and key[1:].isdigit():
        key = key.upper()
    elif key in ("space", "Return", "Tab", "Escape"):
        pass
    else:
        # 修饰键本身（Shift_L/Alt_L/Control_L/Super_L）不算主键，忽略。
        return None
    return "+".join(mods + [key]) if mods else None


def _finish_capture(name, binding, cancel=False):
    capturing[0] = None
    hk_btns[name].config(text="修改", bg=T["soft"], fg=T["accent"])
    if cancel:
        hk_status(f"已取消修改「{dict(HK_LABELS)[name]}」")
        return
    ok, msg = send("set_hotkey", name, binding)
    hk_status(msg if msg else ("已修改" if ok else "修改失败"), not ok)
    if ok:
        hk_bindings[name] = binding
        hk_values[name].config(text=binding)
        # 与 Rust 重新握手一次，确保显示的是已落盘的真实值。
        refresh_hotkeys()


def _on_capture_key(ev):
    name = capturing[0]
    if name is None:
        return
    if ev.keysym in ("Escape",):
        _finish_capture(name, None, cancel=True)
        return
    binding = _fmt_binding(ev)
    if binding is None:
        hk_status("请按 Ctrl / Alt / Shift / Super + 字母、数字或 F 键；Esc 取消", True)
        return
    _finish_capture(name, binding)


def _begin_capture(name):
    if capturing[0] == name:
        return
    if capturing[0] is not None:
        _finish_capture(capturing[0], None, cancel=True)
    capturing[0] = name
    hk_btns[name].config(text="请按键…", bg=T["accent"], fg=T["on"])
    hk_status(f"请为「{dict(HK_LABELS)[name]}」按下新组合键，按 Esc 取消")


hk_values = {}
for _name, _label in HK_LABELS:
    _row = tk.Frame(hk_left, bg=T["panel"])
    _row.pack(fill="x", padx=16, pady=6)
    _card = tk.Frame(_row, bg=T["soft"],
                     highlightthickness=1, highlightbackground=T["border"])
    _card.pack(fill="x")
    # 标题独占上行；「修改」放到下一行，与左侧 Alt+1/Alt+2 同一水平对齐。
    _hdr = tk.Frame(_card, bg=T["soft"])
    _hdr.pack(fill="x", padx=(12, 10), pady=(6, 0))
    tk.Label(_hdr, text=_label, bg=T["soft"], fg=T["sub"],
             font=tf(9)).pack(side="left")
    _lower = tk.Frame(_card, bg=T["soft"])
    _lower.pack(fill="x", padx=(12, 10), pady=(2, 8))
    _val = tk.Label(_lower, text="", bg=T["soft"], fg=T["accent"], font=tf(11, True))
    _val.pack(side="left")
    _btn = tk.Button(_lower, text="修改", command=lambda n=_name: _begin_capture(n),
                     font=tf(9), bg=T["muted"], fg=T["accent"],
                     activebackground=T["border"], activeforeground=T["sub"],
                     relief="flat", bd=0, padx=10, pady=2)
    _btn.pack(side="right")
    hk_values[_name] = _val
    hk_btns[_name] = _btn

section(hk_right, "② 说明", top=12)
hk_notes = tk.Label(hk_right,
         text="· 修改后立即生效，无需重启程序。\n"
              "· 配置保存在 ~/.config/lithezones/config.json。\n"
              "· 系统占用了 Super+1~9（切换工作区）和 Super+Shift+1~9（移动到工作区），请避开。\n"
              "· 若组合键已被别的程序抢先占用，本程序无法抢到，按键将不生效，请换一个组合。\n"
              "· 恢复默认会把两个入口键重置为 Alt+1 / Alt+2。",
         bg=T["panel"], fg=T["sub"], font=tf(9), justify="left",
         wraplength=660, anchor="w")
hk_notes.pack(anchor="w", padx=16, pady=(0, 12))


def _hk_notes_wrap(event):
    """说明文字跟随右栏宽度，避免硬换行把每行挤得太短或窄屏溢出。"""
    if event.width > 120:
        hk_notes.config(wraplength=max(240, event.width - 44))


hk_right.bind("<Configure>", _hk_notes_wrap, add="+")


def hk_status(msg, warn=False):
    status_hk.config(text=msg, fg=T["warn"] if warn else T["sub"])


def refresh_hotkeys():
    ok, msg = send("hotkeys")
    if not ok:
        hk_status(msg or "读取快捷键失败", True)
        return
    for item in msg.split(US):
        if "=" not in item:
            continue
        name, binding = item.split("=", 1)
        if name in hk_bindings:
            hk_bindings[name] = binding
            hk_values[name].config(text=binding)


def restore_hotkeys():
    for name, default in (("panel_window", "Alt+1"), ("panel_layout", "Alt+2")):
        if hk_bindings.get(name) == default:
            continue
        ok, msg = send("set_hotkey", name, default)
        if not ok:
            hk_status(msg or "恢复默认失败", True)
            return
    hk_status("已恢复默认快捷键：Alt+1 打开窗口排列、Alt+2 打开布局管理")
    refresh_hotkeys()


tk.Button(footer_hk, text="关闭", command=lambda: close_panel(),
          font=tf(10), bg=T["muted"], fg=T["sub"],
          activebackground=T["border"], activeforeground=T["sub"],
          relief="flat", bd=0, padx=8, pady=4).pack(
              side="right", padx=(4, 16), pady=10)
tk.Button(footer_hk, text="恢复默认", command=lambda: restore_hotkeys(),
          font=tf(10), bg=T["muted"], fg=T["sub"],
          activebackground=T["border"], activeforeground=T["sub"],
          relief="flat", bd=0, padx=8, pady=4).pack(
              side="right", padx=4, pady=10)

root.bind("<KeyPress>", _on_capture_key)


# ================= 主题标签页 =================
body_theme = tk.Frame(content, bg=T["bg"])
footer_theme = tk.Frame(root, bg=T["panel"], height=58)
footer_theme.pack_propagate(False)
status_theme = tk.Label(footer_theme, text="", bg=T["panel"], fg=T["sub"],
                        font=tf(9), anchor="w")
status_theme.pack(side="left", padx=20, fill="x", expand=True)

theme_left = tk.Frame(body_theme, bg=T["panel"], width=320,
                      highlightthickness=1, highlightbackground=T["border"])
theme_left.pack(side="left", fill="both", padx=(0, 14))
theme_left.pack_propagate(False)
theme_right = tk.Frame(body_theme, bg=T["panel"],
                       highlightthickness=1, highlightbackground=T["border"])
theme_right.pack(side="left", fill="both", expand=True)

section(theme_left, "① 主题颜色", top=12)
tk.Label(theme_left, text="选一个喜欢的强调色，面板会立即换色。",
         bg=T["panel"], fg=T["sub"], font=tf(9), justify="left",
         anchor="w").pack(anchor="w", padx=16, pady=(0, 8))

accent_btns = {}
for _key, (_label, _color, _hover) in ACCENTS.items():
    def _choose_accent(k=_key):
        global accent_name
        accent_name = k
        apply_accent(k)
        theme_status(f"主题颜色已切换为「{ACCENTS[k][0]}」，下次打开面板会沿用")

    _btn = tk.Button(theme_left, text=_label, command=_choose_accent,
                     font=tf(10), bg=_color, fg=T["on"],
                     activebackground=_hover, activeforeground=T["on"],
                     relief="flat", bd=0, padx=10, pady=6)
    _btn.pack(fill="x", padx=16, pady=4)
    accent_btns[_key] = _btn

section(theme_right, "② 说明", top=12)
tk.Label(theme_right,
         text="· 主题颜色只影响管理面板：按钮、选中卡片、高亮区块和编号。\n"
              "· 选择会立即生效并写入 ~/.config/lithezones/panel-prefs.json，下次打开自动沿用。\n"
              "· 深浅模式仍由 config.json 的 theme 控制：light / dark / auto。\n"
              "· 托盘图标和启动器图标仍使用系统图标主题，不随这里改变。",
         bg=T["panel"], fg=T["sub"], font=tf(9), justify="left",
         wraplength=660, anchor="w").pack(anchor="w", padx=16, pady=(0, 12))


def theme_status(msg, warn=False):
    status_theme.config(text=msg, fg=T["warn"] if warn else T["sub"])


def _recolor_widget(widget, mapping):
    """把旧色值替换成新色值，遍历隐藏页和 Canvas 图元。"""
    try:
        options = widget.keys()
    except Exception:
        options = []
    changed = {}
    for opt in ("bg", "background", "fg", "foreground", "activebackground",
                "activeforeground", "highlightbackground", "selectbackground",
                "selectforeground", "readonlybackground", "disabledforeground"):
        if opt not in options:
            continue
        try:
            val = widget.cget(opt)
        except Exception:
            continue
        if val in mapping:
            changed[opt] = mapping[val]
    if changed:
        try:
            widget.config(**changed)
        except Exception:
            pass
    if isinstance(widget, tk.Canvas):
        for item in widget.find_all():
            item_changed = {}
            for opt in ("fill", "outline", "activefill", "disabledfill",
                        "activeoutline", "disabledoutline"):
                try:
                    val = widget.itemcget(item, opt)
                except Exception:
                    continue
                if val in mapping:
                    item_changed[opt] = mapping[val]
            if item_changed:
                try:
                    widget.itemconfig(item, **item_changed)
                except Exception:
                    pass
    for child in widget.winfo_children():
        _recolor_widget(child, mapping)


def apply_accent(key):
    """实时换色：更新 T、重刷控件、重绘 logo 并写回偏好。"""
    global logo
    old = dict(T)
    T.update(palette_for(theme_name, key))
    mapping = {old[k]: T[k] for k in old if k in T and old[k] != T[k]}
    _recolor_widget(root, mapping)
    logo = render_logo()
    logo_lbl.config(image=logo)
    for k, btn in accent_btns.items():
        label, color, hover = ACCENTS[k]
        btn.config(text=("✓ " if k == key else "") + label,
                   bg=color, fg=T["on"],
                   activebackground=hover, activeforeground=T["on"])
    # PhotoImage 是渲染时烘死的，换色后必须清缓存重画，否则缩略图仍是旧色。
    _card_cache.clear()
    _tile_photo_cache.clear()
    if "apply_grid" in globals():
        apply_grid()
    if "refresh_preview" in globals():
        refresh_preview()
    _save_panel_prefs()


def restore_accent():
    apply_accent("blue")
    theme_status("已恢复默认主题颜色：蔚蓝")


for _k, _btn in accent_btns.items():
    _btn.config(text=("✓ " if _k == accent_name else "") + ACCENTS[_k][0])
theme_status(f"当前主题颜色：{ACCENTS[accent_name][0]}")

tk.Button(footer_theme, text="关闭", command=lambda: close_panel(),
          font=tf(10), bg=T["muted"], fg=T["sub"],
          activebackground=T["border"], activeforeground=T["sub"],
          relief="flat", bd=0, padx=8, pady=4).pack(
              side="right", padx=(4, 16), pady=10)
tk.Button(footer_theme, text="恢复默认", command=restore_accent,
          font=tf(10), bg=T["muted"], fg=T["sub"],
          activebackground=T["border"], activeforeground=T["sub"],
          relief="flat", bd=0, padx=8, pady=4).pack(
              side="right", padx=4, pady=10)


# ================= 标签切换 =================
active_tab = [None]


def switch_tab(key):
    # 离开快捷键页时若正在捕获，先取消，避免全局按键监听留着半截状态。
    if key != "hotkeys" and capturing[0] is not None:
        _finish_capture(capturing[0], None, cancel=True)
    active_tab[0] = key
    for wd in (body_win, body_layout, body_hk, body_theme):
        wd.pack_forget()
    for wd in (footer_win, footer_layout, footer_hk, footer_theme):
        wd.place_forget()
    if key == "window":
        body_win.pack(fill="both", expand=True, padx=16, pady=(12, 8))
        footer_win.place(x=0, y=WIN_H - FOOTER_H, width=WIN_W, height=FOOTER_H)
        subtitle.config(text="左侧选布局、右侧选窗口 · 高亮区块即窗口的最终落位")
    elif key == "layout":
        body_layout.pack(fill="both", expand=True, padx=16, pady=(12, 8))
        footer_layout.place(x=0, y=WIN_H - FOOTER_H, width=WIN_W, height=FOOTER_H)
        subtitle.config(text="保存、加载、删除已保存的窗口布局 · 所有操作都在这个窗口完成")
    elif key == "theme":
        body_theme.pack(fill="both", expand=True, padx=16, pady=(12, 8))
        footer_theme.place(x=0, y=WIN_H - FOOTER_H, width=WIN_W, height=FOOTER_H)
        subtitle.config(text="选择面板主题颜色 · 改完立即生效并自动记住")
    else:
        body_hk.pack(fill="both", expand=True, padx=16, pady=(12, 8))
        footer_hk.place(x=0, y=WIN_H - FOOTER_H, width=WIN_W, height=FOOTER_H)
        subtitle.config(text="修改打开面板的快捷键 · 改完立即生效，无需重启")
        refresh_hotkeys()
    active_footer = {"window": footer_win, "layout": footer_layout,
                     "theme": footer_theme}.get(key, footer_hk)
    active_footer.configure(height=58)
    active_footer.pack_propagate(False)
    for k, lbl in tab_btns.items():
        lbl.config(fg=T["accent"] if k == key else T["sub"],
                   font=tf(10, k == key))
    root.update_idletasks()
    if key == "layout":
        # 首次切到布局页时补画预览（启动若停在窗口排列页，预览会延迟到这里）。
        refresh_preview()


def _schedule_minimize():
    """排一次带代次的延迟最小化；期间若有重新打开请求就作废。"""
    _minimize_gen[0] += 1
    gen = _minimize_gen[0]
    root.after(150, lambda: _delayed_minimize(gen))


def _delayed_minimize(gen):
    if _closing[0] or gen != _minimize_gen[0]:
        return
    # 已经最小化（用户自己收起或 Rust 兜底已处理）就不要再发一次命令，
    # 否则会在用户刚从任务栏恢复时把面板重新按下去。
    try:
        if root.state() != "normal":
            return
    except Exception:
        pass
    minimize_panel()


def minimize_panel():
    """应用/加载布局后收起面板，但保留任务栏/启动器图标。

    【0.5.52 机制改动】彻底不再用 Tk 的 root.iconify()。实测 deepin V23
    (dde-kwin) 下 Tk iconify 与 WM 状态机不同步：Tk 内部仍认定窗口是 iconic，
    WM 从 dock 恢复窗口后，Tk 的状态机又在约 50ms 内把窗口收回 HIDDEN，
    表现为「点 dock 图标弹出又立刻缩回」。改用 Rust 侧的 ICCCM
    WM_CHANGE_STATE(IconicState) 最小化（与 xdotool windowminimize 等价）：
    实测 dock 恢复后稳定保持 FOCUSED，不再被收回。
    只发 minimize 命令给 Rust 由其统一处理，Tk 侧保持 normal 状态不干预。
    """
    try:
        send("minimize")
    except Exception:
        pass


def close_panel():
    _closing[0] = True
    try:
        print("close", flush=True)
    except Exception:
        pass
    root.destroy()


def _refresh_worker():
    """面板存活期间定时拉取实时窗口并抓缩略图，不阻塞 Tk 主线程。"""
    while not _closing[0]:
        time.sleep(REFRESH_INTERVAL)
        if _closing[0] or _refresh_busy[0]:
            continue
        _refresh_busy[0] = True
        try:
            ok, msg = send("windows")
            if not ok:
                continue
            new_entries = parse_windows_reply(msg)
            if not new_entries:
                continue
            sig = entries_signature(new_entries)
            thumbs = grab_live_thumbs(new_entries)
            _refresh_q.put((new_entries, thumbs, sig != _entries_sig))
        except Exception:
            pass
        finally:
            _refresh_busy[0] = False


def _poll_refresh():
    if _closing[0]:
        return
    while True:
        try:
            item = _refresh_q.get_nowait()
        except queue.Empty:
            break
        try:
            apply_live_refresh(*item)
        except Exception:
            pass
    root.after(200, _poll_refresh)


root.protocol("WM_DELETE_WINDOW", close_panel)
root.bind("<Escape>", lambda ev: close_panel())
root.bind("<Return>", lambda ev: do_apply() if active_tab[0] == "window" else None)

def _poll_ctrl():
    target = None
    while True:
        try:
            key = _ctrl_q.get_nowait()
        except queue.Empty:
            break
        if key in ("window", "layout"):
            target = key
    if target:
        # 这是用户明确要重新打开：作废尚未触发的延迟最小化，防止刚弹出
        # 又被上一次 apply/load 的定时器收起。
        _minimize_gen[0] += 1
        # 面板可能是 apply/load 后主动最小化的，iconify 状态下 lift 不会恢复显示。
        # 托盘/启动器再次打开时必须先 deiconify。
        root.deiconify()
        switch_tab(target)
        root.lift()
        root.focus_force()
    root.after(120, _poll_ctrl)


threading.Thread(target=_stdin_loop, daemon=True).start()
refresh_list(with_preview=(initial_tab == "layout"))
switch_tab(initial_tab)
root.deiconify()
root.update_idletasks()
# 用户上次保存的模式/行列在启动时生效，按钮状态要与之一致。
set_grid_enabled(not list_mode)
apply_grid()
_entries_sig = entries_signature(entries)
# 面板打开期间（包括最小化但任务栏图标还在时）持续刷新窗口列表/缩略图。
threading.Thread(target=_refresh_worker, daemon=True).start()
root.after(200, _poll_refresh)
root.after(80, sync_scroll)
root.after(120, _poll_ctrl)

# 竞态兜底只在启动头 1.2 秒内做一次：deepin/kwin 下 withdraw→deiconify
# 偶尔被记成"激活隐藏窗口"，需要强制显示。但绝不能长期反复 deiconify，
# 否则用户手动最小化后会被拉回来（"点最小化自动弹出"）。
_BOOT_AT = time.monotonic()


def _ensure_visible(n=0):
    if time.monotonic() - _BOOT_AT < 1.2 and root.state() == "withdrawn":
        try:
            root.deiconify()
            root.lift()
            root.focus_force()
        except Exception:
            pass
    if n < 3:
        root.after(250 * (n + 1), lambda: _ensure_visible(n + 1))


root.after(150, lambda: _ensure_visible(0))
root.mainloop()
