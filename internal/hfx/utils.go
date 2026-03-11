package hfx

import (
	"fmt"
	"os"
	"path/filepath"

	"nekotrans/internal/proto"
)

type RemoteFile struct {
	Name         string
	Path         string
	LastModified int64
	Size         int64
	IsDir        bool
}

func NewRemoteFile(info os.FileInfo, path string) RemoteFile {
	return RemoteFile{
		Name:         info.Name(),
		Path:         path,
		LastModified: info.ModTime().UnixMilli(),
		Size:         info.Size(),
		IsDir:        info.IsDir(),
	}
}

func ListRemoteFiles(path string) ([]RemoteFile, error) {
	entries, err := os.ReadDir(path)
	if err != nil {
		return nil, err
	}
	out := make([]RemoteFile, 0, len(entries))
	for _, entry := range entries {
		info, err := entry.Info()
		if err != nil {
			continue
		}
		full := filepath.Join(path, entry.Name())
		out = append(out, NewRemoteFile(info, full))
	}
	return out, nil
}

type TrafficInfo struct {
	Name            string
	UploadTraffic   int64
	DownloadTraffic int64
}

type TransferConnection struct {
	Name           string
	Channel        *proto.Channel
	currentTraffic TrafficInfo
	totalTraffic   TrafficInfo
}

func NewTransferConnection(name string, ch *proto.Channel) *TransferConnection {
	return &TransferConnection{
		Name:           name,
		Channel:        ch,
		currentTraffic: TrafficInfo{Name: name},
		totalTraffic:   TrafficInfo{Name: name},
	}
}

func (t *TransferConnection) AddUploaded(n int64) {
	t.currentTraffic.UploadTraffic += n
	t.totalTraffic.UploadTraffic += n
}

func (t *TransferConnection) AddDownloaded(n int64) {
	t.currentTraffic.DownloadTraffic += n
	t.totalTraffic.DownloadTraffic += n
}

func (t *TransferConnection) ResetCurrentTraffic() TrafficInfo {
	info := t.currentTraffic
	t.currentTraffic = TrafficInfo{Name: t.Name}
	return info
}

func (t *TransferConnection) ResetTotalTraffic() TrafficInfo {
	info := t.totalTraffic
	t.totalTraffic = TrafficInfo{Name: t.Name}
	return info
}

func (t *TransferConnection) TotalTraffic() TrafficInfo {
	return t.totalTraffic
}

func (t *TransferConnection) Close() error {
	return t.Channel.Close()
}

func FormatSpeed(bytesPerSecond int64) string {
	if bytesPerSecond < 1024 {
		return fmt.Sprintf("%dB/s", bytesPerSecond)
	}
	if bytesPerSecond < 1024*1024 {
		return fmt.Sprintf("%.2fKB/s", float64(bytesPerSecond)/1024.0)
	}
	return fmt.Sprintf("%.2fMB/s", float64(bytesPerSecond)/(1024.0*1024.0))
}

func FormatFileSize(size int64) string {
	if size < 1024 {
		return fmt.Sprintf("%dB", size)
	}
	if size < 1024*1024 {
		return fmt.Sprintf("%.2fKB", float64(size)/1024.0)
	}
	if size < 1024*1024*1024 {
		return fmt.Sprintf("%.2fMB", float64(size)/(1024.0*1024.0))
	}
	return fmt.Sprintf("%.2fGB", float64(size)/(1024.0*1024.0*1024.0))
}

func FormatTime(ms int64) string {
	if ms < 1000 {
		return fmt.Sprintf("%dms", ms)
	}
	totalSeconds := ms / 1000
	hours := totalSeconds / 3600
	minutes := (totalSeconds % 3600) / 60
	seconds := totalSeconds % 60
	if hours == 0 {
		return fmt.Sprintf("%02d:%02d", minutes, seconds)
	}
	return fmt.Sprintf("%02d:%02d:%02d", hours, minutes, seconds)
}
