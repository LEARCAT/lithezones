#!/usr/bin/env python3
"""轻格分窗 - 布局管理：保存 / 加载 / 删除 / 重命名，统一窗口风格。

Rust 通过 --item=名称<US>时间<US>窗口数<US>几何(万分比) 传入已有布局；
窗口内的每次操作输出一行命令，并从 stdin 读取 "ok\\t消息" / "err\\t消息"。
"""
import sys
import subprocess
import tkinter as tk
from PIL import Image, ImageDraw, ImageFont, ImageTk

RESAMPLE = getattr(Image, "LANCZOS", getattr(Image, "ANTIALIAS", 3))
US = "\x1f"


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


# ---------- 主题：与窗口排列完全一致 ----------
THEMES = {
    "light": dict(bg="#f5f6f8", panel="#ffffff", text="#1f2329", sub="#646a73",
                  border="#dfe2e8", accent="#2d7ff9", soft="#eaf2ff", muted="#eceef1",
                  tile="#f7f9fc", scroll="#a9b2be", on="#ffffff", warn="#d93026"),
    "dark": dict(bg="#1e2023", panel="#2b2e33", text="#e8eaed", sub="#9aa0a6",
                 border="#41454b", accent="#5b9bff", soft="#263b5c", muted="#3a3e44",
                 tile="#31343a", scroll="#6b7280", on="#ffffff", warn="#ff6b61"),
}


def rounded(d, box, r, fill=None, outline=None, width=1):
    d.rounded_rectangle(box, radius=r, fill=fill, outline=outline, width=width)


# ---------- 参数 ----------
theme_name = "light"
version = "0.4.6"
current_count = 0
current_rects = ""
items = []

for arg in sys.argv[1:]:
    if arg.startswith("--theme="):
        theme_name = arg.split("=", 1)[1]
    elif arg.startswith("--version="):
        version = arg.split("=", 1)[1]
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

T = THEMES.get(theme_name, THEMES["light"])
sel_idx = 0 if items else -1


def token(s):
    """协议字段不能含制表符/换行。"""
    return str(s).replace("\t", " ").replace("\n", " ").replace("\r", " ")


def send(*fields):
    """向 Rust 主程序发一条命令并等待应答，返回 (成功, 消息)。"""
    print("\t".join(token(f) for f in fields), flush=True)
    reply = sys.stdin.readline()
    if not reply:
        return False, "与后台程序通信中断"
    parts = reply.rstrip("\r\n").split("\t", 1)
    return parts[0] == "ok", parts[1] if len(parts) > 1 else ""


# ---------- 细圆角滚动条（自动隐藏，与窗口排列一致） ----------
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
        self.drag_off = ev.y - y if y <= ev.y <= y + th else th / 2
        if not (y <= ev.y <= y + th):
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
        self.view.yview_moveto(min(1, max(0, (ev.y - self.drag_off) / max(1, h - th))))
        self.redraw(True)

    def on_release(self, ev):
        self.dragging = False
        self.redraw(False)


# ---------- 主窗口 ----------
root = tk.Tk()
root.withdraw()
SW, SH = root.winfo_screenwidth(), root.winfo_screenheight()
root.title("轻格分窗 - 布局管理")
WIN_W, WIN_H = 1120, min(SH - 80, 700)
root.geometry(f"{WIN_W}x{WIN_H}+{(SW - WIN_W) // 2}+{(SH - WIN_H) // 2}")
root.resizable(False, False)
root.configure(bg=T["bg"])
root.attributes("-topmost", True)
root.lift()
root.focus_force()

hdr = tk.Frame(root, bg=T["panel"], height=70)
hdr.pack(fill="x", side="top")
hdr.pack_propagate(False)
logo_im = Image.new("RGBA", (32, 32), (0, 0, 0, 0))
logo_d = ImageDraw.Draw(logo_im)
for lx, ly in ((2, 2), (18, 2), (2, 18), (18, 18)):
    rounded(logo_d, (lx, ly, lx + 12, ly + 12), 3, fill=T["accent"])
LOGO = ImageTk.PhotoImage(logo_im)
tk.Label(hdr, image=LOGO, bg=T["panel"]).pack(side="left", padx=(22, 12))
htext = tk.Frame(hdr, bg=T["panel"])
htext.pack(side="left", pady=12)
tk.Label(htext, text="布局管理", bg=T["panel"], fg=T["text"],
         font=tf(16, True)).pack(anchor="w")
tk.Label(htext, text="保存、加载、删除已保存的窗口布局 · 所有操作都在这个窗口完成",
         bg=T["panel"], fg=T["sub"], font=tf(10)).pack(anchor="w", pady=(3, 0))
tk.Label(hdr, text=f"LitheZones v{version}", bg=T["panel"], fg=T["sub"],
         font=tf(9)).pack(side="right", padx=22)

body = tk.Frame(root, bg=T["bg"])
body.pack(fill="both", expand=True, padx=16, pady=(12, 8))


def section(parent, text, top=12):
    tk.Label(parent, text=text, bg=T["panel"], fg=T["accent"],
             font=tf(11, True)).pack(anchor="w", padx=16, pady=(top, 6))


# ---- 左侧：布局列表 ----
left = tk.Frame(body, bg=T["panel"], width=500,
                highlightthickness=1, highlightbackground=T["border"])
left.pack(side="left", fill="both", padx=(0, 14))
left.pack_propagate(False)
lhead = tk.Frame(left, bg=T["panel"])
lhead.pack(fill="x", pady=(12, 2))
tk.Label(lhead, text="① 已保存布局", bg=T["panel"], fg=T["accent"],
         font=tf(11, True)).pack(side="left", padx=16)
count_badge = tk.Label(lhead, text="", bg=T["soft"], fg=T["accent"],
                       font=tf(10, True), padx=8, pady=2)
count_badge.pack(side="left", padx=8)

lst = tk.Frame(left, bg=T["panel"])
lst.pack(fill="both", expand=True, padx=(10, 4), pady=(4, 10))
lcanvas = tk.Canvas(lst, bg=T["panel"], highlightthickness=0)
sb = ThinScrollbar(lst, lcanvas)
lcanvas.configure(yscrollcommand=lambda first, last: (sb.redraw(), sb.show() if sb.dragging else None))
sb.pack(side="right", fill="y")
lcanvas.pack(side="left", fill="both", expand=True)
inner = tk.Frame(lcanvas, bg=T["panel"])
lcanvas.create_window((0, 0), window=inner, anchor="nw")


def wheel(ev):
    step = -3 if (ev.delta > 0 if getattr(ev, "delta", 0) else ev.num == 4) else 3
    lcanvas.yview_scroll(step, "units")
    sb.show()
    sb.schedule_hide()


for wd in (lst, lcanvas, inner):
    wd.bind("<MouseWheel>", wheel)
    wd.bind("<Button-4>", wheel)
    wd.bind("<Button-5>", wheel)
    wd.bind("<Enter>", lambda ev: (sb.show(), sb.schedule_hide()))

# ---- 右侧：保存 / 操作 / 预览 ----
right = tk.Frame(body, bg=T["panel"],
                 highlightthickness=1, highlightbackground=T["border"])
right.pack(side="left", fill="both", expand=True)

section(right, "② 保存当前布局", top=12)
tk.Label(right, text=f"当前有 {current_count} 个可管理窗口，保存后可随时一键恢复",
         bg=T["panel"], fg=T["sub"], font=tf(10)).pack(anchor="w", padx=16)
entry_row = tk.Frame(right, bg=T["panel"])
entry_row.pack(fill="x", padx=16, pady=(8, 2))
name_var = tk.StringVar(value=f"布局 {len(items) + 1}")
name_entry = tk.Entry(entry_row, textvariable=name_var, font=tf(11), relief="flat",
                      bg=T["muted"], fg=T["text"], insertbackground=T["text"])
name_entry.pack(side="left", fill="x", expand=True, ipady=7)


def action_btn(parent, text, command, primary=False):
    return tk.Button(parent, text=text, command=command, font=tf(10, primary),
                     bg=T["accent"] if primary else T["muted"],
                     fg=T["on"] if primary else T["text"],
                     activebackground=T["accent"] if primary else T["border"],
                     relief="flat", bd=0, padx=12, pady=5)


save_btn = action_btn(entry_row, "保存", lambda: None)  # 命令在 do_save 定义后绑定
save_btn.pack(side="right", padx=(8, 0))

section(right, "③ 布局操作", top=14)
ops = tk.Frame(right, bg=T["panel"])
ops.pack(fill="x", padx=16)
load_btn = action_btn(ops, "加载布局", lambda: do_load(), primary=True)
load_btn.pack(side="left", padx=(0, 6))
rename_btn = action_btn(ops, "重命名", lambda: do_rename())
rename_btn.pack(side="left", padx=6)
del_btn = action_btn(ops, "删除布局", lambda: do_delete())
del_btn.pack(side="left", padx=6)

section(right, "④ 选中布局预览", top=14)
PV_W, PV_H = 380, 196
preview = tk.Label(right, bg=T["panel"], width=PV_W, height=PV_H)
preview.pack(padx=16)
PREVIEW = [None]

tips = tk.Frame(right, bg=T["soft"])
tips.pack(fill="x", padx=16, pady=(10, 10))
tk.Label(tips, text="操作提示", bg=T["soft"], fg=T["accent"],
         font=tf(10, True)).pack(anchor="w", padx=10, pady=(9, 4))
tk.Label(tips,
         text="· 保存：记录当前所有窗口的位置与大小\n"
              "· 加载：按当前屏幕比例恢复窗口落位\n"
              "· 删除：需连续点击两次确认，避免误删",
         bg=T["soft"], fg=T["sub"], font=tf(9), justify="left",
         wraplength=390).pack(anchor="w", padx=10, pady=(0, 9))


# ---------- 列表 / 预览渲染 ----------
def render_preview(item):
    s = 3
    im = Image.new("RGBA", (PV_W * s, PV_H * s), T["panel"])
    d = ImageDraw.Draw(im)
    p = lambda v: int(v * s)
    bx0, by0, bx1, by1 = p(12), p(10), p(PV_W - 12), p(PV_H - 10)
    rounded(d, (bx0, by0, bx1, by1), p(10),
            fill=T["tile"], outline=T["border"], width=p(1))
    if not item or not item.get("rects"):
        d.text(((bx0 + bx1) / 2, (by0 + by1) / 2), "暂无几何预览",
               font=pf(12 * s), fill=T["sub"], anchor="mm")
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
                   font=pf(11 * s, True), fill=T["on"], anchor="mm")
    return im.resize((PV_W, PV_H), RESAMPLE)


def refresh_preview():
    item = items[sel_idx] if 0 <= sel_idx < len(items) else None
    PREVIEW[0] = ImageTk.PhotoImage(render_preview(item))
    preview.config(image=PREVIEW[0])


def bind_row(wd, cb):
    wd.bind("<Button-1>", cb)
    wd.bind("<Double-Button-1>", lambda ev: do_load())
    for ch in wd.winfo_children():
        bind_row(ch, cb)


def refresh_list():
    for wd in list(inner.winfo_children()):
        wd.destroy()
    count_badge.config(text=f"{len(items)} 个")
    for i, item in enumerate(items):
        on = i == sel_idx
        bg = T["soft"] if on else T["panel"]
        row = tk.Frame(inner, bg=bg)
        row.pack(fill="x", padx=6, pady=4)
        tk.Frame(row, bg=T["accent"] if on else T["panel"], width=3).pack(
            side="left", fill="y")
        txt = tk.Frame(row, bg=bg)
        txt.pack(side="left", fill="x", padx=10, pady=8)
        tk.Label(txt, text=item["name"], bg=bg,
                 fg=T["accent"] if on else T["text"],
                 font=tf(11, True)).pack(anchor="w")
        tk.Label(txt, text=f'{item["created"]}  ·  {item["count"]} 个窗口',
                 bg=bg, fg=T["sub"], font=tf(9)).pack(anchor="w", pady=(3, 0))
        bind_row(row, lambda ev, idx=i: select(idx))
    if not items:
        tk.Label(inner, text="暂无保存的布局\n在右侧输入名称，点击「保存」即可记录当前布局",
                 bg=T["panel"], fg=T["sub"], font=tf(11),
                 justify="center").pack(pady=50)
    inner.update_idletasks()
    iw = max(lcanvas.winfo_width() - 4, inner.winfo_width())
    lcanvas.configure(scrollregion=(0, 0, iw, inner.winfo_height()))
    sb.redraw()
    state = "normal" if items else "disabled"
    for b in (load_btn, rename_btn, del_btn):
        b.config(state=state,
                 fg=T["on"] if (state == "normal" and b is load_btn) else
                 (T["sub"] if state == "disabled" else T["text"]))
    refresh_preview()


def select(idx):
    global sel_idx
    sel_idx = idx
    refresh_list()


def set_status(msg, warn=False):
    status.config(text=msg, fg=T["warn"] if warn else T["sub"])


# ---------- 操作 ----------
def do_save():
    name = name_var.get().strip()
    if not name:
        set_status("请先输入布局名称", True)
        return
    ok, msg = send("save", name)
    if ok:
        items.append(dict(name=name, created="刚刚", count=current_count,
                          rects=current_rects))
        select(len(items) - 1)
        name_var.set(f"布局 {len(items) + 1}")
        name_entry.focus_set()
    else:
        refresh_list()
    set_status(msg, not ok)


def do_load():
    if not (0 <= sel_idx < len(items)):
        return
    ok, msg = send("load", str(sel_idx))
    set_status(msg, not ok)


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
    set_status(msg, not ok)


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
        set_status("再点一次「删除布局」确认删除，4 秒内有效", True)
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
    set_status(msg or f"已删除「{name}」", not ok)


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
    tk.Label(dh, text=title, bg=T["panel"], fg=T["text"],
             font=tf(12, True)).pack(side="left", padx=20, pady=14)
    db = tk.Frame(dlg, bg=T["panel"])
    db.pack(fill="both", expand=True, padx=16, pady=(10, 6))
    var = tk.StringVar(value=initial)
    ent = tk.Entry(db, textvariable=var, font=tf(11), relief="flat",
                   bg=T["muted"], fg=T["text"], insertbackground=T["text"])
    ent.pack(fill="x", ipady=7, padx=6)
    df = tk.Frame(dlg, bg=T["panel"], height=48)
    df.pack(fill="x", side="bottom")
    df.pack_propagate(False)
    answer = [None]

    def finish(v):
        answer[0] = v
        dlg.destroy()

    tk.Button(df, text="取消", command=lambda: finish(None), font=tf(10),
              bg=T["muted"], fg=T["text"], relief="flat", bd=0).pack(
                  side="right", padx=16, pady=10)
    tk.Button(df, text="确定", command=lambda: finish(var.get().strip()),
              font=tf(10, True), bg=T["accent"], fg=T["on"],
              relief="flat", bd=0).pack(side="right", padx=4, pady=10)
    ent.bind("<Return>", lambda ev: finish(var.get().strip()))
    dlg.bind("<Escape>", lambda ev: finish(None))
    dlg.protocol("WM_DELETE_WINDOW", lambda: finish(None))
    dlg.deiconify()
    ent.focus_set()
    ent.select_range(0, "end")
    root.wait_window(dlg)
    return answer[0]


# ---------- 底部状态栏 ----------
footer = tk.Frame(root, bg=T["panel"], height=58)
footer.pack(fill="x", side="bottom")
footer.pack_propagate(False)
status = tk.Label(footer, text="", bg=T["panel"], fg=T["sub"],
                  font=tf(10), anchor="w")
status.pack(side="left", padx=20, fill="x", expand=True)
action_btn(footer, "关闭", lambda: on_close()).pack(side="right", padx=20, pady=12)


def on_close():
    try:
        print("close", flush=True)
    except Exception:
        pass
    root.destroy()


save_btn.config(command=do_save)
root.protocol("WM_DELETE_WINDOW", on_close)
root.bind("<Escape>", lambda ev: on_close())
refresh_list()
root.deiconify()
root.mainloop()
