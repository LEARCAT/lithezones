# 轻格分窗 LitheZones

一个为 **deepin V25 / X11** 编写的轻量窗口排列工具。托盘常驻，点一下就能把当前打开的窗口按预设布局排好，适合多窗口办公、分屏对照和临时整理桌面。

- 当前版本：**0.5.58**
- 适用环境：deepin V25、X11 会话、amd64（V25 的 Treeland/Wayland 会话不适用，需登录 X11）
- 许可证：[GPL-3.0-or-later](LICENSE)

## 功能

- 托盘常驻，托盘图标或启动器图标都能打开「窗口排列」面板。
- **9 种布局**：四分网格、上下对半、左右对半、左1右2、上1下2、左2右1、上2下1、上3下3、居中浮动。
- 右侧选择窗口，**按点击先后编号 1、2、3…**，先点的落到第 1 格。
- 面板打开期间实时刷新窗口列表，新开/关闭的窗口自动更新。
- 支持微缩图和列表两种模式，偏好自动保存。
- 支持命名布局快照：记录窗口位置、大小以及上下层叠顺序。
- 主题页提供 6 种强调色，选择后立即生效并记住；深浅模式另由 `config.json` 控制。
- 默认快捷键：`Alt+1` 打开窗口排列，`Alt+2` 打开布局管理，可在面板里改键/恢复默认。
- 托盘菜单「软件更新」：自动检查 Gitee / GitHub Release，发现新版后下载 deb 并安装（免密 sudo → pkexec → 系统安装器逐级降级，密码不经过本程序）。
- 点「应用布局」后面板最小化到任务栏，不是退出；再点任务栏图标即可恢复。

## 安装

图形方式：双击 `lithezones_0.5.58_amd64.deb`，用系统安装器安装。

命令行方式：

```bash
sudo apt install wmctrl python3 python3-tk
sudo dpkg -i lithezones_0.5.58_amd64.deb
```

如果提示依赖问题：

```bash
sudo apt --fix-broken install
sudo apt install wmctrl python3 python3-tk
```

说明：

- 管理面板需要 `python3-tk`（本机实测 deepin V25 已自带，部分精简镜像可能缺失）；托盘和快捷键核心功能不依赖 Python。
- 缩略图所需的 Pillow 已随包打包，不需要额外安装。
- 面板固定使用 `/usr/bin/python3`，需要 3.12。
- 安装后**默认不开机自启**，需要的话从托盘菜单勾选。
- 安装脚本会把应用加入 dock 收藏；若任务栏图标没马上出现，注销重新登录一次。

详细使用说明见 [安装说明.md](安装说明.md)。

## 从源码构建

需要 Rust 稳定版工具链、Python 3.12 和 `cargo`：

```bash
cargo test --release
cargo build --release
python3 tools/build_deb.py
```

构建脚本会在项目根目录生成 `lithezones_<版本>_amd64.deb`。它使用 GNU tar + 真 gzip + 手写 ar 构造 deb，避免部分沙箱环境下 `dpkg-deb` 的 `utime`/`chmod` 限制。

`share/vendor/` 内是随包分发的 Pillow 10.4.0（含预编译 `.so`，面向 CPython 3.12 / x86_64），用于保证面板缩略图在 deepin V25 上开箱即用。许可声明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 目录结构

```
src/          Rust 后端：X11 排窗、快照、托盘、D-Bus、快捷键
share/        Tkinter 管理面板与随包 Python 依赖
packaging/    Debian control / postinst / changelog / copyright
assets/       图标与托盘菜单素材
tools/        deb 构建脚本
```

## 配置

配置文件位于 `~/.config/lithezones/`：

- `config.json`：快捷键、深浅模式、主题、布局快照
- `panel-prefs.json`：面板模式、网格行列数、强调色

## 反馈

使用时遇到问题，附上 deepin 版本、会话类型（X11/Wayland）、`/usr/bin/python3 --version`、截图和复现步骤，会更容易定位。目前主要在 deepin V25（X11 会话）上开发和验证，其他发行版/桌面环境未做充分测试。
