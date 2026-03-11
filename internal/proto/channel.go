package proto

import (
    "bufio"
    "encoding/binary"
    "errors"
    "io"
    "net"
    "unicode/utf8"
)

type Channel struct {
    conn net.Conn
    rw   *bufio.ReadWriter
}

func NewChannel(conn net.Conn) *Channel {
    return &Channel{
        conn: conn,
        rw:   bufio.NewReadWriter(bufio.NewReader(conn), bufio.NewWriter(conn)),
    }
}

func (c *Channel) Close() error {
    return c.conn.Close()
}

func (c *Channel) Flush() error {
    return c.rw.Flush()
}

func (c *Channel) ReadFull(buf []byte) error {
    _, err := io.ReadFull(c.rw, buf)
    return err
}

func (c *Channel) ReadByte() (byte, error) {
    return c.rw.ReadByte()
}

func (c *Channel) ReadBool() (bool, error) {
    b, err := c.ReadByte()
    if err != nil {
        return false, err
    }
    return b != 0, nil
}

func (c *Channel) ReadInt16() (int16, error) {
    var buf [2]byte
    if err := c.ReadFull(buf[:]); err != nil {
        return 0, err
    }
    return int16(binary.BigEndian.Uint16(buf[:])), nil
}

func (c *Channel) ReadInt32() (int32, error) {
    var buf [4]byte
    if err := c.ReadFull(buf[:]); err != nil {
        return 0, err
    }
    return int32(binary.BigEndian.Uint32(buf[:])), nil
}

func (c *Channel) ReadInt64() (int64, error) {
    var buf [8]byte
    if err := c.ReadFull(buf[:]); err != nil {
        return 0, err
    }
    return int64(binary.BigEndian.Uint64(buf[:])), nil
}

func (c *Channel) ReadUTF() (string, error) {
    ulen, err := c.ReadUint16()
    if err != nil {
        return "", err
    }
    if ulen == 0 {
        return "", nil
    }
    buf := make([]byte, ulen)
    if err := c.ReadFull(buf); err != nil {
        return "", err
    }
    if !utf8.Valid(buf) {
        return "", errors.New("invalid utf-8 string")
    }
    return string(buf), nil
}

func (c *Channel) ReadUint16() (uint16, error) {
    v, err := c.ReadInt16()
    return uint16(v), err
}

func (c *Channel) WriteByte(v byte) error {
    return c.rw.WriteByte(v)
}

func (c *Channel) WriteBool(v bool) error {
    if v {
        return c.WriteByte(1)
    }
    return c.WriteByte(0)
}

func (c *Channel) WriteInt16(v int16) error {
    var buf [2]byte
    binary.BigEndian.PutUint16(buf[:], uint16(v))
    _, err := c.rw.Write(buf[:])
    return err
}

func (c *Channel) WriteInt32(v int32) error {
    var buf [4]byte
    binary.BigEndian.PutUint32(buf[:], uint32(v))
    _, err := c.rw.Write(buf[:])
    return err
}

func (c *Channel) WriteInt64(v int64) error {
    var buf [8]byte
    binary.BigEndian.PutUint64(buf[:], uint64(v))
    _, err := c.rw.Write(buf[:])
    return err
}

func (c *Channel) WriteBytes(b []byte) error {
    _, err := c.rw.Write(b)
    return err
}

func (c *Channel) WriteUTF(s string) error {
    if s == "" {
        return c.WriteUint16(0)
    }
    b := []byte(s)
    if len(b) > 65535 {
        return errors.New("string too long")
    }
    if err := c.WriteUint16(uint16(len(b))); err != nil {
        return err
    }
    _, err := c.rw.Write(b)
    return err
}

func (c *Channel) WriteUint16(v uint16) error {
    return c.WriteInt16(int16(v))
}
