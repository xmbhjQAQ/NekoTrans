//go:build !windows

package main

import (
    "os"
)

func writeStdoutLine(s string) {
    os.Stdout.WriteString(s + "\n")
}

func writeStderrLine(s string) {
    os.Stderr.WriteString(s + "\n")
}
