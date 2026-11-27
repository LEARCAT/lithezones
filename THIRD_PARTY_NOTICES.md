# 第三方组件许可声明

## Pillow 10.4.0

`share/vendor/PIL/` 内为 Pillow 10.4.0，随包分发以便 deepin V25 上的管理面板直接生成窗口缩略图。原始项目：<https://github.com/python-pillow/Pillow>

Pillow 采用 HPND（Historical Permission Notice and Disclaimer）许可：

```
Copyright (c) 1997-2020 by Secret Labs AB
Copyright (c) 1995-2020 by Fredrik Lundh

By obtaining, using and/or copying this software and/or its associated
documentation, you agree that you have read, understood and will comply
with the following terms and conditions:

Permission to use, copy, modify, and distribute this software and its
associated documentation for any purpose and without fee is hereby granted,
provided that the above copyright notice appears in all copies, and that
both the above copyright notice and this permission notice appear in all
supporting documentation, and that the name of Secret Labs AB or the author
not be used in advertising or publicity pertaining to distribution of the
software without specific, written prior permission.

THE AUTHOR AND SECRET LABS AB DISCLAIMS ALL WARRANTIES WITH REGARD TO
THIS SOFTWARE, INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS. IN NO EVENT SHALL THE AUTHOR OR SECRET LABS AB BE LIABLE FOR ANY
SPECIAL, INDIRECT OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER
RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF
CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

预编译的 `.so` 面向 CPython 3.12 / x86_64。

## Rust 依赖

`Cargo.toml` 中声明的依赖均来自 crates.io，各自许可以其上游仓库为准；完整版本锁定见 `Cargo.lock`。
