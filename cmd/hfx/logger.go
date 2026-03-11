package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

type LogLevel int

const (
	LogError LogLevel = iota
	LogWarn
	LogInfo
	LogDebug
)

type Logger struct {
	mu    sync.Mutex
	level LogLevel
	file  *os.File
}

func NewLogger(path string, levelStr string) (*Logger, error) {
	level := parseLogLevel(levelStr)
	if strings.TrimSpace(path) == "" {
		return &Logger{level: level, file: nil}, nil
	}
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return nil, err
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0644)
	if err != nil {
		return nil, err
	}
	return &Logger{level: level, file: f}, nil
}

func (l *Logger) Close() {
	if l == nil || l.file == nil {
		return
	}
	_ = l.file.Close()
}

func (l *Logger) Info(msg string) {
	l.log(LogInfo, msg)
}

func (l *Logger) Warn(msg string) {
	l.log(LogWarn, msg)
}

func (l *Logger) Error(msg string) {
	l.log(LogError, msg)
}

func (l *Logger) Debug(msg string) {
	l.log(LogDebug, msg)
}

func (l *Logger) log(level LogLevel, msg string) {
	if l == nil || l.file == nil {
		return
	}
	if level > l.level {
		return
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	ts := time.Now().Format(time.RFC3339)
	_, _ = l.file.WriteString(fmt.Sprintf("%s [%s] %s\n", ts, level.String(), msg))
}

func parseLogLevel(s string) LogLevel {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "debug":
		return LogDebug
	case "info", "":
		return LogInfo
	case "warn", "warning":
		return LogWarn
	case "error":
		return LogError
	default:
		return LogInfo
	}
}

func (l LogLevel) String() string {
	switch l {
	case LogDebug:
		return "DEBUG"
	case LogInfo:
		return "INFO"
	case LogWarn:
		return "WARN"
	case LogError:
		return "ERROR"
	default:
		return "INFO"
	}
}
