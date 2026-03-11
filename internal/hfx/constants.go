package hfx

const (
    ClientHeader = "HFXC"
    VersionCode  = 300
    BlockSize    = 1024 * 1024
)

const (
    ControllerShutdown      int16 = 0
    ControllerListFiles     int16 = 1
    ControllerDeleteFile    int16 = 2
    ControllerMkdir         int16 = 3
    ControllerRequestRecv   int16 = 10
    ControllerRequestSend   int16 = 11
)

const (
    TransferEndPoint        int16 = -1
    TransferFile            int16 = 0
    TransferFolder          int16 = 1
    TransferFileSlice       int16 = 2
    TransferEOF             int16 = 3
    TransferEndInterrupted  int16 = 4
    TransferEndReadError    int16 = 5
    TransferEndWriteError   int16 = 6
)
