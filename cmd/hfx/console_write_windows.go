//go:build windows

package main

import (
    "os"
    "strings"
    "syscall"
    "unicode/utf16"
    "unsafe"
)

const (
    stdOutputHandle = uint32(0xFFFFFFF5) // -11
    stdErrorHandle  = uint32(0xFFFFFFF4) // -12
    fileTypeChar    = 0x0002
)

var (
    kernel32             = syscall.NewLazyDLL("kernel32.dll")
    procGetStdHandle     = kernel32.NewProc("GetStdHandle")
    procWriteConsoleW    = kernel32.NewProc("WriteConsoleW")
    procGetFileType      = kernel32.NewProc("GetFileType")
)

func writeStdoutLine(s string) {
    writeConsoleLine(stdOutputHandle, os.Stdout, s)
}

func writeStderrLine(s string) {
    writeConsoleLine(stdErrorHandle, os.Stderr, s)
}

func writeConsoleLine(stdHandle uint32, fallback *os.File, s string) {
    handle, _, _ := procGetStdHandle.Call(uintptr(stdHandle))
    if handle == 0 {
        fallback.WriteString(s + "\n")
        return
    }
    ftype, _, _ := procGetFileType.Call(handle)
    if ftype != fileTypeChar {
        fallback.WriteString(s + "\n")
        return
    }

    text := strings.ReplaceAll(s, "\n", "\r\n") + "\r\n"
    u16 := utf16.Encode([]rune(text))
    if len(u16) == 0 {
        return
    }
    var written uint32
    _, _, _ = procWriteConsoleW.Call(handle, uintptr(unsafe.Pointer(&u16[0])), uintptr(len(u16)), uintptr(unsafe.Pointer(&written)), 0)
}
