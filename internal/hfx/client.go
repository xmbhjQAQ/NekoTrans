package hfx

import (
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"

	"nekotrans/internal/proto"
)

type Callbacks struct {
	ConnectingControlChannel  func(address string, port int)
	VersionMismatch           func(localVersion, remoteVersion int)
	ConnectControlFailed      func(err error)
	ConnectingTransferChannel func(name, address, bind string)
	ConnectTransferFailed     func(name, address string, err error)
	OutOfMemory               func(createdBuffers, requiredBuffers int, availableMB uint64, arch string)
	RemoteOutOfMemory         func()
	ConnectSuccess            func(channelNames []string)

	Receiving func()
	Sending   func()
	Exit      func()

	FileUploading   func(channel, path string, targetSize, totalSize int64)
	FileDownloading func(channel, path string, targetSize, totalSize int64)
	SpeedInfo       func(info []TrafficInfo)

	ChannelComplete func(channel string, traffic int64, timeMs int64)
	ChannelError    func(channel string, errType string, message string)

	ReadFileError  func(message string)
	WriteFileError func(message string)

	Complete   func(isUpload bool, traffic int64, timeMs int64)
	Incomplete func()

	ChecksumComputed func(path string, sum string)
}

type Client struct {
	serverAddr  string
	port        int
	homeDir     string
	maxBuffers  int
	dialTimeout time.Duration
	resume      bool
	checksum    bool

	control     *proto.Channel
	connections []*TransferConnection
	buffers     chan []byte

	callbacks Callbacks
}

func NewClient(serverAddr string, port int, homeDir string, maxBuffers int, dialTimeout time.Duration, resume bool, checksum bool, callbacks Callbacks) *Client {
	return &Client{
		serverAddr:  serverAddr,
		port:        port,
		homeDir:     homeDir,
		maxBuffers:  maxBuffers,
		dialTimeout: dialTimeout,
		resume:      resume,
		checksum:    checksum,
		callbacks:   callbacks,
	}
}

func (c *Client) Connect() error {
	if c.callbacks.ConnectingControlChannel != nil {
		c.callbacks.ConnectingControlChannel(c.serverAddr, c.port)
	}
	timeout := c.dialTimeout
	if timeout == 0 {
		timeout = 8 * time.Second
	}
	dialer := net.Dialer{Timeout: timeout}
	conn, err := dialer.Dial("tcp", fmt.Sprintf("%s:%d", c.serverAddr, c.port))
	if err != nil {
		if c.callbacks.ConnectControlFailed != nil {
			c.callbacks.ConnectControlFailed(err)
		}
		return err
	}
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetKeepAlive(true)
		_ = tcp.SetKeepAlivePeriod(30 * time.Second)
	}
	c.control = proto.NewChannel(conn)

	if err := c.control.WriteBytes([]byte(ClientHeader)); err != nil {
		return err
	}
	if err := c.control.WriteInt32(int32(VersionCode)); err != nil {
		return err
	}
	if err := c.control.Flush(); err != nil {
		return err
	}
	ok, err := c.control.ReadBool()
	if err != nil {
		if c.callbacks.ConnectControlFailed != nil {
			c.callbacks.ConnectControlFailed(err)
		}
		return err
	}
	if !ok {
		remoteVer, _ := c.control.ReadInt32()
		if c.callbacks.VersionMismatch != nil {
			c.callbacks.VersionMismatch(VersionCode, int(remoteVer))
		}
		c.control.Close()
		return errors.New("version mismatch")
	}

	ipCount32, err := c.control.ReadInt32()
	if err != nil {
		return err
	}
	ipCount := int(ipCount32)

	names := make([]string, ipCount)
	addrs := make([]net.IP, ipCount)
	binds := make([]net.IP, ipCount)

	for i := 0; i < ipCount; i++ {
		name, err := c.control.ReadUTF()
		if err != nil {
			return err
		}
		addrLen, err := c.control.ReadByte()
		if err != nil {
			return err
		}
		addrBytes := make([]byte, int(addrLen))
		if err := c.control.ReadFull(addrBytes); err != nil {
			return err
		}
		addr := net.IP(addrBytes)
		bindLen, err := c.control.ReadByte()
		if err != nil {
			return err
		}
		var bind net.IP
		if bindLen != 0 {
			bindBytes := make([]byte, int(bindLen))
			if err := c.control.ReadFull(bindBytes); err != nil {
				return err
			}
			bind = net.IP(bindBytes)
		}
		names[i] = name
		addrs[i] = addr
		binds[i] = bind
	}

	c.connections = make([]*TransferConnection, 0, ipCount)
	for i := 0; i < ipCount; i++ {
		name := names[i]
		addr := addrs[i]
		bind := binds[i]
		if c.callbacks.ConnectingTransferChannel != nil {
			bindStr := "null"
			if bind != nil {
				bindStr = bind.String()
			}
			c.callbacks.ConnectingTransferChannel(name, addr.String(), bindStr)
		}

		var tconn net.Conn
		if bind == nil {
			tconn, err = dialer.Dial("tcp", fmt.Sprintf("%s:%d", addr.String(), c.port))
		} else {
			dialer.LocalAddr = &net.TCPAddr{IP: bind, Port: 0}
			tconn, err = dialer.Dial("tcp", fmt.Sprintf("%s:%d", addr.String(), c.port))
			dialer.LocalAddr = nil
		}
		if err != nil {
			if c.callbacks.ConnectTransferFailed != nil {
				c.callbacks.ConnectTransferFailed(name, addr.String(), err)
			}
			_ = c.control.WriteBool(false)
			_ = c.control.WriteUTF(name)
			_ = c.control.Flush()
			_ = c.control.Close()
			return err
		}
		tchannel := proto.NewChannel(tconn)
		if tcp, ok := tconn.(*net.TCPConn); ok {
			_ = tcp.SetKeepAlive(true)
			_ = tcp.SetKeepAlivePeriod(30 * time.Second)
		}
		c.connections = append(c.connections, NewTransferConnection(name, tchannel))
		if err := c.control.WriteBool(true); err != nil {
			return err
		}
		if err := c.control.WriteUTF(name); err != nil {
			return err
		}
		if err := c.control.Flush(); err != nil {
			return err
		}
		if _, err := c.control.ReadBool(); err != nil {
			return err
		}
	}

	bufferCount32, err := c.control.ReadInt32()
	if err != nil {
		return err
	}
	bufferCount := int(bufferCount32)
	if c.maxBuffers > 0 && bufferCount > c.maxBuffers {
		if c.callbacks.OutOfMemory != nil {
			c.callbacks.OutOfMemory(0, bufferCount, availableMemoryMB(), runtime.GOARCH)
		}
		_ = c.control.WriteBool(false)
		_ = c.control.Flush()
		return errors.New("buffer request exceeds limit")
	}
	c.buffers = make(chan []byte, bufferCount)
	for i := 0; i < bufferCount; i++ {
		c.buffers <- make([]byte, BlockSize)
	}
	if err := c.control.WriteBool(true); err != nil {
		return err
	}
	if err := c.control.Flush(); err != nil {
		return err
	}
	remoteOK, err := c.control.ReadBool()
	if err != nil {
		return err
	}
	if !remoteOK {
		if c.callbacks.RemoteOutOfMemory != nil {
			c.callbacks.RemoteOutOfMemory()
		}
		return errors.New("remote out of memory")
	}

	if err := c.control.WriteInt32(int32(CurrentFileSystem())); err != nil {
		return err
	}
	if err := c.control.WriteUTF(c.homeDir); err != nil {
		return err
	}
	if err := c.control.Flush(); err != nil {
		return err
	}

	channelNames := make([]string, 0, len(c.connections))
	for _, conn := range c.connections {
		channelNames = append(channelNames, conn.Name)
	}
	if c.callbacks.ConnectSuccess != nil {
		c.callbacks.ConnectSuccess(channelNames)
	}
	return nil
}

func (c *Client) Start() error {
	for {
		id, err := c.control.ReadInt16()
		if err != nil {
			return err
		}
		switch id {
		case ControllerListFiles:
			if err := c.handleListFiles(); err != nil {
				return err
			}
		case ControllerDeleteFile:
			if err := c.handleDeleteFile(); err != nil {
				return err
			}
		case ControllerMkdir:
			if err := c.handleMkdir(); err != nil {
				return err
			}
		case ControllerRequestRecv:
			if err := c.handleReceiveFiles(); err != nil {
				return err
			}
		case ControllerRequestSend:
			if err := c.handleSendFiles(); err != nil {
				return err
			}
		case ControllerShutdown:
			c.handleShutdown()
			return nil
		}
	}
}

func (c *Client) handleShutdown() {
	_ = c.control.Close()
	for _, conn := range c.connections {
		_ = conn.Close()
	}
	if c.callbacks.Exit != nil {
		c.callbacks.Exit()
	}
}

func (c *Client) handleDeleteFile() error {
	path, err := c.control.ReadUTF()
	if err != nil {
		return err
	}
	ok := deleteLocalFile(path)
	if err := c.control.WriteBool(ok); err != nil {
		return err
	}
	return c.control.Flush()
}

func (c *Client) handleMkdir() error {
	parent, err := c.control.ReadUTF()
	if err != nil {
		return err
	}
	child, err := c.control.ReadUTF()
	if err != nil {
		return err
	}
	target := filepath.Join(parent, child)
	err = os.MkdirAll(target, 0755)
	if err := c.control.WriteBool(err == nil); err != nil {
		return err
	}
	return c.control.Flush()
}

func (c *Client) handleListFiles() error {
	path, err := c.control.ReadUTF()
	if err != nil {
		return err
	}
	if path != "/" {
		files, err := ListRemoteFiles(path)
		if err != nil {
			if err := c.control.WriteInt32(-1); err != nil {
				return err
			}
			return c.control.Flush()
		}
		if err := c.control.WriteInt32(int32(len(files))); err != nil {
			return err
		}
		for _, file := range files {
			if err := writeRemoteFile(c.control, file); err != nil {
				return err
			}
		}
		return c.control.Flush()
	}

	roots := listRoots()
	if len(roots) == 1 && roots[0] == "/" {
		files, err := ListRemoteFiles(roots[0])
		if err != nil {
			if err := c.control.WriteInt32(-1); err != nil {
				return err
			}
			return c.control.Flush()
		}
		if err := c.control.WriteInt32(int32(len(files))); err != nil {
			return err
		}
		for _, file := range files {
			if err := writeRemoteFile(c.control, file); err != nil {
				return err
			}
		}
		return c.control.Flush()
	}

	if err := c.control.WriteInt32(int32(len(roots))); err != nil {
		return err
	}
	for _, root := range roots {
		info, err := os.Stat(root)
		if err != nil {
			continue
		}
		if err := c.control.WriteUTF(root); err != nil {
			return err
		}
		if err := c.control.WriteUTF(root); err != nil {
			return err
		}
		if err := c.control.WriteInt64(info.ModTime().UnixMilli()); err != nil {
			return err
		}
		if err := c.control.WriteInt64(info.Size()); err != nil {
			return err
		}
		if err := c.control.WriteBool(info.IsDir()); err != nil {
			return err
		}
	}
	return c.control.Flush()
}

func (c *Client) handleReceiveFiles() error {
	if c.callbacks.Receiving != nil {
		c.callbacks.Receiving()
	}
	ok := c.receiveFiles()
	if !ok {
		return errors.New("receive files failed")
	}
	return nil
}

func (c *Client) handleSendFiles() error {
	listSize32, err := c.control.ReadInt32()
	if err != nil {
		return err
	}
	listSize := int(listSize32)
	fileList := make([]RemoteFile, 0, listSize)
	for i := 0; i < listSize; i++ {
		path, err := c.control.ReadUTF()
		if err != nil {
			return err
		}
		info, err := os.Stat(path)
		if err != nil {
			continue
		}
		fileList = append(fileList, NewRemoteFile(info, path))
	}
	remoteDirPath, err := c.control.ReadUTF()
	if err != nil {
		return err
	}
	remoteDirFS32, err := c.control.ReadInt32()
	if err != nil {
		return err
	}
	localDirPath, err := c.control.ReadUTF()
	if err != nil {
		return err
	}

	remoteDir := NewDirectory(remoteDirPath, int(remoteDirFS32))
	localDir := NewDirectory(localDirPath, CurrentFileSystem())

	if c.callbacks.Sending != nil {
		c.callbacks.Sending()
	}
	ok := c.sendFiles(fileList, localDir, remoteDir)
	if !ok {
		return errors.New("send files failed")
	}
	return nil
}

func (c *Client) sendFiles(files []RemoteFile, localDir, remoteDir Directory) bool {
	reader := NewReadFileCall(c.buffers, files, localDir, remoteDir, len(c.connections))
	reader.Start()

	monitorStop := startSpeedMonitor(c.connections, c.callbacks)
	start := time.Now()

	var wg sync.WaitGroup
	errCh := make(chan error, len(c.connections))
	for _, conn := range c.connections {
		wg.Add(1)
		go func(tc *TransferConnection) {
			defer wg.Done()
			if err := sendFileCall(reader, tc, c.callbacks); err != nil {
				errCh <- err
			}
		}(conn)
	}

	complete, err := c.control.ReadBool()
	if err != nil {
		monitorStop()
		if c.callbacks.Incomplete != nil {
			c.callbacks.Incomplete()
		}
		return false
	}
	if !complete {
		msg, _ := c.control.ReadUTF()
		monitorStop()
		if c.callbacks.WriteFileError != nil {
			c.callbacks.WriteFileError(msg)
		}
		reader.ShutdownByWriteError()
		return true
	}

	wg.Wait()
	select {
	case <-errCh:
		monitorStop()
		if c.callbacks.Incomplete != nil {
			c.callbacks.Incomplete()
		}
		return false
	default:
	}

	if err := reader.Wait(); err != nil {
		_ = c.control.WriteBool(false)
		_ = c.control.WriteUTF(err.Error())
		_ = c.control.Flush()
		monitorStop()
		if c.callbacks.ReadFileError != nil {
			c.callbacks.ReadFileError(err.Error())
		}
		return true
	}

	_ = c.control.WriteBool(true)
	_ = c.control.Flush()

	totalUpload := int64(0)
	for _, conn := range c.connections {
		totalUpload += conn.ResetTotalTraffic().UploadTraffic
	}
	monitorStop()

	if c.callbacks.Complete != nil {
		c.callbacks.Complete(true, totalUpload, time.Since(start).Milliseconds())
	}
	return true
}

func (c *Client) receiveFiles() bool {
	writer := NewWriteFileCall(c.buffers, len(c.connections), c.resume, c.checksum, c.callbacks.ChecksumComputed)

	monitorStop := startSpeedMonitor(c.connections, c.callbacks)
	start := time.Now()

	var wg sync.WaitGroup
	errCh := make(chan error, len(c.connections))
	for idx, conn := range c.connections {
		wg.Add(1)
		go func(i int, tc *TransferConnection) {
			defer wg.Done()
			if err := receiveFileCall(i, tc, writer, c.callbacks); err != nil {
				errCh <- err
			}
		}(idx, conn)
	}

	writeErr := writer.Start()
	if writeErr != nil {
		monitorStop()
		_ = c.control.WriteBool(false)
		_ = c.control.WriteUTF(writeErr.Error())
		_ = c.control.Flush()
		if c.callbacks.WriteFileError != nil {
			c.callbacks.WriteFileError(writeErr.Error())
		}
		return true
	}

	wg.Wait()
	select {
	case <-errCh:
		monitorStop()
		_ = c.control.WriteBool(true)
		_ = c.control.Flush()
		if c.callbacks.Incomplete != nil {
			c.callbacks.Incomplete()
		}
		return false
	default:
	}

	monitorStop()
	_ = c.control.WriteBool(true)
	_ = c.control.Flush()
	ok, err := c.control.ReadBool()
	if err != nil {
		return false
	}
	if ok {
		totalDownload := int64(0)
		for _, conn := range c.connections {
			totalDownload += conn.ResetTotalTraffic().DownloadTraffic
		}
		if c.callbacks.Complete != nil {
			c.callbacks.Complete(false, totalDownload, time.Since(start).Milliseconds())
		}
	} else {
		msg, _ := c.control.ReadUTF()
		if c.callbacks.ReadFileError != nil {
			c.callbacks.ReadFileError(msg)
		}
	}
	return true
}

func startSpeedMonitor(connections []*TransferConnection, callbacks Callbacks) func() {
	stopCh := make(chan struct{})
	doneCh := make(chan struct{})

	go func() {
		ticker := time.NewTicker(1 * time.Second)
		defer ticker.Stop()
		defer close(doneCh)
		for {
			select {
			case <-ticker.C:
				if callbacks.SpeedInfo != nil {
					info := make([]TrafficInfo, 0, len(connections))
					for _, conn := range connections {
						info = append(info, conn.ResetCurrentTraffic())
					}
					callbacks.SpeedInfo(info)
				}
			case <-stopCh:
				return
			}
		}
	}()

	return func() {
		close(stopCh)
		<-doneCh
	}
}

func deleteLocalFile(path string) bool {
	info, err := os.Stat(path)
	if err != nil {
		return false
	}
	if info.IsDir() {
		entries, err := os.ReadDir(path)
		if err != nil {
			return false
		}
		for _, entry := range entries {
			if !deleteLocalFile(filepath.Join(path, entry.Name())) {
				return false
			}
		}
		return os.Remove(path) == nil
	}
	return os.Remove(path) == nil
}

func listRoots() []string {
	if runtime.GOOS != "windows" {
		return []string{"/"}
	}
	roots := make([]string, 0, 26)
	for c := 'A'; c <= 'Z'; c++ {
		root := fmt.Sprintf("%c:\\", c)
		if _, err := os.Stat(root); err == nil {
			roots = append(roots, root)
		}
	}
	return roots
}

func writeRemoteFile(ch *proto.Channel, file RemoteFile) error {
	if err := ch.WriteUTF(file.Name); err != nil {
		return err
	}
	if err := ch.WriteUTF(file.Path); err != nil {
		return err
	}
	if err := ch.WriteInt64(file.LastModified); err != nil {
		return err
	}
	if err := ch.WriteInt64(file.Size); err != nil {
		return err
	}
	if err := ch.WriteBool(file.IsDir); err != nil {
		return err
	}
	return nil
}

func availableMemoryMB() uint64 {
	var stats runtime.MemStats
	runtime.ReadMemStats(&stats)
	if stats.Sys == 0 {
		return 0
	}
	return stats.Sys / (1024 * 1024)
}

func NormalizeHomeDir(dir string) string {
	if dir == "" {
		return "/"
	}
	if strings.TrimSpace(dir) == "" {
		return "/"
	}
	return dir
}
