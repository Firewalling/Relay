//go:build linux

package supervisor

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestStateStoreRejectsSymlinkedPathsWithoutTouchingRelayState(t *testing.T) {
	root := t.TempDir()
	relayState := filepath.Join(root, "target", ".relay")
	if err := os.MkdirAll(relayState, 0o700); err != nil {
		t.Fatalf("create Relay state target: %v", err)
	}
	sentinel := filepath.Join(relayState, "sentinel")
	if err := os.WriteFile(sentinel, []byte("unchanged\n"), 0o600); err != nil {
		t.Fatalf("write sentinel: %v", err)
	}
	linkedDirectory := filepath.Join(root, "go-runs")
	if err := os.Symlink(relayState, linkedDirectory); err != nil {
		t.Fatalf("create directory symlink: %v", err)
	}

	path := filepath.Join(linkedDirectory, "state.json")
	err := NewStateStore().Save(path, fixedPersistedResult())
	if err == nil || !strings.Contains(err.Error(), "symbolic links") {
		t.Fatalf("Save error = %v, want symlink rejection", err)
	}
	if _, err := os.Stat(filepath.Join(relayState, "state.json")); !os.IsNotExist(err) {
		t.Fatalf("Go state appeared inside .relay: %v", err)
	}
	contents, err := os.ReadFile(sentinel)
	if err != nil || string(contents) != "unchanged\n" {
		t.Fatalf("Relay sentinel changed: %q, %v", contents, err)
	}

	ordinary := filepath.Join(root, "ordinary.json")
	if err := os.WriteFile(ordinary, []byte("ordinary\n"), 0o600); err != nil {
		t.Fatalf("write ordinary target: %v", err)
	}
	linkedFile := filepath.Join(root, "linked-state.json")
	if err := os.Symlink(ordinary, linkedFile); err != nil {
		t.Fatalf("create file symlink: %v", err)
	}
	if err := NewStateStore().Save(linkedFile, fixedPersistedResult()); err == nil || !strings.Contains(err.Error(), "symbolic link") {
		t.Fatalf("Save file-symlink error = %v", err)
	}
	if _, err := NewStateStore().Load(linkedFile); err == nil || !strings.Contains(err.Error(), "symbolic link") {
		t.Fatalf("Load file-symlink error = %v", err)
	}
}

func TestStateStoreUsesValidatedCanonicalPath(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	target := filepath.Join(outside, "nested")
	if err := os.Mkdir(target, 0o700); err != nil {
		t.Fatalf("create symlink target: %v", err)
	}
	link := filepath.Join(root, "link")
	if err := os.Symlink(target, link); err != nil {
		t.Fatalf("create symlink: %v", err)
	}
	uncleanPath := link + string(os.PathSeparator) + ".." + string(os.PathSeparator) + "state.json"
	if err := NewStateStore().Save(uncleanPath, fixedPersistedResult()); err != nil {
		t.Fatalf("Save canonical path: %v", err)
	}
	if _, err := os.Stat(filepath.Join(root, "state.json")); err != nil {
		t.Fatalf("canonical state missing: %v", err)
	}
	if _, err := os.Stat(filepath.Join(outside, "state.json")); !os.IsNotExist(err) {
		t.Fatalf("unclean symlink traversal was used: %v", err)
	}
}

func TestOpenStateDirectoryRejectsReplacementAfterValidation(t *testing.T) {
	root := t.TempDir()
	directory := filepath.Join(root, "runs")
	if err := os.Mkdir(directory, 0o700); err != nil {
		t.Fatalf("create original directory: %v", err)
	}
	info, err := os.Lstat(directory)
	if err != nil {
		t.Fatalf("stat original directory: %v", err)
	}
	if err := os.Rename(directory, filepath.Join(root, "moved-runs")); err != nil {
		t.Fatalf("move original directory: %v", err)
	}
	if err := os.Mkdir(directory, 0o700); err != nil {
		t.Fatalf("create replacement directory: %v", err)
	}
	handle, err := openStateDirectory(directory, info)
	if handle != nil {
		_ = handle.Close()
	}
	if err == nil || !strings.Contains(err.Error(), "changed during validation") {
		t.Fatalf("openStateDirectory error = %v, want identity-change rejection", err)
	}
}

func TestOpenStateFileDoesNotFollowReplacementSymlink(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "state.json")
	if err := os.WriteFile(path, []byte("{}"), 0o600); err != nil {
		t.Fatalf("write original state: %v", err)
	}
	_, directory, directoryInfo, err := validateStatePath(path, false)
	if err != nil {
		t.Fatalf("validateStatePath: %v", err)
	}
	if err := os.Remove(path); err != nil {
		t.Fatalf("remove original state: %v", err)
	}
	if err := os.Symlink(filepath.Join(root, "outside.json"), path); err != nil {
		t.Fatalf("create replacement symlink: %v", err)
	}
	handle, err := openStateDirectory(directory, directoryInfo)
	if err != nil {
		t.Fatalf("openStateDirectory: %v", err)
	}
	defer handle.Close()
	file, err := openStateFile(handle, filepath.Base(path))
	if file != nil {
		_ = file.Close()
	}
	if err == nil {
		t.Fatal("openStateFile followed a replacement symlink")
	}
}
