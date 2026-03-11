package hfx

import (
    "errors"
    "io"
    "os"
    "path/filepath"
    "sync"
)

type ReadFileCall struct {
    buffers            chan []byte
    blocks             chan FileBlock
    files              []RemoteFile
    localDir           Directory
    remoteDir          Directory
    operateThreadCount int

    mu      sync.Mutex
    fileIdx int
    done    chan struct{}
    err     error
}

func NewReadFileCall(buffers chan []byte, files []RemoteFile, localDir, remoteDir Directory, operateThreadCount int) *ReadFileCall {
    return &ReadFileCall{
        buffers:            buffers,
        blocks:             make(chan FileBlock, operateThreadCount*8+32),
        files:              files,
        localDir:           localDir,
        remoteDir:          remoteDir,
        operateThreadCount: operateThreadCount,
        fileIdx:            -1,
        done:               make(chan struct{}),
    }
}

func (r *ReadFileCall) Start() {
    go func() {
        defer close(r.done)
        if err := r.run(); err != nil {
            r.mu.Lock()
            r.err = err
            r.mu.Unlock()
            for i := 0; i < r.operateThreadCount; i++ {
                r.blocks <- FileBlock{Kind: BlockReadError, FileIndex: -1}
            }
            return
        }
        for i := 0; i < r.operateThreadCount; i++ {
            r.blocks <- FileBlock{Kind: BlockEndPoint, FileIndex: -1}
        }
    }()
}

func (r *ReadFileCall) Wait() error {
    <-r.done
    r.mu.Lock()
    defer r.mu.Unlock()
    return r.err
}

func (r *ReadFileCall) TakeBlock() (FileBlock, error) {
    block, ok := <-r.blocks
    if !ok {
        return FileBlock{}, io.EOF
    }
    return block, nil
}

func (r *ReadFileCall) RecycleBuffer(buf []byte) {
    if buf == nil {
        return
    }
    r.buffers <- buf
}

func (r *ReadFileCall) ShutdownByWriteError() {
    r.recycleAllBuffers()
    for i := 0; i < r.operateThreadCount; i++ {
        r.blocks <- FileBlock{Kind: BlockWriteError, FileIndex: -1}
    }
}

func (r *ReadFileCall) ShutdownByConnectionBreak() {
    r.recycleAllBuffers()
    for i := 0; i < r.operateThreadCount; i++ {
        r.blocks <- FileBlock{Kind: BlockInterrupt, FileIndex: -1}
    }
}

func (r *ReadFileCall) recycleAllBuffers() {
    // best-effort: drain any pending blocks that have buffers
    for {
        select {
        case blk := <-r.blocks:
            if blk.Data != nil {
                r.RecycleBuffer(blk.Data)
            }
        default:
            return
        }
    }
}

func (r *ReadFileCall) run() error {
    for _, file := range r.files {
        if !fileExists(file.Path) {
            continue
        }
        if err := r.readToBlocks(file); err != nil {
            return err
        }
        if file.IsDir {
            if err := r.walkDir(file.Path); err != nil {
                return err
            }
        }
    }
    return nil
}

func (r *ReadFileCall) walkDir(path string) error {
    entries, err := os.ReadDir(path)
    if err != nil {
        return err
    }
    for _, entry := range entries {
        info, err := entry.Info()
        if err != nil {
            continue
        }
        full := filepath.Join(path, entry.Name())
        rf := NewRemoteFile(info, full)
        if err := r.readToBlocks(rf); err != nil {
            return err
        }
        if rf.IsDir {
            if err := r.walkDir(full); err != nil {
                return err
            }
        }
    }
    return nil
}

func (r *ReadFileCall) readToBlocks(file RemoteFile) error {
    r.fileIdx++
    if file.IsDir {
        r.blocks <- FileBlock{
            Kind:         BlockData,
            IsFile:       false,
            FileIndex:    r.fileIdx,
            Path:         r.localDir.GenerateTransferPath(file.Path, r.remoteDir),
            LastModified: file.LastModified,
            TotalSize:    0,
            Index:        0,
            Data:         nil,
            DataLen:      0,
        }
        return nil
    }

    f, err := os.Open(file.Path)
    if err != nil {
        return err
    }
    defer f.Close()

    stat, err := f.Stat()
    if err != nil {
        return err
    }
    length := stat.Size()

    if length == 0 {
        buf := <-r.buffers
        r.blocks <- FileBlock{
            Kind:         BlockData,
            IsFile:       true,
            FileIndex:    r.fileIdx,
            Path:         r.localDir.GenerateTransferPath(file.Path, r.remoteDir),
            LastModified: file.LastModified,
            TotalSize:    length,
            Index:        0,
            Data:         buf,
            DataLen:      0,
        }
        return nil
    }

    remaining := length
    index := 0
    for remaining > 0 {
        blkSize := int64(BlockSize)
        if remaining < blkSize {
            blkSize = remaining
        }
        buf := <-r.buffers
        n, err := io.ReadFull(f, buf[:blkSize])
        if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrUnexpectedEOF) {
            r.buffers <- buf
            return err
        }
        r.blocks <- FileBlock{
            Kind:         BlockData,
            IsFile:       true,
            FileIndex:    r.fileIdx,
            Path:         r.localDir.GenerateTransferPath(file.Path, r.remoteDir),
            LastModified: file.LastModified,
            TotalSize:    length,
            Index:        index,
            Data:         buf,
            DataLen:      n,
        }
        remaining -= int64(n)
        index++
    }
    return nil
}

func fileExists(path string) bool {
    _, err := os.Stat(path)
    return err == nil
}
