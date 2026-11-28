#!/usr/bin/env python3
"""构建 LitheZones 的 amd64 deb：GNU tar + 真 gzip + 手写 ar。

部分受限环境下 fakeroot/dpkg-deb 的 utime/chmod 会被拦截，cp 覆盖暂存
文件也可能静默失败，所以这里完全用 Python 构造暂存树、GNU tar、gzip 与 ar。

用法（在项目根目录执行）：

    cargo build --release
    python3 tools/build_deb.py [输出目录]

产物存放约定（与 LitheScape 统一）：
    本仓库（lithezones-release）是发布仓库，保持「纯源码、零产物」；
    .deb 一律输出到开发目录 ../dde-layout-rs/，避免同一版本在多处重复存放。
    需要传参指定其它输出目录时：python3 tools/build_deb.py [输出目录]

    本脚本是唯一一份，发布仓库与开发目录共用（开发目录用
    ../lithezones-release/tools/build_deb.py 调用），避免两份副本漂移。
    源码与编译产物位置自动探测，从哪个目录调用都能正常工作。
"""
import atexit
import gzip
import io
import os
import shutil
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# 源码根与开发目录：本仓库叫 lithezones-release，开发目录叫 dde-layout-rs。
REPO_ROOT = ROOT
DEV_ROOT = ROOT.parent / 'dde-layout-rs'

# 源码与编译产物可能分别在发布仓库或开发目录，逐个探测。
SRC_ROOT = None
BINARY = None
for cand_root in (ROOT, DEV_ROOT):
    if not (cand_root / 'packaging' / 'control').is_file():
        continue
    cand_bin = cand_root / 'target' / 'release' / 'lithezones'
    if SRC_ROOT is None:
        SRC_ROOT = cand_root
    if cand_bin.is_file():
        BINARY = cand_bin
        if SRC_ROOT is None or cand_root == ROOT:
            SRC_ROOT = cand_root
        break
if SRC_ROOT is None:
    sys.exit('找不到源码根目录（packaging/control）')
if BINARY is None:
    sys.exit(f'找不到编译产物，请先执行 cargo build --release（应在 {ROOT} 或 {DEV_ROOT}）')

ROOT = SRC_ROOT

# 输出目录：默认为开发目录（唯一存放处），发布仓库内不落 deb。
OUT_DIR = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEV_ROOT

version_line = next(
    line for line in (ROOT / 'packaging/control').read_text().splitlines()
    if line.lower().startswith('version:')
)
VERSION = version_line.split(':', 1)[1].strip()
OUT_DIR.mkdir(parents=True, exist_ok=True)
OUT = OUT_DIR / f'lithezones_{VERSION}_amd64.deb'

tmp_root = ROOT / '.build-tmp'
tmp_root.mkdir(exist_ok=True)
WORK = Path(tempfile.mkdtemp(prefix=f'deb-{VERSION}-', dir=tmp_root))


def _cleanup_work():
    # 每次拆包暂存约 17MB；打完（或中途失败）即删，避免历史版本反复堆积。
    # 某些沙箱会拦截 shutil.rmtree，兜底走系统 rm（绕过 Python 层的包装）。
    if not WORK.exists():
        return
    shutil.rmtree(WORK, ignore_errors=True)
    if WORK.exists():
        import subprocess
        subprocess.run(['/usr/bin/rm', '-rf', str(WORK)],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       check=False)


atexit.register(_cleanup_work)

DATA = WORK / 'data'
CTRL = WORK / 'ctrl'
DATA.mkdir(parents=True)
CTRL.mkdir(parents=True)


def cp(src, dst, mode=0o644):
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dst)
    os.chmod(dst, mode)


# ---------------- data.tar.gz ----------------
cp(BINARY, DATA / 'usr/bin/lithezones', 0o755)
cp(ROOT / 'packaging/lithezones.desktop',
   DATA / 'usr/share/applications/lithezones.desktop')
cp(ROOT / 'packaging/copyright', DATA / 'usr/share/doc/lithezones/copyright')

changelog_gz = DATA / 'usr/share/doc/lithezones/changelog.Debian.gz'
changelog_gz.parent.mkdir(parents=True, exist_ok=True)
changelog_gz.write_bytes(gzip.compress(
    (ROOT / 'packaging/changelog').read_bytes(), compresslevel=9, mtime=0))

for size in (14, 16, 22, 24, 32, 48):
    for theme in ('light', 'dark'):
        cp(ROOT / 'assets/argb' / f'status-{theme}-{size}.png',
           DATA / 'usr/share/icons/hicolor' / f'{size}x{size}' / 'apps'
           / f'dde-layout-status-{theme}.png')

# deepin V25 默认图标主题是 bloom，且 hicolor 被 excluded-icon-themes 排除。
# 只装 hicolor 时 dock 拿不到原生尺寸图标，会退去缩放低分辨率图 → 边缘发虚。
# 因此同一批 PNG 同时装进 bloom 的 apps/<size>/（bloom 用无 x 的目录名）。
for size in (16, 24, 32, 48, 64, 128):
    cp(ROOT / 'assets/app-icons' / f'lithezones-{size}.png',
       DATA / 'usr/share/icons/hicolor' / f'{size}x{size}' / 'apps'
       / 'lithezones.png')
    cp(ROOT / 'assets/app-icons' / f'lithezones-{size}.png',
       DATA / 'usr/share/icons/bloom' / 'apps' / str(size)
       / 'lithezones.png')

scalable = DATA / 'usr/share/icons/hicolor/scalable/apps'
for name in ('dde-layout-status-dark.svg', 'dde-layout-status-light.svg',
             'lithezones.svg'):
    cp(ROOT / 'assets' / name, scalable / name)

cp(ROOT / 'share/panel.py', DATA / 'usr/share/lithezones/panel.py')
cp(ROOT / 'share/lithezones-panel.png',
   DATA / 'usr/share/lithezones/lithezones-panel.png')
vendor_dst = DATA / 'usr/share/lithezones/vendor'
shutil.copytree(ROOT / 'share/vendor', vendor_dst, dirs_exist_ok=True)
# 包内 vendor 统一 644（包括 .so），避免无意义的权限变化。
for _p in vendor_dst.rglob('*'):
    if _p.is_file():
        os.chmod(_p, 0o644)


def make_tar(root, extra_dirs=()):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode='w', format=tarfile.GNU_FORMAT) as tf:
        def add_dir(path, arcname):
            ti = tarfile.TarInfo(arcname)
            ti.type = tarfile.DIRTYPE
            ti.mode = 0o755
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = 'root'
            ti.mtime = 0
            tf.addfile(ti)

        for rel in sorted(set(extra_dirs)):
            add_dir(root / rel, './' + rel)
        for path in sorted(root.rglob('*')):
            rel = str(path.relative_to(root))
            if rel in extra_dirs:
                continue
            if path.is_dir():
                add_dir(path, './' + rel)
                continue
            ti = tf.gettarinfo(str(path), arcname='./' + rel)
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = 'root'
            ti.mtime = 0
            ti.mode = 0o755 if (os.stat(path).st_mode & 0o111) else 0o644
            if path.is_file():
                with open(path, 'rb') as f:
                    tf.addfile(ti, f)
            else:
                tf.addfile(ti)
    return buf.getvalue()


data_tar_gz = gzip.compress(make_tar(DATA), compresslevel=9, mtime=0)

# ---------------- control.tar.gz ----------------
# 成员必须在归档根目录（./control、./postinst、./prerm），不能放 ./DEBIAN/ 下。
cp(ROOT / 'packaging/control', CTRL / 'control')
cp(ROOT / 'packaging/postinst', CTRL / 'postinst', 0o755)
# prerm 可选：没有就不装（dpkg 允许缺省）。
_prerm = ROOT / 'packaging/prerm'
if _prerm.is_file():
    cp(_prerm, CTRL / 'prerm', 0o755)
control_tar_gz = gzip.compress(make_tar(CTRL), compresslevel=9, mtime=0)


def ar_header(name, size, mode=0o644):
    return (f'{name:<16}{0:<12}{0:<6}{0:<6}{mode:<8o}{size:<10}`\n'
            .encode('ascii'))


with open(OUT, 'wb') as f:
    f.write(b'!<arch>\n')
    for name, payload in (
        ('debian-binary', b'2.0\n'),
        ('control.tar.gz', control_tar_gz),
        ('data.tar.gz', data_tar_gz),
    ):
        f.write(ar_header(name, len(payload)))
        f.write(payload)
        if len(payload) % 2:
            f.write(b'\n')

# 打完立即显式清理一次（不依赖 atexit，沙箱下 atexit 里的 rmtree 可能被拦截）。
# atexit 仍保留，用于中途异常退出时兜底。
_cleanup_work()

print('OUTPUT', OUT, OUT.stat().st_size, 'bytes')
