package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"strings"
	"sync"
	"time"

	"nekotrans/internal/hfx"
)

const (
	appName    = "NekoTrans"
	appVersion = "0.2.0"
)

func main() {
	cfgPath := extractConfigPath(os.Args)
	explicitCfg := hasExplicitConfig(os.Args)

	cfg, cfgErr := loadConfig(cfgPath)
	if cfgErr != nil {
		if explicitCfg || !errors.Is(cfgErr, os.ErrNotExist) {
			fmt.Fprintf(os.Stderr, "配置文件读取失败: %v\n", cfgErr)
			os.Exit(2)
		}
		cfg = Config{}
	}

	var (
		connect     string
		deviceID    string
		homeDir     string
		showVersion bool
		maxBuffers  int
		port        int
		retryCount  int
		retryDelay  time.Duration
		dialTimeout time.Duration
		logFile     string
		logLevel    string
		resume      bool
		checksum    bool
	)

	defConnect := cfg.Connect
	defDevice := cfg.Device
	defDir := cfg.Dir
	defPort := cfg.Port
	if defPort == 0 {
		defPort = 5740
	}
	defMaxBuffers := cfg.MaxBuffers
	defRetry := cfg.Retry
	defRetryDelay := parseDurationDefault(cfg.RetryDelay, 2*time.Second)
	defDialTimeout := parseDurationDefault(cfg.DialTimeout, 8*time.Second)
	defLogFile := cfg.LogFile
	defLogLevel := cfg.LogLevel
	defResume := cfg.Resume
	defChecksum := cfg.Checksum

	flag.StringVar(&connect, "c", defConnect, "连接方式: adb 或 IP 地址")
	flag.StringVar(&connect, "connect", defConnect, "连接方式: adb 或 IP 地址")
	flag.StringVar(&deviceID, "s", defDevice, "ADB 设备 ID (可选)")
	flag.StringVar(&deviceID, "device", defDevice, "ADB 设备 ID (可选)")
	if defDir == "" {
		defDir = "/"
	}
	flag.StringVar(&homeDir, "d", defDir, "电脑接收目录 (默认: /)")
	flag.StringVar(&homeDir, "dir", defDir, "电脑接收目录 (默认: /)")
	flag.IntVar(&port, "port", defPort, "服务端端口 (默认: 5740)")
	flag.IntVar(&maxBuffers, "max-buffers", defMaxBuffers, "限制缓冲区数量 (0 表示不限制)")
	flag.IntVar(&retryCount, "retry", defRetry, "连接失败重试次数")
	flag.DurationVar(&retryDelay, "retry-delay", defRetryDelay, "重试间隔 (如 2s)")
	flag.DurationVar(&dialTimeout, "dial-timeout", defDialTimeout, "连接超时 (如 8s)")
	flag.StringVar(&logFile, "log-file", defLogFile, "日志文件路径 (为空则关闭文件日志)")
	flag.StringVar(&logLevel, "log-level", defLogLevel, "日志级别: error|warn|info|debug")
	flag.BoolVar(&resume, "resume", defResume, "启用断点续传(仅客户端侧)")
	flag.BoolVar(&checksum, "checksum", defChecksum, "启用传输校验(仅客户端侧)")
	flag.String("config", cfgPath, "配置文件路径 (JSON)")
	flag.BoolVar(&showVersion, "v", false, "显示版本信息")
	flag.BoolVar(&showVersion, "version", false, "显示版本信息")

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "%s v%s\n\n", appName, appVersion)
		fmt.Fprintln(os.Stderr, "Usage:")
		fmt.Fprintln(os.Stderr, "  hfx -c adb [-s DEVICE] [-d DIR]")
		fmt.Fprintln(os.Stderr, "  hfx -c 192.168.1.114 -d D:\\Transfer\\Files")
		fmt.Fprintln(os.Stderr, "\nOptions:")
		fmt.Fprintln(os.Stderr, "  -c, --connect        连接方式: adb 或 IP 地址")
		fmt.Fprintln(os.Stderr, "  -s, --device         ADB 设备 ID (可选)")
		fmt.Fprintln(os.Stderr, "  -d, --dir            电脑接收目录 (默认: /)")
		fmt.Fprintln(os.Stderr, "      --port           服务端端口 (默认: 5740)")
		fmt.Fprintln(os.Stderr, "      --max-buffers    限制缓冲区数量 (0 表示不限制)")
		fmt.Fprintln(os.Stderr, "      --retry          连接失败重试次数")
		fmt.Fprintln(os.Stderr, "      --retry-delay    重试间隔 (如 2s)")
		fmt.Fprintln(os.Stderr, "      --dial-timeout   连接超时 (如 8s)")
		fmt.Fprintln(os.Stderr, "      --log-file       日志文件路径 (为空则关闭文件日志)")
		fmt.Fprintln(os.Stderr, "      --log-level      日志级别: error|warn|info|debug")
		fmt.Fprintln(os.Stderr, "      --resume         启用断点续传(仅客户端侧)")
		fmt.Fprintln(os.Stderr, "      --checksum       启用传输校验(仅客户端侧)")
		fmt.Fprintln(os.Stderr, "      --config         配置文件路径 (JSON)")
		fmt.Fprintln(os.Stderr, "  -v, --version        显示版本信息")
	}

	flag.Parse()

	if showVersion {
		fmt.Printf("%s v%s\n", appName, appVersion)
		return
	}

	if strings.TrimSpace(connect) == "" {
		fmt.Fprintln(os.Stderr, "错误: 未指定连接方式，请使用 -c adb 或 -c <IP>。")
		flag.Usage()
		os.Exit(2)
	}

	homeDir = hfx.NormalizeHomeDir(homeDir)

	logger, err := NewLogger(logFile, logLevel)
	if err != nil {
		fmt.Fprintf(os.Stderr, "日志文件初始化失败: %v\n", err)
		os.Exit(2)
	}
	defer logger.Close()

	ui := NewConsoleUI(logger)

	if err := ensureWritableDir(homeDir); err != nil {
		ui.Error(fmt.Sprintf("接收目录不可用: %v", err))
		os.Exit(1)
	}

	if connect == "adb" {
		ui.Info("执行 ADB 端口转发...")
		if err := hfx.ADBForward(port, deviceID); err != nil {
			ui.Error(fmt.Sprintf("ADB 端口转发失败: %v", err))
			os.Exit(1)
		}
		connect = "127.0.0.1"
		ui.Info("ADB 端口转发成功。")
	}

	if resume {
		ui.Info("断点续传: 已启用(客户端侧)")
	}
	if checksum {
		ui.Info("传输校验: 已启用(客户端侧)")
	}

	callbacks := ui.Callbacks(checksum)

	var lastErr error
	attempts := retryCount + 1
	for i := 1; i <= attempts; i++ {
		client := hfx.NewClient(connect, port, homeDir, maxBuffers, dialTimeout, resume, checksum, callbacks)
		if err := client.Connect(); err == nil {
			lastErr = nil
			if err := client.Start(); err != nil {
				ui.Error(fmt.Sprintf("运行结束: %v", err))
				os.Exit(1)
			}
			return
		} else {
			lastErr = err
			if i < attempts {
				ui.Error(fmt.Sprintf("连接失败: %v，%s 后重试(%d/%d)", err, retryDelay, i, attempts))
				time.Sleep(retryDelay)
			}
		}
	}

	if lastErr != nil {
		ui.Error(fmt.Sprintf("连接失败: %v", lastErr))
		os.Exit(1)
	}
}

type ConsoleUI struct {
	mu     sync.Mutex
	logger *Logger
}

func NewConsoleUI(logger *Logger) *ConsoleUI {
	return &ConsoleUI{logger: logger}
}

func (c *ConsoleUI) Info(msg string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	writeStdoutLine(msg)
	if c.logger != nil {
		c.logger.Info(msg)
	}
}

func (c *ConsoleUI) Error(msg string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	writeStderrLine(msg)
	if c.logger != nil {
		c.logger.Error(msg)
	}
}

func (c *ConsoleUI) Callbacks(checksumEnabled bool) hfx.Callbacks {
	cb := hfx.Callbacks{
		ConnectingControlChannel: func(address string, port int) {
			c.Info(fmt.Sprintf("连接控制通道: %s:%d", address, port))
		},
		VersionMismatch: func(localVersion, remoteVersion int) {
			c.Error(fmt.Sprintf("版本不匹配: 本地 %d, 远端 %d", localVersion, remoteVersion))
		},
		ConnectControlFailed: func(err error) {
			c.Error(fmt.Sprintf("控制通道连接失败: %v", err))
		},
		ConnectingTransferChannel: func(name, address, bind string) {
			c.Info(fmt.Sprintf("连接传输通道: %s -> %s (绑定: %s)", name, address, bind))
		},
		ConnectTransferFailed: func(name, address string, err error) {
			c.Error(fmt.Sprintf("传输通道连接失败: %s -> %s (%v)", name, address, err))
		},
		OutOfMemory: func(createdBuffers, requiredBuffers int, availableMB uint64, arch string) {
			c.Error(fmt.Sprintf("内存不足: 已创建 %d / 需要 %d (可用约 %dMB, 架构 %s)", createdBuffers, requiredBuffers, availableMB, arch))
		},
		RemoteOutOfMemory: func() {
			c.Error("远端内存不足，连接失败。")
		},
		ConnectSuccess: func(channelNames []string) {
			c.Info("连接成功，通道: " + strings.Join(channelNames, ", "))
		},
		Receiving: func() {
			c.Info("开始接收文件...")
		},
		Sending: func() {
			c.Info("开始发送文件...")
		},
		Exit: func() {
			c.Info("连接已关闭。")
		},
		SpeedInfo: func(info []hfx.TrafficInfo) {
			if len(info) == 0 {
				return
			}
			totalUp := int64(0)
			totalDown := int64(0)
			parts := make([]string, 0, len(info))
			for _, item := range info {
				totalUp += item.UploadTraffic
				totalDown += item.DownloadTraffic
				parts = append(parts, fmt.Sprintf("%s ↑%s ↓%s", item.Name, hfx.FormatSpeed(item.UploadTraffic), hfx.FormatSpeed(item.DownloadTraffic)))
			}
			summary := fmt.Sprintf("速度: %s | 总 ↑%s ↓%s", strings.Join(parts, " | "), hfx.FormatSpeed(totalUp), hfx.FormatSpeed(totalDown))
			c.Info(summary)
		},
		ChannelComplete: func(channel string, traffic int64, timeMs int64) {
			if timeMs == 0 {
				c.Info(fmt.Sprintf("通道完成: %s", channel))
				return
			}
			speed := hfx.FormatSpeed(traffic * 1000 / timeMs)
			c.Info(fmt.Sprintf("通道完成: %s (平均 %s)", channel, speed))
		},
		ChannelError: func(channel string, errType string, message string) {
			if message == "" {
				c.Error(fmt.Sprintf("通道错误: %s (%s)", channel, errType))
			} else {
				c.Error(fmt.Sprintf("通道错误: %s (%s) %s", channel, errType, message))
			}
		},
		ReadFileError: func(message string) {
			c.Error("读取文件失败: " + message)
		},
		WriteFileError: func(message string) {
			c.Error("写入文件失败: " + message)
		},
		Complete: func(isUpload bool, traffic int64, timeMs int64) {
			if timeMs == 0 {
				c.Info("传输完成。")
				return
			}
			speed := hfx.FormatSpeed(traffic * 1000 / timeMs)
			timeStr := hfx.FormatTime(timeMs)
			sizeStr := hfx.FormatFileSize(traffic)
			if isUpload {
				c.Info(fmt.Sprintf("上传完成: 速度 %s, 用时 %s, 总量 %s", speed, timeStr, sizeStr))
			} else {
				c.Info(fmt.Sprintf("下载完成: 速度 %s, 用时 %s, 总量 %s", speed, timeStr, sizeStr))
			}
		},
		Incomplete: func() {
			c.Error("传输未完成，可能有通道中断。")
		},
	}

	if checksumEnabled {
		cb.ChecksumComputed = func(path, sum string) {
			c.Info(fmt.Sprintf("校验: %s %s", path, sum))
		}
	}

	return cb
}
