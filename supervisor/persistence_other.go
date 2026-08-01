//go:build !linux

package supervisor

import (
	"errors"
	"os"
	"path/filepath"
)

func openStateDirectory(path string, expected os.FileInfo) (*os.File, error) {
	directory, err := os.Open(path)
	if err != nil {
		return nil, err
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
	temporary, err := os.CreateTemp(directory.Name(), ".relay-run-*.tmp")
	if err != nil {
		return nil, "", err
	}
	return temporary, filepath.Base(temporary.Name()), nil
}

func openStateFile(directory *os.File, name string) (*os.File, error) {
	return os.Open(filepath.Join(directory.Name(), name))
}

func renameStateFile(directory *os.File, oldName, newName string) error {
	return os.Rename(filepath.Join(directory.Name(), oldName), filepath.Join(directory.Name(), newName))
}

func removeStateFile(directory *os.File, name string) error {
	return os.Remove(filepath.Join(directory.Name(), name))
}
