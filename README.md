# RsProxyCache

用 Rust 实现的单文件视频和 HLS 点播代理缓存。播放器把完整源地址拼在代理的 `/v1/` 后面即可播放：

```text
http://127.0.0.1:8080/v1/https://media.example.com/video.mp4?token=a%2Fb&expires=123
```

HLS 点播使用同一入口，例如 `/v1/https://media.example.com/movie.m3u8`。入口可通过 `.m3u8` 后缀或源站 HLS 内容类型识别。将主列表、子列表、媒体数据和密钥涉及的所有源站域名加入允许列表。

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
| `RPC_LOG_LEVEL` | `info` | 标准错误日志级别，支持 `error`、`warn`、`info`、`debug`、`trace`、`off` |
| `RPC_ALLOW_PRIVATE_ORIGINS` | `false` | 允许连接白名单内的非公网源站，例如内网 NAS |

通过受控内网或本机入口部署。经过反向代理时保留原始路径和查询串，尤其是源地址的双斜杠、百分号编码和签名参数。若需对外提供 HTTPS，可在入口终止 TLS。源地址和代理播放地址都可能包含签名，应按凭据处理。

## 缓存行为

- 支持无 Range 的渐进式 `GET`、单字节区间 `GET` 和 `HEAD`。源站忽略播放器的 Range 请求时返回 `502`。
- 完整命中的媒体数据直接从磁盘读取。更换签名会使用独立的缓存条目。
- 单文件视频收齐后按源地址的 `host/path` 发布，文件可直接打开。相同路径发生冲突时保留扩展名并加短后缀。尚未发布的数据和索引保存在缓存根目录的 `.rpc` 子目录中。
- 明文 HLS 点播在收齐一个可播放版本后发布本地 `m3u8`，用相对路径引用分片和其他依赖。原代理地址也可离线播放已发布版本。尚未发布的播放列表和 AES-128 密钥按请求回源。
- 缓存无 TTL，跨重启保留。待发布数据和已发布文件共用容量和全局 LRU。已发布 MP4 按整文件淘汰，已发布 HLS 按可播放条目淘汰。补齐缺口时，只有源站目标和强 `ETag` 一致才合并字节。
- 同一视频版本、同一缺失区间的并发请求共享回源，下载成功后从缓存读取该区间。缓存容量不足或写入失败时，等待的请求可能再次回源。

完整暂存文件可以直接发布。需要拼接缓存区间时，原区间和发布临时文件一起计入容量。配额不足时，已有效缓存的区间仍可用于代理播放。若进程在发布时被强停，重启会根据发布记录检查媒体文件和 HLS 依赖，清理未完成的暂存文件及已被完整文件覆盖的区间。

运行时发现缓存文件缺失，会移除失效索引并回源补齐。整个缓存目录丢失后，服务尝试重建并重新取得独占锁。磁盘或锁暂不可用时，请求仍可回源，后续请求会再次尝试恢复缓存。响应已经开始时，只在能确认目标与强 `ETag` 一致的情况下补齐缺失区间。

同一源地址换片后，如需立即清理旧版本，停止服务后在同一个缓存目录运行：

```sh
RPC_CACHE_DIR=./cache target/release/rs-proxy-cache clear 'https://media.example.com/video.mp4?token=a%2Fb&expires=123'
```

显式离线缓存使用 `POST /v1/offline/{完整源地址}`，并在同一路径用 `GET` 查询进度或用 `DELETE` 请求取消。HLS 可通过 `X-RPC-Bandwidth` 指定主播放列表中的带宽。接口状态与移动端 SDK 共用同一下载任务语义。 [离线缓存接口](sdk/README.md#离线缓存)

## 日志

服务向标准错误输出带时间、级别和键值字段的日志。默认记录启动配置、缓存目录恢复、磁盘故障、回源失败和播放流中断。同类磁盘故障每 10 秒最多报告一次。使用 `RPC_LOG_LEVEL=debug` 可查看请求区间、缓存命中、回源状态码和响应头耗时。请求相关事件带有请求 ID，解析出有效源地址后还会附上哈希缓存键。日志省略签名 URL、鉴权头和 Cookie。

## 验证

开发机安装 Rust、Cargo 和 Python 3.11 或更新版本后，可运行：

```sh
sh tests/verify.sh          # Rust 单测和本机 HTTP 集成测试
sh tests/verify.sh --full   # 另跑真实媒体解码和跳转，需要 PATH 上的 FFmpeg
```

测试启动真实代理与受控源站，使用本机回环接口和临时缓存目录。Cargo 依赖已缓存时，可加 `CARGO_NET_OFFLINE=true` 完全离线运行。独占锁竞争用例使用 POSIX `flock`，在其他平台跳过。

完整模式生成 MP4、MPEG-TS HLS 和 fMP4 HLS，经代理验证解码与跳转，并等待一次真实的源站响应体读超时。实际播放器的接入验收见 [首版播放验收](docs/playback-acceptance.md)。

## 移动端 SDK

Android AAR 和 iOS XCFramework 共用 Rust 代理与磁盘缓存。构建、接入及验收见 [移动端接入](sdk/README.md)。
