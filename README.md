# RsProxyCache

用 Rust 实现的单文件视频代理缓存。播放器把完整源地址拼在代理的 `/v1/` 后面即可播放：

```text
http://127.0.0.1:8080/v1/https://media.example.com/video.mp4?token=a%2Fb&expires=123
```

## 启动

需要 Rust 1.91 或更新版本。把示例域名换成实际源站域名：

```sh
RPC_ORIGIN_HOSTS=media.example.com,cdn.example.com cargo run --release --locked
```

| 环境变量 | 默认值 | 用途 |
| --- | --- | --- |
| `RPC_ORIGIN_HOSTS` | 必填 | 逗号分隔的精确主机名列表，每次重定向都重新校验 |
| `RPC_LISTEN` | `127.0.0.1:8080` | 服务监听地址 |
| `RPC_CACHE_DIR` | `./cache` | 单实例独占的磁盘缓存目录 |
| `RPC_CACHE_BYTES` | `10737418240` | 缓存数据容量上限，单位为字节 |
| `RPC_ALLOW_PRIVATE_ORIGINS` | `false` | 允许连接白名单内的非公网源站，例如内网 NAS |

通过受控内网或本机入口部署。经过反向代理时保留原始路径和查询串，尤其是源地址的双斜杠、百分号编码和签名参数。若需对外提供 HTTPS，可在入口终止 TLS。源地址和代理播放地址都可能包含签名，应按凭据处理。

## 缓存行为

- 支持无 Range 的渐进式 `GET`、单字节区间 `GET` 和 `HEAD`。源站忽略播放器的 Range 请求时返回 `502`。
- 完整命中直接读取磁盘，源站离线或签名过期时仍可读取。更新签名会使用独立的缓存条目。
- 缓存无 TTL，跨重启保留，容量不足时按全局 LRU 淘汰区间。补齐缺口时，只有源站目标和强 `ETag` 一致才合并字节。
- 同一视频版本、同一缺失区间的并发请求共享回源，下载成功后从缓存读取该区间。缓存容量不足或写入失败时，等待的请求可能再次回源。

同一源地址换片后，如需立即清理旧版本，停止服务后在同一个缓存目录运行：

```sh
RPC_CACHE_DIR=./cache target/release/rs-proxy-cache clear 'https://media.example.com/video.mp4?token=a%2Fb&expires=123'
```

## 验证

```sh
cargo build --locked
python3 tests/playback.py -v
```

测试使用本机回环接口和临时缓存目录。系统安装了 FFmpeg 时，还会生成真实 MP4，经代理验证解码、跳转与源地址保真。实际播放器的接入验收见 [首版播放验收](docs/playback-acceptance.md)。
