package hfx

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"hash"
	"io"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type WriteFileCall struct {
	buffers      chan []byte
	queues       [][]FileBlock
	channelDone  []bool
	canceled     bool
	mu           sync.Mutex
	cond         *sync.Cond
	lastPath     string
	lastFile     *os.File
	lastModified int64
	cursor       int64
	resume       bool
	checksum     bool
	checksumFn   func(path string, sum string)
	hasher       hash.Hash
	resumeSize   int64
}

func NewWriteFileCall(buffers chan []byte, dequeCount int, resume bool, checksum bool, checksumFn func(path string, sum string)) *WriteFileCall {
	w := &WriteFileCall{
		buffers:     buffers,
		queues:      make([][]FileBlock, dequeCount),
		channelDone: make([]bool, dequeCount),
		resume:      resume,
		checksum:    checksum,
		checksumFn:  checksumFn,
	}
	w.cond = sync.NewCond(&w.mu)
	return w
}

func (w *WriteFileCall) Start() error {
	errCh := make(chan error, 1)
	go func() {
		errCh <- w.run()
	}()
	return <-errCh
}

func (w *WriteFileCall) GetBuffer() []byte {
	return <-w.buffers
}

func (w *WriteFileCall) PutBlock(block FileBlock, idx int) {
	w.mu.Lock()
	w.queues[idx] = append(w.queues[idx], block)
	w.cond.Signal()
	w.mu.Unlock()
}

func (w *WriteFileCall) FinishChannel(idx int) {
	w.mu.Lock()
	w.channelDone[idx] = true
	w.cond.Signal()
	w.mu.Unlock()
}

func (w *WriteFileCall) Cancel() {
	w.mu.Lock()
	w.canceled = true
	for i := range w.queues {
		for _, blk := range w.queues[i] {
			if blk.Data != nil {
				w.buffers <- blk.Data
			}
		}
		w.queues[i] = nil
	}
	w.cond.Signal()
	w.mu.Unlock()
}

func (w *WriteFileCall) run() error {
	defer w.closeLastFile()
	for {
		blk := w.takeBlock()
		if blk == nil {
			return nil
		}
		if !blk.IsFile {
			if err := w.tryMkdirs(blk.Path); err != nil {
				w.Cancel()
				return err
			}
			if err := setFileLastModified(blk.Path, blk.LastModified); err != nil {
				_ = err
			}
			continue
		}

		if err := w.createParentDirIfNotExists(blk.Path); err != nil {
			w.Cancel()
			return err
		}

		if w.lastPath != blk.Path {
			w.closeLastFile()
			w.resumeSize = 0
			if w.resume {
				if info, err := os.Stat(blk.Path); err == nil && !info.IsDir() {
					w.resumeSize = info.Size()
				}
			}
			file, err := createAndOpenFile(blk.Path, blk.TotalSize, w.resume)
			if err != nil {
				w.Cancel()
				return err
			}
			w.lastFile = file
			w.lastPath = blk.Path
			w.lastModified = blk.LastModified
			w.cursor = 0
			if w.resumeSize > blk.TotalSize {
				w.resumeSize = blk.TotalSize
			}
			if w.checksum {
				w.hasher = sha256.New()
				if w.resume && w.resumeSize > 0 {
					if err := seedHasherFromFile(w.hasher, blk.Path, w.resumeSize); err != nil {
						w.Cancel()
						return err
					}
				}
			}
		}

		if w.lastFile == nil {
			w.Cancel()
			return errors.New("file not opened")
		}

		if w.resume && w.resumeSize > 0 {
			if blk.StartPosition()+int64(blk.DataLen) <= w.resumeSize {
				if blk.Data != nil {
					w.buffers <- blk.Data
				}
				continue
			}
		}

		if w.cursor != blk.StartPosition() {
			if _, err := w.lastFile.Seek(blk.StartPosition(), 0); err != nil {
				w.Cancel()
				return err
			}
			w.cursor = blk.StartPosition()
		}

		if blk.DataLen > 0 {
			if _, err := w.lastFile.Write(blk.Data[:blk.DataLen]); err != nil {
				w.Cancel()
				return err
			}
			if w.hasher != nil {
				_, _ = w.hasher.Write(blk.Data[:blk.DataLen])
			}
			w.cursor += int64(blk.DataLen)
		}

		if blk.Data != nil {
			w.buffers <- blk.Data
		}
	}
}

func (w *WriteFileCall) closeLastFile() {
	if w.lastFile != nil {
		_ = w.lastFile.Close()
		_ = setFileLastModified(w.lastPath, w.lastModified)
		if w.checksum && w.hasher != nil {
			sum := hex.EncodeToString(w.hasher.Sum(nil))
			if w.checksumFn != nil {
				w.checksumFn(w.lastPath, sum)
			}
		}
		w.lastFile = nil
		w.lastPath = ""
		w.cursor = 0
		w.hasher = nil
		w.resumeSize = 0
	}
}

func (w *WriteFileCall) takeBlock() *FileBlock {
	w.mu.Lock()
	defer w.mu.Unlock()

	for {
		blk, idx := w.tryTakeMinBlock()
		if blk != nil {
			if idx >= 0 {
				w.queues[idx] = w.queues[idx][1:]
			}
			return blk
		}
		if w.canceled || (w.allChannelsFinished() && w.allQueuesEmpty()) {
			return nil
		}
		w.cond.Wait()
	}
}

func (w *WriteFileCall) tryTakeMinBlock() (*FileBlock, int) {
	var min *FileBlock
	minIdx := -1
	for i := range w.queues {
		if len(w.queues[i]) == 0 {
			continue
		}
		head := w.queues[i][0]
		if min == nil || compareBlock(head, *min) < 0 {
			tmp := head
			min = &tmp
			minIdx = i
		}
	}
	return min, minIdx
}

func compareBlock(a, b FileBlock) int {
	if a.FileIndex != b.FileIndex {
		if a.FileIndex < b.FileIndex {
			return -1
		}
		return 1
	}
	if a.Index < b.Index {
		return -1
	}
	if a.Index > b.Index {
		return 1
	}
	return 0
}

func (w *WriteFileCall) allChannelsFinished() bool {
	for _, done := range w.channelDone {
		if !done {
			return false
		}
	}
	return true
}

func (w *WriteFileCall) allQueuesEmpty() bool {
	for _, q := range w.queues {
		if len(q) > 0 {
			return false
		}
	}
	return true
}

func (w *WriteFileCall) createParentDirIfNotExists(path string) error {
	parent := filepath.Dir(path)
	if parent == "." {
		return nil
	}
	return mkdirOrThrow(parent)
}

func (w *WriteFileCall) tryMkdirs(path string) error {
	return mkdirOrThrow(path)
}

func mkdirOrThrow(path string) error {
	info, err := os.Stat(path)
	if err == nil {
		if !info.IsDir() {
			if err := os.Remove(path); err != nil {
				return err
			}
			return os.MkdirAll(path, 0755)
		}
		return nil
	}
	return os.MkdirAll(path, 0755)
}

func createAndOpenFile(path string, length int64, resume bool) (*os.File, error) {
	flags := os.O_CREATE | os.O_RDWR
	if !resume {
		flags |= os.O_TRUNC
	}
	file, err := os.OpenFile(path, flags, 0644)
	if err != nil {
		return nil, err
	}
	if err := file.Truncate(length); err != nil {
		_ = file.Close()
		return nil, err
	}
	return file, nil
}

func seedHasherFromFile(hasher hash.Hash, path string, size int64) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()

	buf := make([]byte, 1024*1024)
	remaining := size
	for remaining > 0 {
		chunk := int64(len(buf))
		if remaining < chunk {
			chunk = remaining
		}
		n, err := io.ReadFull(f, buf[:chunk])
		if n > 0 {
			_, _ = hasher.Write(buf[:n])
			remaining -= int64(n)
		}
		if err != nil {
			if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
				break
			}
			return err
		}
	}
	return nil
}

func setFileLastModified(path string, ms int64) error {
	if ms <= 0 {
		return nil
	}
	t := time.UnixMilli(ms)
	return os.Chtimes(path, t, t)
}
