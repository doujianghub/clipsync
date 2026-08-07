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
    assert_eq!(s.max_bytes, Settings::default().max_bytes, "应回退到默认值");

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
    assert!(dir.join("pairings.json.bad").exists(), "坏文件应保留待人工挽救");

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

    let mut s = Settings::default();
    s.max_bytes = crate::size_parse::parse_byte_size("777MB").unwrap() as usize;
    s.upload_limit_bytes_per_sec = crate::size_parse::parse_rate("33MB/s").unwrap();
    save_settings(&dir, &s).unwrap();

    let back = load_or_init_settings(&dir).unwrap();
    assert_eq!(back.max_bytes, 777_000_000);
    assert_eq!(back.upload_limit_bytes_per_sec, 33_000_000);
}
