package hfx

import (
    "path/filepath"
    "regexp"
    "strings"
)

const (
    FileSystemUnix    = 0
    FileSystemWindows = 1
)

type Directory struct {
    Path       string
    FileSystem int
}

func NewDirectory(path string, fs int) Directory {
    d := Directory{FileSystem: fs}
    d.Path = d.normalizePath(path)
    return d
}

func (d Directory) normalizePath(path string) string {
    if path == "" || path == "/" {
        return "/"
    }
    sep := "/"
    if d.FileSystem == FileSystemWindows {
        sep = "\\"
        path = strings.ReplaceAll(path, "/", sep)
    }
    if d.FileSystem == FileSystemWindows {
        if len(path) == 2 && path[1] == ':' {
            path += sep
        }
    }
    if path != sep && !strings.HasSuffix(path, sep) {
        path += sep
    }
    return path
}

func (d Directory) Parent() *Directory {
    if d.FileSystem == FileSystemUnix {
        if d.Path == "/" {
            return nil
        }
        trimmed := strings.TrimSuffix(d.Path, "/")
        idx := strings.LastIndex(trimmed, "/")
        parent := "/"
        if idx > 0 {
            parent = trimmed[:idx+1]
        }
        p := NewDirectory(parent, d.FileSystem)
        return &p
    }
    if d.Path == "/" {
        return nil
    }
    trimmed := strings.TrimSuffix(d.Path, "\\")
    idx := strings.LastIndex(trimmed, "\\")
    if idx <= 2 {
        p := NewDirectory("/", d.FileSystem)
        return &p
    }
    p := NewDirectory(trimmed[:idx+1], d.FileSystem)
    return &p
}

func (d Directory) Append(child string) Directory {
    if child == "" {
        return d
    }
    for strings.HasPrefix(child, "/") || strings.HasPrefix(child, "\\") {
        child = child[1:]
    }
    return NewDirectory(d.Path+child, d.FileSystem)
}

var illegalChars = regexp.MustCompile(`[\\:*?"<>|]`)

func (d Directory) GenerateTransferPath(file string, remote Directory) string {
    localSep := "/"
    remoteSep := "/"
    if d.FileSystem == FileSystemWindows {
        localSep = "\\"
    }
    if remote.FileSystem == FileSystemWindows {
        remoteSep = "\\"
    }

    normalizedFile := file
    if d.FileSystem == FileSystemWindows {
        normalizedFile = strings.ReplaceAll(file, "/", localSep)
    }

    localFolder := d.Path
    relative := ""
    if strings.HasPrefix(normalizedFile, localFolder) {
        relative = normalizedFile[len(localFolder):]
    } else {
        if strings.HasPrefix(normalizedFile, localSep) {
            relative = normalizedFile[1:]
        } else {
            relative = normalizedFile
        }
    }

    segments := strings.Split(relative, localSep)
    sanitizedSegments := make([]string, 0, len(segments))
    for _, seg := range segments {
        if seg == "" {
            continue
        }
        sanitized := illegalChars.ReplaceAllString(seg, "_")
        sanitizedSegments = append(sanitizedSegments, sanitized)
    }
    sanitizedRelative := strings.Join(sanitizedSegments, remoteSep)
    if sanitizedRelative == "" {
        return remote.Path
    }
    return remote.Path + sanitizedRelative
}

func CurrentFileSystem() int {
    if filepath.Separator == '\\' {
        return FileSystemWindows
    }
    return FileSystemUnix
}
