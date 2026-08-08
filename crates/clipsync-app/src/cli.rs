//! 命令行子命令的展示逻辑。
//!
//! 与 `main` 的组装流程分开：这些只负责"把信息打给用户看"，不参与同步。

/// 打印本机可达地址及其性质，便于排查连通性。
///
/// 注意：这里描述的是地址**性质**而非优先级。优先级由对端按自身网络位置判定
/// ——同一个 192.168.x 地址，对同网段设备是"直连"，对异地设备则不可达；
/// 覆盖网地址则相反。故此处只如实说明每个地址的类型。
pub(crate) fn print_local_addrs(port: u16) {
    use clipsync_net::local::{local_candidates, local_networks};

    let cands = local_candidates(port);
    println!("本机可达地址（配对与连接时会告知对端）：");
    if cands.is_empty() {
        println!("  （未找到可用地址，请检查网络连接）");
        return;
    }
    for sa in &cands {
        println!("  {:<42} {}", sa.to_string(), describe_addr(sa.ip()));
    }

    let nets = local_networks();
    println!();
    println!(
        "本机网段：IPv4 {} 个、IPv6 {} 个",
        nets.v4.len(),
        nets.v6.len()
    );
    println!();
    println!("说明：覆盖网（Tailscale / ZeroTier / Netbird / WireGuard 等）的虚拟网卡地址");
    println!("      会自动出现在上表中，无需任何额外配置。对端连接时按");
    println!("      「同网段直连 → 覆盖网 → 公网」的顺序自动优选最快路径。");
}

/// 用平实语言描述一个地址的性质。
fn describe_addr(ip: std::net::IpAddr) -> &'static str {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 100 && (64..128).contains(&o[1]) {
                "覆盖网段 (100.64/10 CGNAT，Tailscale 等常用)"
            } else if v4.is_private() {
                "局域网/私有网段"
            } else {
                "公网地址"
            }
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            if (seg[0] & 0xfe00) == 0xfc00 {
                "覆盖网段 (IPv6 ULA)"
            } else {
                "公网 IPv6"
            }
        }
    }
}

/// 打印剪贴板的原始内容与文件可读性，用于排查"复制了文件却没同步"。
///
/// **为什么需要它**：这个症状至少有三种成因，表现完全一样（什么都没发生），
/// 解法却南辕北辙——
///   1. 剪贴板里**根本没有文件类型**：有些 App 放的是"承诺文件"或纯文本，
///      我们只认 `public.file-url`；
///   2. **有路径但读不了**：文件在别的 App 的容器里，或在受保护的个人文件夹，
///      macOS 的 TCC 会拒绝；
///   3. 路径已失效：临时文件被清掉了。
///
/// 光看同步结果分不出这三者。这里把原始类型、每条路径、以及 stat/open 的真实
/// errno 一并打出来，一次定位。
///
/// **必须从 App 包内运行**，不能从终端：
/// ```text
/// /Applications/ClipSync.app/Contents/MacOS/clipsync clipdiag
/// ```
/// macOS 的文件访问授权是**按可执行文件**记的，终端自己往往早就被授权过
/// 全盘访问了——在终端里跑一切正常，恰恰会把真正的问题藏起来。
pub(crate) fn print_clipboard_diagnosis() {
    print_identity();
    let types = clipsync_clip::filelist::pasteboard_types();
    println!("剪贴板数据类型（{}）：", types.len());
    if types.is_empty() {
        println!("  （空）");
    }
    for t in &types {
        println!("  {t}");
    }
    println!();

    match clipsync_clip::filelist::read_file_paths() {
        Ok(Some(paths)) => {
            println!("识别到 {} 个文件：", paths.len());
            for p in &paths {
                println!("  {}", p.display());
                probe_one(p);
            }
        }
        Ok(None) => {
            println!("识别到 0 个文件。");
            println!();
            if types.iter().any(|t| t.contains("promise")) {
                println!("  剪贴板里是**承诺文件**（promised file）——文件此刻还不存在，");
                println!("  要等接收方「接受」时才由源 App 写出来。本程序目前不认这种，");
                println!("  这就是它没被同步的原因。");
            } else if types.is_empty() {
                println!("  剪贴板是空的。请先复制文件，再运行本命令。");
            } else {
                println!("  上面这些类型里没有 public.file-url，说明源 App 放进剪贴板的");
                println!("  并不是文件引用（可能只是文本或图片）。");
            }
        }
        Err(e) => println!("读取剪贴板文件列表失败：{e:#}"),
    }
}

/// 打印本进程的「身份」——macOS 的隐私授权就是认这个。
///
/// **为什么排在最前面**：授权是按可执行文件记的，而"你在设置里打开的那个
/// ClipSync"未必就是"正在跑的这个"。两种实机遇到过的错位：
///   - 从终端跑 `clipsync`：终端自己往往早被授过全盘访问，于是一切正常，
///     恰好把问题藏起来；
///   - App 从「下载」这类隔离位置直接双击：macOS 会做 **App 转移**
///     （App Translocation），实际执行的是 `/private/var/.../AppTranslocation/`
///     下一个随机路径的只读副本，而且**每次启动路径都变**——授权当然留不住。
#[cfg(target_os = "macos")]
fn print_identity() {
    let exe = std::env::current_exe().unwrap_or_default();
    println!("正在运行：{}", exe.display());

    if exe.to_string_lossy().contains("/AppTranslocation/") {
        println!("  ⚠ 这是 macOS 的 **App 转移** 副本，路径每次启动都变。");
        println!("    授权永远留不住。请把 ClipSync.app 拖进「应用程序」再运行。");
    } else if !exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        println!("  ⚠ 不是从 App 包里运行的。授权按可执行文件记，终端里的这个");
        println!("    与 ClipSync.app 是两个不同的身份，看到的结果也不一样。");
    }

    // 指定要求决定授权能不能扛过一次重新安装（见打包脚本里的说明）。
    let bundle = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    if let Some(bundle) = bundle.filter(|b| b.extension().is_some_and(|e| e == "app")) {
        if let Ok(out) = std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "-r-", &bundle.to_string_lossy()])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stderr) + String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().find(|l| l.contains("designated =>")) {
                let req = line.trim().trim_start_matches("# ").trim();
                println!("  签名要求：{req}");
                if req.contains("cdhash") {
                    println!(
                        "    ⚠ 锁死在这一份二进制的哈希上——**每装一次新包，已授的权限就作废**，"
                    );
                    println!("      表现是「开关自己关掉了」。请用新版打包脚本重新打包。");
                }
            }
        }
    }
    println!();
}

#[cfg(not(target_os = "macos"))]
fn print_identity() {}

/// 逐条验证一个路径到底能不能读，并解释被拒的原因。
fn probe_one(path: &std::path::Path) {
    // 分两步：`metadata` 只看目录项，`open` 才真正碰内容。TCC 常常允许前者
    // 而拒绝后者，只测一个会得出相反的结论。
    let meta = std::fs::metadata(path);
    let open = std::fs::File::open(path);
    println!(
        "      元数据 {}",
        match &meta {
            Ok(m) => format!("✓ {} 字节", m.len()),
            Err(e) => format!("✗ {e}"),
        }
    );
    println!(
        "      读内容 {}",
        match &open {
            Ok(_) => "✓".to_string(),
            Err(e) => format!("✗ {e}"),
        }
    );
    let denied = |e: &std::io::Error| e.kind() == std::io::ErrorKind::PermissionDenied;
    if meta.as_ref().err().is_some_and(denied) || open.as_ref().err().is_some_and(denied) {
        // 与运行期日志用**同一份**解释，不另写一套措辞。
        let d = clipsync_clip::filelist::explain_denied(path);
        println!("      → {}", d.reason);
        println!("      → 去「{}」把 ClipSync 打开", d.where_to_fix);
    }
}
