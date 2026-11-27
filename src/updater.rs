//! 软件更新：从 Gitee / GitHub Releases 探测最新版，下载 deb 并安装。
//!
//! 设计约束（deepin V25 / 只读根分区 / 无免密 sudo）：
//!   · 版本号比较只认 0.5.x 三段数字，避免 tag 前缀 v 干扰。
//!   · Gitee 为主：/releases/latest 的 assets 里带 deb 的 browser_download_url
//!     （用 /releases/{id}/attach_files 上传）。GitHub 只作双保险回退：
//!     Gitee 取不到版本或没带 deb 地址时，才用 GitHub 的信息补齐。
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
    /// deb 的匿名下载地址（优先来自 Gitee assets，缺失时回退 GitHub）。
    pub deb_url: Option<String>,
    /// 更新说明（Release body）。当前启动提示只报版本号不展示它，
    /// 保留字段供将来「更新详情」复用。
    #[allow(dead_code)]
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

/// 探测最新 Release：Gitee 为主（版本 + deb 地址），GitHub 双保险回退。
pub fn check(current: &str) -> Option<Release> {
    // Gitee /releases/latest 的 assets 里带 deb 的 browser_download_url
    // （发布时用 /releases/{id}/attach_files 上传，匿名可读）。
    let gitee = http_get(GITEE_LATEST, 20).and_then(|t| {
        json_string(&t, "tag_name").map(|v| Release {
            version: v.trim_start_matches('v').to_string(),
            deb_url: json_deb_url(&t),
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

    // 两端都查、版本取较高者，避免某一端 Release 落后导致漏报新版。
    // 版本相同时优先 Gitee 附件（国内下载稳）；缺 deb 地址时用另一侧补齐。
    let mut release = match (gitee, github) {
        (Some(gt), Some(gh)) => {
            let gt_v = parse_version(&gt.version).unwrap_or([0, 0, 0]);
            let gh_v = parse_version(&gh.version).unwrap_or([0, 0, 0]);
            if gt_v >= gh_v {
                let mut r = gt;
                if r.deb_url.is_none() {
                    r.deb_url = gh.deb_url;
                }
                r
            } else {
                gh
            }
        }
        (Some(gt), None) => gt,
        (None, Some(gh)) => gh,
        (None, None) => return None,
    };
    if release.deb_url.is_none() {
        let v = &release.version;
        release.deb_url = Some(format!(
            "https://gitee.com/learcat/lithezones/releases/download/v{0}/lithezones_{0}_amd64.deb",
            v
        ));
    }
    if is_newer(&release.version, current) {
        Some(release)
    } else {
        None
    }
}

/// 根据发布信息生成启动提示文案：只报版本号 + 引导到托盘菜单，不夹带更新说明。
fn notice_text(release: &Release) -> String {
    format!(
        "发现新版本 {}。点击托盘菜单「软件更新」下载安装。",
        release.version
    )
}

/// 只检查、不下载不安装的启动提示。有新版本返回给用户看的文案，没有则 None。
/// 供托盘启动后后台调用：有新版本弹一次通知，没有则完全静默。
pub fn startup_notice(current: &str) -> Option<String> {
    let release = check(current)?;
    Some(notice_text(&release))
}

/// 缓存目录：~/.cache/lithezones/。
fn cache_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/LEARCAT".to_string());
    let dir = PathBuf::from(home).join(".cache/lithezones");
    fs::create_dir_all(&dir).with_context(|| format!("创建缓存目录失败: {}", dir.display()))?;
    Ok(dir)
}

/// 从 curl `-#` 进度输出里解析百分比。
/// 进度行形如 "#####          52.6%"，以 \r 刷新；取出现的最后一个百分比。
fn parse_curl_percent(chunk: &str) -> Option<u32> {
    let mut last = None;
    for token in chunk.split_whitespace() {
        if let Some(num) = token.strip_suffix('%') {
            if let Ok(v) = num.parse::<f32>() {
                last = Some(v.round() as u32);
            }
        }
    }
    last
}

/// 下载 deb 到缓存目录，返回本地路径。
/// 主地址（Gitee 优先）失败时回退 GitHub 标准地址，两端都失败才报错。
/// progress: 可选进度回调（0-100），用于通知栏原地更新下载进度。
pub fn download(release: &Release, mut progress: Option<&mut dyn FnMut(u32)>) -> Result<PathBuf> {
    let url = release
        .deb_url
        .as_ref()
        .ok_or_else(|| anyhow!("该版本没有提供 deb 下载地址"))?;
    let name = format!("lithezones_{}_amd64.deb", release.version);
    let path = cache_dir()?.join(name);
    let tmp = path.with_extension("deb.part");

    let v = &release.version;
    let fallback = format!(
        "https://github.com/LEARCAT/lithezones/releases/download/v{0}/lithezones_{0}_amd64.deb",
        v
    );
    let candidates: Vec<&str> = if fallback == *url {
        vec![url.as_str()]
    } else {
        vec![url.as_str(), fallback.as_str()]
    };

    let mut ok = false;
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let mut child = Command::new("curl")
            .args([
                "-fL",
                "-#",
                "--connect-timeout",
                "10",
                "--max-time",
                "300",
                "-o",
                &tmp.to_string_lossy(),
                candidate,
            ])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| "调用 curl 下载失败（请确认已安装 curl）")?;

        // 读 stderr 进度（curl 用 \r 刷新），解析百分比并节流回调
        let has_progress = progress.is_some();
        if has_progress {
            if let Some(stderr) = child.stderr.as_mut() {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                let mut acc = String::new();
                let mut last_report: Option<u32> = None;
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if let Some(pct) = parse_curl_percent(&acc) {
                                // 只在整数百分比变化时回调，避免刷屏
                                if last_report != Some(pct) {
                                    last_report = Some(pct);
                                    if let Some(cb) = progress.as_mut() {
                                        cb(pct);
                                    }
                                }
                            }
                            // 控制累加串长度，只保留尾部片段
                            if acc.len() > 8192 {
                                acc = acc
                                    .chars()
                                    .rev()
                                    .take(1024)
                                    .collect::<String>()
                                    .chars()
                                    .rev()
                                    .collect();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        let status = child
            .wait()
            .with_context(|| "等待 curl 结束失败")?;
        if status.success() {
            ok = true;
            break;
        }
        let _ = fs::remove_file(&tmp);
    }
    if !ok {
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

/// 安装结果。只有 dpkg 真正把新二进制装上才需要自动重启；
/// 交给系统安装器的场景 deb 尚未落地，重启只会启动旧版本，必须交给用户手动完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,       // sudo -n / pkexec 的 dpkg -i 成功，新二进制已就位
    OpenedInstaller, // 已打开系统安装器，由用户手动完成，不自动重启
    Failed,          // 无法自动安装
}

/// 安装 deb：依次尝试 sudo -n、pkexec、xdg-open。
/// 返回 (安装结果, 提示文案)。
pub fn install(path: &PathBuf) -> (InstallOutcome, String) {
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
        return (InstallOutcome::Installed, "已更新到新版本，即将自动重启".to_string());
    }

    // 2) pkexec：弹一次系统密码框，密码不经过本程序。
    if Command::new("pkexec")
        .args(["dpkg", "-i", &deb])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (InstallOutcome::Installed, "已更新到新版本，即将自动重启".to_string());
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
                InstallOutcome::OpenedInstaller,
                "deb 已下载，已打开系统安装器，安装完成后将自动启动轻格分窗".to_string(),
            );
        }
    }

    (InstallOutcome::Failed, "更新包已下载，但无法自动安装，请手动安装".to_string())
}

/// 供菜单点击调用：只检查 + 下载，不做安装。
/// 安装交给独立的 --apply-update 助手进程：本进程会先退出释放 D-Bus 名称，
/// 避免"旧版还在运行就被替换二进制"。返回 (通知文案, 下载好的 deb 路径)。
/// progress: 下载进度回调（0-100），供通知栏原地更新。
pub fn prepare_update(
    current: &str,
    progress: Option<&mut dyn FnMut(u32)>,
) -> (String, Option<PathBuf>) {
    let release = match check(current) {
        Some(r) => r,
        None => return ("当前已是最新版本，或暂时无法连接更新服务器".to_string(), None),
    };
    match download(&release, progress) {
        Ok(p) => (
            format!("已下载新版本 {}，即将退出旧版并安装", release.version),
            Some(p),
        ),
        Err(e) => (format!("下载更新失败：{}", e), None),
    }
}

/// 读取系统已安装版本，供「等安装器装完再启动」的助手判断。
pub fn installed_version() -> Option<String> {
    let out = Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "lithezones"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// 供 --apply-update 助手进程调用：安装已下载的 deb。
/// 返回 (通知文案, 安装结果)，由调用方决定立即启动还是等待安装器。
pub fn apply_downloaded(path: &PathBuf) -> (String, InstallOutcome) {
    let (outcome, msg) = install(path);
    let text = match outcome {
        InstallOutcome::Installed => format!("更新完成：{}", msg),
        InstallOutcome::OpenedInstaller | InstallOutcome::Failed => {
            format!("{}, 位置: {}", msg, path.display())
        }
    };
    (text, outcome)
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

    #[test]
    fn startup_notice_exact_wording() {
        // 启动提示必须是规定的一句话，且绝不夹带更新说明正文。
        let release = Release {
            version: "0.5.99".into(),
            deb_url: None,
            notes: "修复若干问题\n第二行不应出现在通知里".into(),
        };
        assert_eq!(
            notice_text(&release),
            "发现新版本 0.5.99。点击托盘菜单「软件更新」下载安装。"
        );
    }

    #[test]
    fn gitee_assets_deb_url_is_parsed() {
        // Gitee releases/latest 的 assets 结构里应能取出 .deb 下载地址。
        let text = r#"{"assets":[{"name":"v0.5.57.zip","browser_download_url":"https://gitee.com/x/zip"},{"name":"lithezones_0.5.57_amd64.deb","browser_download_url":"https://gitee.com/x/lithezones_0.5.57_amd64.deb"}]}"#;
        assert_eq!(
            json_deb_url(text).as_deref(),
            Some("https://gitee.com/x/lithezones_0.5.57_amd64.deb")
        );
    }

    #[test]
    fn version_compare_picks_higher_endpoint() {
        // 两端版本不一致时必须取较高者，防止一端 Release 落后导致漏报。
        assert!(is_newer("0.5.58", "0.5.57"));
        assert!(!is_newer("0.5.57", "0.5.58"));
        // 平手时按 Gitee 优先由 check 的 match 分支处理，这里只锁定比较语义。
        assert!(!is_newer("0.5.57", "0.5.57"));
    }

    #[test]
    fn startup_notice_ignores_notes() {
        let release = Release {
            version: "0.6.0".into(),
            deb_url: None,
            notes: "任意说明文字".into(),
        };
        let msg = notice_text(&release);
        assert!(msg.contains("0.6.0"));
        assert!(msg.contains("软件更新"));
        assert!(!msg.contains("任意说明文字"));
    }

    #[test]
    fn curl_percent_parsing() {
        // curl -# 用 \r 刷新，形如 "#####     52.6%\r#########  100.0%"
        assert_eq!(parse_curl_percent("###          52.6%"), Some(53));
        assert_eq!(parse_curl_percent("\r#########  100.0%"), Some(100));
        assert_eq!(parse_curl_percent("#=#=#"), None);
        // 取最后一个百分比
        assert_eq!(parse_curl_percent("  20.0%\r  87.3%"), Some(87));
        assert_eq!(parse_curl_percent(""), None);
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
