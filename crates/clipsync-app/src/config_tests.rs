//! `config` 的单元测试（单独成文件以控制行数，仍是其子模块）。

use super::*;

/// 损坏的 settings.json 不得阻止程序启动。
///
/// 这是个托盘常驻程序，而文档与菜单都在引导用户"要精确值就手改
/// settings.json"——改出语法错误是很现实的。原实现把错误抛给 `main`，
/// 进程直接退出，用户看到的是"双击图标毫无反应"，且无处可查原因。
#[test]
fn corrupt_settings_falls_back_instead_of_failing() {
    let dir = std::env::temp_dir().join("clipsync_bad_settings");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), "{ 这不是合法 JSON ,,, ").unwrap();

    let s = load_or_init_settings(&dir).expect("配置损坏不该让启动失败");
    assert_eq!(
        s.auto_fetch_bytes,
        Settings::default().auto_fetch_bytes,
        "应回退到默认值"
    );

    // 坏文件要保留下来，用户手写的内容可能还想找回。
    assert!(
        dir.join("settings.json.bad").exists(),
        "原文件应被保留为 .bad，而不是直接覆盖丢弃"
    );
    // 同时应写出一份可用的新配置，下次启动不再走这条路。
    assert!(dir.join("settings.json").exists());
    assert!(load_or_init_settings(&dir).is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 损坏的 pairings.json 同样不阻止启动，退化为"尚无配对设备"。
#[test]
fn corrupt_pairings_falls_back_to_empty() {
    let dir = std::env::temp_dir().join("clipsync_bad_pairings");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pairings.json"), "[[[ 坏掉了").unwrap();

    let list = load_pairings(&dir).expect("配对记录损坏不该让启动失败");
    assert!(list.is_empty());
    assert!(
        dir.join("pairings.json.bad").exists(),
        "坏文件应保留待人工挽救"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 与上面相反：identity.json 损坏**必须**报错。
///
/// 那里面是本机长期身份私钥，悄悄换一个新的等于换了台设备——所有对端都会
/// 因公钥对不上而拒绝连接，用户只会看到"忽然全都连不上了"，完全猜不到原因。
/// 这种时候明确失败比自作主张地"恢复"要负责得多。
#[test]
fn corrupt_identity_fails_loudly() {
    let dir = std::env::temp_dir().join("clipsync_bad_identity");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("identity.json"), "not json at all").unwrap();

    let err = load_or_init_identity(&dir).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("重新配对") || msg.contains("加密身份"),
        "错误信息应说清后果与可行动作，实际: {msg}"
    );
    // 不得偷偷换一个新身份。
    assert!(
        !dir.join("identity.json.bad").exists(),
        "身份文件不该被自动旁路——那会静默失去所有配对"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 解除配对要真的从磁盘上去掉，否则重启后它又回来了。
#[test]
fn remove_pairing_persists() {
    use clipsync_net::pairing::PairingRecord;

    let dir = std::env::temp_dir().join("clipsync_unpair_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mk = |seed: u8, name: &str| PairingRecord {
        device: clipsync_core::DeviceId::from_public_key(&[seed; 32]),
        name: name.to_string(),
        static_public_key: vec![seed; 32],
        addrs: vec![],
        introduced_by: None,
    };
    let a = mk(1, "笔记本");
    let b = mk(2, "台式机");
    upsert_pairing(&dir, a.clone()).unwrap();
    upsert_pairing(&dir, b.clone()).unwrap();
    assert_eq!(load_pairings(&dir).unwrap().len(), 2);

    let removed = remove_pairing(&dir, &b.device).unwrap();
    assert_eq!(removed.as_deref(), Some("台式机"), "应返回被删设备的名字");

    // 重新从磁盘读——这才证明是真删了而不是只改了内存。
    let left = load_pairings(&dir).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].device, a.device);

    // 删不存在的设备是无操作，不该报错。
    assert!(remove_pairing(&dir, &b.device).unwrap().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 设置往返：写入的自定义值应能原样读回。
#[test]
fn settings_roundtrip_custom_values() {
    let dir = std::env::temp_dir().join("clipsync_settings_roundtrip");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let s = Settings {
        auto_fetch_bytes: crate::size_parse::parse_byte_size("777MB").unwrap() as usize,
        upload_limit_bytes_per_sec: crate::size_parse::parse_rate("33MB/s").unwrap(),
        ..Default::default()
    };
    save_settings(&dir, &s).unwrap();

    let back = load_or_init_settings(&dir).unwrap();
    assert_eq!(back.auto_fetch_bytes, 777_000_000);
    assert_eq!(back.upload_limit_bytes_per_sec, 33_000_000);
}

/// 退出设备组要把配对表清空——留下任何一条，重启后那台设备又回来了。
#[test]
fn leaving_the_group_clears_every_pairing() {
    use clipsync_net::pairing::PairingRecord;

    let dir = std::env::temp_dir().join("clipsync_leave_group_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for i in 0..3u8 {
        upsert_pairing(
            &dir,
            PairingRecord {
                device: clipsync_core::DeviceId::from_public_key(&[i; 32]),
                name: format!("设备{i}"),
                static_public_key: vec![i; 32],
                addrs: vec![],
                introduced_by: None,
            },
        )
        .unwrap();
    }
    assert_eq!(load_pairings(&dir).unwrap().len(), 3);

    clear_pairings(&dir).unwrap();
    assert!(
        load_pairings(&dir).unwrap().is_empty(),
        "退出设备组后不该剩下任何配对"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 旧版本的 pairings.json 没有 introduced_by 字段，必须仍能读取。
#[test]
fn old_records_without_source_still_load() {
    let dir = std::env::temp_dir().join("clipsync_oldrec_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pairings.json"),
        r#"[{"device":"aabbccddeeff0011","name":"旧记录","static_public_key":[1,2,3]}]"#,
    )
    .unwrap();

    let list = load_pairings(&dir).expect("旧格式应能读取");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "旧记录");
    assert!(list[0].introduced_by.is_none(), "旧记录视为亲手配对");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 每次 `update` 都必须让版本号变。
///
/// 托盘的菜单标签（`单次上限：100 MiB…`）就挂在这个数上——变了才重渲染。
/// 回归自实机反馈「选了 100 MiB，二级菜单还写着不限，下次再选才显示上一次
/// 选的值」：改设置的弹窗跑在后台线程，`on_action` 在用户还没看见窗口时就
/// 返回了，点完立刻刷等于把**改之前**的值又渲染一遍。改成按这个版本号轮询
/// 之后，它漏掉一次自增就等于菜单又晚一拍。
#[test]
fn every_update_bumps_the_version() {
    let dir = std::env::temp_dir().join("clipsync_settings_version");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let h = SettingsHandle::new(dir.clone(), Settings::default());
    let v0 = h.version();

    h.update(|s| s.auto_fetch_bytes = 100 << 20);
    let v1 = h.version();
    assert_ne!(v1, v0, "改了值，版本号必须跟着变");
    assert_eq!(h.snapshot().auto_fetch_bytes, 100 << 20);

    // 连改两次也要各记一笔——否则第二次改动在托盘上就是不可见的。
    h.update(|s| s.upload_limit_bytes_per_sec = 5_000_000);
    assert_ne!(h.version(), v1, "第二次改动也得让版本号变");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 老的 `max_bytes` 字段名必须仍能读进来。
///
/// 三台机器上都已经有写好的 settings.json，改名不该把用户设过的值悄悄重置
/// 成默认——那种"设置自己变回去了"最难察觉，也最招人烦。
#[test]
fn the_old_max_bytes_key_still_loads() {
    let dir = std::env::temp_dir().join("clipsync_maxbytes_alias");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        r#"{"max_bytes":524288000,"allow_image":true,"allow_files":true,"listen_port":47684}"#,
    )
    .unwrap();

    let s = load_or_init_settings(&dir).expect("旧字段名应能读取");
    assert_eq!(
        s.auto_fetch_bytes, 524_288_000,
        "用户设过的 500 MiB 不该被重置"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 只写想改的那一项，其余该用默认值——而不是把整份配置判为损坏。
///
/// 回归自一次真实踩坑：验证发布包时手写了 `{"language":"en"}`，程序却输出
/// 中文。查下来是这份配置被判损坏、另存为 `.bad` 后回退到了默认值。行为本身
/// 没错（损坏就该回退），错在**判据**：README 明说这个文件可以手工编辑，那
/// 少写几个字段就该照默认值补齐，而不是整份作废。
///
/// 逐字段断言而不是只测一个：`#[serde(default)]` 是一项一项加的，漏掉哪个
/// 都只有那一个字段会引发整份失效，光测 language 发现不了。
#[test]
fn a_partial_config_fills_in_defaults() {
    let dir = std::env::temp_dir().join("clipsync_partial_config");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), r#"{"language":"en"}"#).unwrap();

    let s = load_or_init_settings(&dir).expect("部分配置应当能加载");
    let d = Settings::default();

    assert_eq!(s.language, "en", "写了的字段要生效");
    assert_eq!(s.auto_fetch_bytes, d.auto_fetch_bytes);
    assert_eq!(s.allow_image, d.allow_image);
    assert_eq!(s.allow_files, d.allow_files);
    assert_eq!(s.listen_port, d.listen_port);
    assert_eq!(s.file_cache_bytes, d.file_cache_bytes);
    assert_eq!(s.compress_transfers, d.compress_transfers);

    assert!(
        !dir.join("settings.json.bad").exists(),
        "不该被当成损坏配置另存"
    );
}

/// 空对象也算合法：等同于全默认。
#[test]
fn an_empty_config_object_is_valid() {
    let dir = std::env::temp_dir().join("clipsync_empty_config");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
    let s = load_or_init_settings(&dir).expect("空对象应当能加载");
    assert_eq!(s.listen_port, Settings::default().listen_port);
    assert!(!dir.join("settings.json.bad").exists());
}
