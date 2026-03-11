package hfx

import (
    "errors"
    "time"
)

func sendFileCall(reader *ReadFileCall, conn *TransferConnection, callbacks Callbacks) error {
    ch := conn.Channel
    start := time.Now()
    conn.ResetTotalTraffic()

    for {
        block, err := reader.TakeBlock()
        if err != nil {
            return err
        }
        if block.FileIndex == -1 {
            switch block.Kind {
            case BlockEndPoint:
                if err := ch.WriteInt16(TransferEOF); err != nil {
                    return err
                }
                if err := ch.Flush(); err != nil {
                    return err
                }
                if callbacks.ChannelComplete != nil {
                    callbacks.ChannelComplete(conn.Name, conn.TotalTraffic().UploadTraffic, time.Since(start).Milliseconds())
                }
            case BlockInterrupt:
                _ = ch.WriteInt16(TransferEndInterrupted)
                _ = ch.Flush()
                if callbacks.ChannelError != nil {
                    callbacks.ChannelError(conn.Name, "interrupt", "")
                }
            case BlockReadError:
                _ = ch.WriteInt16(TransferEndReadError)
                _ = ch.Flush()
                if callbacks.ChannelError != nil {
                    callbacks.ChannelError(conn.Name, "read_error", "")
                }
            case BlockWriteError:
                _ = ch.WriteInt16(TransferEndWriteError)
                _ = ch.Flush()
                if callbacks.ChannelError != nil {
                    callbacks.ChannelError(conn.Name, "write_error", "")
                }
            }
            break
        }
        if block.IsFile {
            if err := ch.WriteInt16(TransferFile); err != nil {
                return err
            }
        } else {
            if err := ch.WriteInt16(TransferFolder); err != nil {
                return err
            }
        }
        if err := ch.WriteInt32(int32(block.FileIndex)); err != nil {
            return err
        }
        if err := ch.WriteUTF(block.Path); err != nil {
            return err
        }
        if err := ch.WriteInt64(block.LastModified); err != nil {
            return err
        }
        if !block.IsFile {
            if err := ch.Flush(); err != nil {
                return err
            }
            continue
        }
        if err := ch.WriteInt64(block.TotalSize); err != nil {
            return err
        }
        if err := ch.WriteInt32(int32(block.Index)); err != nil {
            return err
        }
        if err := ch.WriteInt32(int32(block.DataLen)); err != nil {
            return err
        }

        if callbacks.FileUploading != nil {
            callbacks.FileUploading(conn.Name, block.Path, block.StartPosition()+int64(block.DataLen), block.TotalSize)
        }

        if block.DataLen > 0 {
            if err := ch.WriteBytes(block.Data[:block.DataLen]); err != nil {
                reader.RecycleBuffer(block.Data)
                return err
            }
        }
        if err := ch.Flush(); err != nil {
            reader.RecycleBuffer(block.Data)
            return err
        }
        reader.RecycleBuffer(block.Data)
        conn.AddUploaded(int64(block.DataLen))
    }
    return nil
}

func receiveFileCall(idx int, conn *TransferConnection, writer *WriteFileCall, callbacks Callbacks) error {
    ch := conn.Channel
    start := time.Now()
    conn.ResetTotalTraffic()

    for {
        header, err := ch.ReadInt16()
        if err != nil {
            writer.FinishChannel(idx)
            if callbacks.ChannelError != nil {
                callbacks.ChannelError(conn.Name, "exception", err.Error())
            }
            return err
        }
        switch header {
        case TransferFolder:
            fileIndex, err := ch.ReadInt32()
            if err != nil {
                return err
            }
            path, err := ch.ReadUTF()
            if err != nil {
                return err
            }
            lastModified, err := ch.ReadInt64()
            if err != nil {
                return err
            }
            writer.PutBlock(FileBlock{
                Kind:         BlockData,
                IsFile:       false,
                FileIndex:    int(fileIndex),
                Path:         path,
                LastModified: lastModified,
            }, idx)
        case TransferFile:
            fileIndex, err := ch.ReadInt32()
            if err != nil {
                return err
            }
            path, err := ch.ReadUTF()
            if err != nil {
                return err
            }
            lastModified, err := ch.ReadInt64()
            if err != nil {
                return err
            }
            totalSize, err := ch.ReadInt64()
            if err != nil {
                return err
            }
            index, err := ch.ReadInt32()
            if err != nil {
                return err
            }
            length, err := ch.ReadInt32()
            if err != nil {
                return err
            }

            if callbacks.FileDownloading != nil {
                callbacks.FileDownloading(conn.Name, path, int64(index)*int64(BlockSize)+int64(length), totalSize)
            }

            buf := writer.GetBuffer()
            if length > int32(len(buf)) {
                return errors.New("block length exceeds buffer size")
            }
            if length > 0 {
                if err := ch.ReadFull(buf[:length]); err != nil {
                    return err
                }
                conn.AddDownloaded(int64(length))
            }
            writer.PutBlock(FileBlock{
                Kind:         BlockData,
                IsFile:       true,
                FileIndex:    int(fileIndex),
                Path:         path,
                LastModified: lastModified,
                TotalSize:    totalSize,
                Index:        int(index),
                Data:         buf,
                DataLen:      int(length),
            }, idx)
        case TransferEOF:
            writer.FinishChannel(idx)
            if callbacks.ChannelComplete != nil {
                callbacks.ChannelComplete(conn.Name, conn.TotalTraffic().DownloadTraffic, time.Since(start).Milliseconds())
            }
            return nil
        case TransferEndInterrupted:
            writer.Cancel()
            if callbacks.ChannelError != nil {
                callbacks.ChannelError(conn.Name, "interrupt", "")
            }
            return nil
        case TransferEndReadError:
            writer.Cancel()
            if callbacks.ChannelError != nil {
                callbacks.ChannelError(conn.Name, "read_error", "")
            }
            return nil
        case TransferEndWriteError:
            writer.Cancel()
            if callbacks.ChannelError != nil {
                callbacks.ChannelError(conn.Name, "write_error", "")
            }
            return nil
        default:
            return errors.New("unknown transfer header")
        }
    }
}
