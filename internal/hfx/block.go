package hfx

type BlockKind int

const (
    BlockData BlockKind = iota
    BlockEndPoint
    BlockInterrupt
    BlockReadError
    BlockWriteError
)

type FileBlock struct {
    Kind         BlockKind
    IsFile       bool
    FileIndex    int
    Path         string
    LastModified int64
    TotalSize    int64
    Index        int
    Data         []byte
    DataLen      int
}

func (b FileBlock) StartPosition() int64 {
    return int64(BlockSize) * int64(b.Index)
}
