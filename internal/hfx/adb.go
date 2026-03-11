package hfx

import (
    "bytes"
    "fmt"
    "os/exec"
)

func ADBForward(port int, device string) error {
    adbPath := "adb"
    if err := checkADB(adbPath); err != nil {
        adbPath = ".\\adb"
        if err := checkADB(adbPath); err != nil {
            return err
        }
    }

    args := []string{}
    if device != "" {
        args = append(args, "-s", device)
    }
    args = append(args, "forward", fmt.Sprintf("tcp:%d", port), fmt.Sprintf("tcp:%d", port))

    cmd := exec.Command(adbPath, args...)
    var stderr bytes.Buffer
    cmd.Stderr = &stderr
    if err := cmd.Run(); err != nil {
        if stderr.Len() > 0 {
            return fmt.Errorf("adb forward failed: %s", stderr.String())
        }
        return err
    }
    return nil
}

func checkADB(path string) error {
    cmd := exec.Command(path, "version")
    return cmd.Run()
}
