# 架构与代码导航

## 一句话概括

四个 crate：`core` 是平台无关的决策大脑，`clip` 管剪贴板，`net` 管网络与加密，
`app` 把它们组装成可执行文件。**所有平台特定代码都只在 `clip` 和 `app` 的少数
几个文件里**，其余全部平台无关。

## 数据流

```
用户复制
   ↓
[clip] PollingWatcher 发现变化（Win: 剪贴板序列号 / mac: changeCount）
   ↓ 去抖动 120ms，合并一次复制引发的多次变化
[clip] ArboardClipboard::read() → 归一化为 ClipContent + 敏感标记 + 文件路径
   ↓
[app] clip-watch 线程 → HubEvent::Local → 中枢
   ↓
[core] SyncEngine::on_local_change() 决策：广播 / 跳过(回声·重复·敏感·超限·暂停)
   ↓ 广播
[app] hub 分发给各对端的发送通道
   ↓
[net] NoiseConnection::send() → postcard 编码 → Noise 加密 → 长度前缀分帧 → TCP
   ↓ ~~~ 网络 ~~~
[net] 对端 recv → 解密 → 解码
   ↓
[app] pump → HubEvent::Remote → 中枢
   ↓
[core] SyncEngine::on_remote_clip() 决策：应用 / 跳过
   ↓ 应用（文件则先走内容传输，见下）
[app] engine.expect_echo(hash) 登记防回环 → clipboard.write()
```

**防回环**是这条链路的关键：写入本地剪贴板会再次触发监听器，若不抑制就会
把刚收到的内容又广播回去，形成风暴。见 `core/src/engine.rs` 的 `expect_echo`。

## 文件内容的独立传输流

文件不随 `Clip` 消息发送（100MB 进内存且无法取消），而是：

```
发送方复制文件 → Clip{ Files([元数据]) }        // 很小，只有名字/大小/标识
接收方查缓存 → FileNeed{ generation, id, offset } // offset = 已有字节数，即断点
发送方流式读盘 → FileChunk × N                    // 每块 256KB，块间检查代际号
              → FileDone{ content_hash }         // 全份内容哈希，供校验
接收方校验通过 → 落地(硬链接) → 写入剪贴板
```

代际号（= `Clip` 的 seq）是取代语义的实现：用户复制新内容会推进代际号，
发送方在块边界发现代际过期即中止，接收方保留已收字节供将来续传。

## 各 crate 职责

### `clipsync-core` —— 平台无关的决策大脑
**不做任何 I/O**（不碰剪贴板、网络、文件、时间），因此可脱离环境完整单测。

| 文件 | 内容 |
|---|---|
| `engine.rs` | **同步决策核心**：防回环、去重、敏感过滤、大小限制、暂停 |
| `content.rs` | `ClipContent`（文本/图片/文件元数据）与确定性内容哈希 |
| `message.rs` | 设备间的消息协议（含文件传输的 5 种消息） |
| `device.rs` | 设备身份（Noise 公钥指纹） |
| `hash.rs` | FNV-1a 确定性哈希（跨设备结果一致） |

### `clipsync-clip` —— 剪贴板（平台特定集中地）

| 文件 | 内容 | 平台 |
|---|---|---|
| `arboard_backend.rs` | 读写文本/图片、锁竞争重试、轮询监听与去抖动 | 跨平台 |
| `change_token.rs` | 廉价变更令牌 | **Win 已实现 / mac 待补** |
| `sensitive.rs` | 敏感内容标记探测 | **Win 已实现 / mac 待补** |
| `formats.rs` | 不打开剪贴板判断有无图片 | **Win 已实现 / mac 待补** |
| `filelist.rs` | 文件列表读写 | **Win 已实现 / mac 待补** |
| `stub.rs` | 内存假剪贴板（测试用） | 跨平台 |

### `clipsync-net` —— 网络与加密（全部平台无关）

| 文件 | 内容 |
|---|---|
| `transport.rs` | Noise_IK 握手 + 加密收发 + 多帧重组 + 超时接收 |
| `crypto.rs` | 静态密钥对（设备长期身份） |
| `pairing.rs` / `pairing_handshake.rs` | 配对码 + SPAKE2 密钥协商 + 认证的身份交换 |
| `peer.rs` | **候选地址分类与优选**（同网段 > 覆盖网 > 公网） |
| `local.rs` | 枚举本机网卡（覆盖网地址由此自动发现） |
| `discovery.rs` | 局域网 UDP 组播信标 |
| `wire.rs` | 长度前缀分帧 |

### `clipsync-app` —— 组装与界面

| 文件 | 内容 |
|---|---|
| `main.rs` | 入口、子命令、组装各部件 |
| `hub.rs` | **同步中枢**：唯一持有引擎，串行处理所有事件；接收侧文件状态机 |
| `net_manager.rs` | 监听/拨号/连接去重、收发泵、发送侧文件流 |
| `addrbook.rs` | 地址簿（汇聚配对/信标/对端通告三来源） |
| `filecache.rs` | 文件内容缓存：续传、校验、LRU 淘汰、落地 |
| `filetransfer.rs` | 发送侧登记表与流式分块 |
| `tray.rs` | 系统托盘（图标由代码绘制） |
| `autostart.rs` | 开机自启（Win 注册表 / mac LaunchAgent） |
| `config.rs` | 配置、身份、配对记录的持久化 |

## 几个关键设计决策及其原因

| 决策 | 原因 |
|---|---|
| **同步线程模型，不用 tokio** | `snow`(Noise) 是同步 API；个人少量设备场景每连接一线程足够；省掉重型运行时的内存与依赖 |
| **单一中枢线程持有引擎** | 所有事件串行化，天然无锁竞争，避免并发 bug |
| **连接方向去重**：仅 device_id 较小方拨号 | 双向同时拨号会产生重复连接与抖动 |
| **地址按拓扑分类而非按产品** | 不为 Tailscale/ZeroTier 各写一套适配；枚举网卡自动囊括所有覆盖网 |
| **文件只传元数据 + 独立分块流** | 内存恒定、可中途取代、可断点续传 |
| **缓存键 = hash(名字,大小,mtime)** | 无需读文件即可算出；文件被改后标识自动失效 |
| **托盘在主线程** | macOS 要求 UI 在主线程；Windows 需消息循环 |

## 修改时的注意事项

- **改 `core` 要跑单测**：那里的逻辑（尤其防回环）出错会导致同步风暴。
- **改消息协议**：`message.rs` 的 `SyncMessage` 是双方共识，改动需两端同时更新；
  `PROTOCOL_VERSION` 可用于将来做兼容检查。
- **改剪贴板访问**：注意不要增加打开剪贴板的次数——那会与其它程序抢锁，
  导致用户复制粘贴失败。能用"不打开剪贴板的查询"就别打开。
- **平台代码**：一律用 `#[cfg]` 分支 + 非目标平台的安全回退，保证任何平台都能编译。
