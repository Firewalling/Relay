//go:build linux

package supervisor

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"syscall"
)

func openStateDirectory(path string, expected os.FileInfo) (*os.File, error) {
	fd, err := syscall.Open(path, syscall.O_RDONLY|syscall.O_DIRECTORY|syscall.O_NOFOLLOW|syscall.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	directory := os.NewFile(uintptr(fd), path)
	if directory == nil {
		_ = syscall.Close(fd)
		return nil, errors.New("could not create directory handle")
	}
	actual, err := directory.Stat()
	if err != nil {
		_ = directory.Close()
		return nil, err
	}
	if !os.SameFile(expected, actual) {
		_ = directory.Close()
		return nil, errors.New("run-state directory changed during validation")
	}
	return directory, nil
}

func createStateTemp(directory *os.File) (*os.File, string, error) {
	procPath := fmt.Sprintf("/proc/self/fd/%d", directory.Fd())
	temporary, err := os.CreateTemp(procPath, ".relay-run-*.tmp")
	if err != nil {
		return nil, "", err
	}
	return temporary, filepath.Base(temporary.Name()), nil
}

func openStateFile(directory *os.File, name string) (*os.File, error) {
	fd, err := syscall.Openat(
		int(directory.Fd()),
		name,
		syscall.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK|syscall.O_CLOEXEC,
		0,
	)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), name)
	if file == nil {
		_ = syscall.Close(fd)
		return nil, errors.New("could not create run-state file handle")
	}
	return file, nil
}

func renameStateFile(directory *os.File, oldName, newName string) error {
	return syscall.Renameat(int(directory.Fd()), oldName, int(directory.Fd()), newName)
}

func removeStateFile(directory *os.File, name string) error {
	return syscall.Unlinkat(int(directory.Fd()), name)
}
