# NekoTrans

NekoTrans 是独立的多轨快传项目。当前实现为桌面端客户端（与原 HybridFileXfer Android 服务端协议兼容）。

## 功能

- 连接控制通道并建立多传输通道
- 列表浏览、删除、创建目录
- 多通道并行上传/下载
- 传输速度与完成统计输出
- 文件日志
- 客户端侧断点续传/校验开关

## 使用

```bash
# ADB 方式
hfx -c adb -s <DEVICE_ID> -d D:\Transfer\Files

# 局域网直连
hfx -c 192.168.1.114 -d D:\Transfer\Files

# 自动重试与超时
hfx -c adb --retry 5 --retry-delay 2s --dial-timeout 8s -d D:\Transfer\Files

# 启用日志/断点续传/校验
hfx -c adb --log-file logs\nekotrans.log --resume --checksum -d D:\Transfer\Files

# 使用配置文件
hfx --config nekotrans.json
```

## 参数

- `-c, --connect` 连接方式：`adb` 或 IP
- `-s, --device` ADB 设备 ID（可选）
- `-d, --dir` 电脑接收目录（默认 `/`）
- `--port` 服务端端口（默认 `5740`）
- `--max-buffers` 限制缓冲区数量（默认不限制）
- `--retry` 连接失败重试次数
- `--retry-delay` 重试间隔（如 `2s`）
- `--dial-timeout` 连接超时（如 `8s`）
- `--log-file` 日志文件路径（为空则关闭文件日志）
- `--log-level` 日志级别：`error|warn|info|debug`
- `--resume` 启用断点续传（仅客户端侧）
- `--checksum` 启用传输校验（仅客户端侧）
- `--config` 配置文件路径（JSON）

## 构建

```bash
cd HybridFileXfer-Go
go build ./cmd/hfx
```

## 配置文件示例

`nekotrans.json`

```json
{
  "connect": "adb",
  "device": "",
  "dir": "D:/Transfer/Files",
  "port": 5740,
  "max_buffers": 0,
  "retry": 5,
  "retry_delay": "2s",
  "dial_timeout": "8s",
  "log_file": "logs/nekotrans.log",
  "log_level": "info",
  "resume": true,
  "checksum": false
}
```

## 说明

- `--resume` 与 `--checksum` 当前只在客户端侧生效，不会改变传输协议。
- 若要实现端到端断点续传/校验，需要安卓端配合扩展协议。

