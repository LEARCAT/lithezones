//! 软件更新：从 Gitee / GitHub Releases 探测最新版，下载 deb 并安装。
//!
//! 设计约束（deepin V23 / 只读根分区 / 无免密 sudo）：
//!   · 版本号比较只认 0.5.x 三段数字，避免 tag 前缀 v 干扰。
//!   · Gitee Release 没有自定义附件，只能拿版本号；deb 下载地址从
//!     GitHub Release 的 assets 里取。GitHub 不可用时给出明确提示。
//!   · 安装不追求"零交互"：先试 `sudo -n`（免密环境真静默），失败再
//!     `pkexec dpkg -i`（弹一次密码框），最后退回 `xdg-open` 让系统
//!     安装器接管。全程不把密码交给本程序。
//!   · 所有网络/安装动作都放在 spawn_blocking 线程里，不卡托盘主循环。

use anyhow::{anyhow, bail, Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const GITEE_LATEST: &str =
    "https://gitee.com/api/v5/repos/learcat/lithezones/releases/latest";
const GITHUB_LATEST: &str =
    "https://api.github.com/repos/LEARCAT/lithezones/releases/latest";

/// 上游发布信息。
#[derive(Debug, Clone)]
pub struct Release {
    /// 纯版本号，如 0.5.55（去掉 v 前缀）。
    pub version: String,
    /// deb 的匿名下载地址（来自 GitHub assets）。
    pub deb_url: Option<String>,
    /// 更新说明。
    pub notes: String,
}

/// 解析形如 "v0.5.55" / "0.5.55" 的版本号，返回可比较的三段数组。
/// 无法解析时返回 None，比较逻辑会保守地判为"不更新"。
fn parse_version(s: &str) -> Option<[u64; 3]> {
    let s = s.trim().trim_start_matches('v').trim_start_matches('V');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // patch 可能带后缀（如 0.5.55-beta），只取前导数字。
    let patch_str: String = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = patch_str.parse().ok()?;
    Some([major, minor, patch])
}

/// 判断 latest 是否比 current 新。版本号解析失败时返回 false（保守）。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a.cmp(&b) == Ordering::Greater,
        _ => false,
    }
}

/// 从 JSON 文本里取一个字符串字段（轻量解析，避免为一个字段引入依赖）。
fn json_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = text.find(&needle)?;
    let rest = &text[pos + needle.len()..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => break,
            },
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// 从 GitHub Release JSON 的 assets 数组里取 .deb 的下载地址。
fn json_deb_url(text: &str) -> Option<String> {
    // assets 里每个对象含 "name" 与 "browser_download_url"；
    // 只在 name 以 .deb 结尾时取紧随其后的下载 URL。
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("\"browser_download_url\"") {
        let idx = search_from + rel;
        // 该 URL 前面最近的对象片段里若含 .deb 名称则认为命中。
        let window_start = idx.saturating_sub(600);
        let window = &text[window_start..idx];
        let url = json_string(&text[idx..], "browser_download_url")?;
        if window.rfind("\"name\"").and_then(|p| {
            json_string(&window[p..], "name")
        }).map(|n| n.ends_with(".deb")).unwrap_or(false) {
            return Some(url);
        }
        search_from = idx + "browser_download_url".len() + 1;
    }
    None
}

/// 用 curl 拉取 URL；失败返回 None（网络不可达是常态，不视为错误）。
fn http_get(url: &str, timeout_secs: u64) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            &timeout_secs.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// 探测最新 Release：Gitee 优先（读版本号快），GitHub 兜底并补充 deb 地址。
pub fn check(current: &str) -> Option<Release> {
    let gitee = http_get(GITEE_LATEST, 20).and_then(|t| {
        json_string(&t, "tag_name").map(|v| Release {
            version: v.trim_start_matches('v').to_string(),
            deb_url: None,
            notes: json_string(&t, "body").unwrap_or_default(),
        })
    });

    let github = http_get(GITHUB_LATEST, 25).and_then(|t| {
        json_string(&t, "tag_name").map(|v| Release {
            version: v.trim_start_matches('v').to_string(),
            deb_url: json_deb_url(&t),
            notes: json_string(&t, "body").unwrap_or_default(),
        })
    });

    // 优先用 GitHub 的版本（信息更全：带 deb 地址）；Gitee 只作兜底。
    let mut release = match (github, gitee) {
        (Some(gh), _) => gh,
        (None, Some(gt)) => gt,
        (None, None) => return None,
    };
    if release.deb_url.is_none() {
        // Gitee 没有附件时，按约定拼 GitHub 的标准下载路径。
        let v = &release.version;
        release.deb_url = Some(format!(
            "https://github.com/LEARCAT/lithezones/releases/download/v{0}/lithezones_{0}_amd64.deb",
            v
        ));
    }
    if is_newer(&release.version, current) {
        Some(release)
    } else {
        None
    }
}

/// 缓存目录：~/.cache/lithezones/。
fn cache_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/LEARCAT".to_string());
    let dir = PathBuf::from(home).join(".cache/lithezones");
    fs::create_dir_all(&dir).with_context(|| format!("创建缓存目录失败: {}", dir.display()))?;
    Ok(dir)
}

/// 下载 deb 到缓存目录，返回本地路径。
pub fn download(release: &Release) -> Result<PathBuf> {
    let url = release
        .deb_url
        .as_ref()
        .ok_or_else(|| anyhow!("该版本没有提供 deb 下载地址"))?;
    let name = format!("lithezones_{}_amd64.deb", release.version);
    let path = cache_dir()?.join(name);
    let tmp = path.with_extension("deb.part");

    let status = Command::new("curl")
        .args([
            "-fL",
            "--connect-timeout",
            "10",
            "--max-time",
            "300",
            "-o",
            &tmp.to_string_lossy(),
            url,
        ])
        .status()
        .with_context(|| "调用 curl 下载失败（请确认已安装 curl）")?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        bail!("下载失败（网络不可达或下载地址失效）");
    }
    let meta = fs::metadata(&tmp)?;
    if meta.len() < 1_000_000 {
        let _ = fs::remove_file(&tmp);
        bail!("下载文件过小，可能不是有效的 deb 包");
    }
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// 安装 deb：依次尝试 sudo -n、pkexec、xdg-open。
/// 返回 (是否成功, 提示文案)。
pub fn install(path: &PathBuf) -> (bool, String) {
    let deb = path.to_string_lossy();

    // 1) 免密 sudo：真静默。
    if Command::new("sudo")
        .args(["-n", "dpkg", "-i", &deb])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (true, "已更新到新版本，重启程序后生效".to_string());
    }

    // 2) pkexec：弹一次系统密码框，密码不经过本程序。
    if Command::new("pkexec")
        .args(["dpkg", "-i", &deb])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (true, "已更新到新版本，重启程序后生效".to_string());
    }

    // 3) 交给系统安装器（deepin 上通常是 deepin-deb-installer）。
    for opener in ["xdg-open", "deepin-deb-installer"] {
        if Command::new(opener)
            .arg(&*deb)
            .spawn()
            .map(|_| true)
            .unwrap_or(false)
        {
            return (
                true,
                "deb 已下载，已打开系统安装器，请手动完成安装".to_string(),
            );
        }
    }

    (false, "更新包已下载，但无法自动安装，请手动安装".to_string())
}

/// 供菜单点击调用：检查 → 下载 → 安装，全程同步（调用方应放在 spawn_blocking）。
/// 返回给用户看的通知文案。
pub fn run_update(current: &str) -> String {
    let release = match check(current) {
        Some(r) => r,
        None => return "当前已是最新版本，或暂时无法连接更新服务器".to_string(),
    };
    let path = match download(&release) {
        Ok(p) => p,
        Err(e) => return format!("下载更新失败：{}", e),
    };
    let (ok, msg) = install(&path);
    if !ok {
        return format!("{}, 位置: {}", msg, path.display());
    }
    // 更新说明可能很长，只取第一行加入通知，避免通知气泡撑爆。
    let brief = release
        .notes
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(60)
        .collect::<String>();
    if brief.is_empty() {
        format!("发现新版本 {}：{}", release.version, msg)
    } else {
        format!("发现新版本 {}（{}）：{}", release.version, brief, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_basic() {
        assert_eq!(parse_version("0.5.54"), Some([0, 5, 54]));
        assert_eq!(parse_version("v0.5.55"), Some([0, 5, 55]));
        assert_eq!(parse_version("V1.2.3-beta"), Some([1, 2, 3]));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn is_newer_cases() {
        assert!(is_newer("v0.5.55", "0.5.54"));
        assert!(is_newer("0.6.0", "0.5.99"));
        assert!(!is_newer("0.5.54", "0.5.54"));
        assert!(!is_newer("0.5.53", "0.5.54"));
        assert!(!is_newer("bad", "0.5.54"));
        assert!(!is_newer("0.5.55", "bad"));
    }

    #[test]
    fn json_string_simple() {
        let text = r#"{"tag_name":"v0.5.55","body":"line1\nline2"}"#;
        assert_eq!(json_string(text, "tag_name").as_deref(), Some("v0.5.55"));
        assert_eq!(json_string(text, "body").as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn json_deb_url_from_assets() {
        let text = r#"{"assets":[{"name":"source.zip","browser_download_url":"https://x/zip"},{"name":"lithezones_0.5.55_amd64.deb","browser_download_url":"https://x/lithezones_0.5.55_amd64.deb"}]}"#;
        assert_eq!(
            json_deb_url(text).as_deref(),
            Some("https://x/lithezones_0.5.55_amd64.deb")
        );
    }
}

#[cfg(test)]
mod live_check_tests {
    use super::*;

    /// 真实网络探测（release 构建时手动运行）：确认两个端点可达且能拼出下载地址。
    /// 用 #[ignore] 避免常规测试依赖网络。
    #[test]
    #[ignore]
    fn live_release_probe() {
        // 传一个比线上更老的版本，确保 check 返回 Some。
        let r = check("0.0.0").expect("应能探测到线上 Release");
        assert!(!r.version.is_empty());
        assert!(r.deb_url.as_ref().map(|u| u.ends_with(".deb")).unwrap_or(false));
        eprintln!("live version={} url={}", r.version, r.deb_url.unwrap_or_default());
    }
}
