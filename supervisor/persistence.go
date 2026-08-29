package supervisor

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"relay/protocol"
)

const maximumPersistedRecordBytes int64 = 140 << 20

type StateStore struct {
	rename       func(string, string) error
	maximumBytes int64
}

func NewStateStore() *StateStore {
	return &StateStore{maximumBytes: maximumPersistedRecordBytes}
}

func (store *StateStore) Save(path string, result protocol.ExecutionResult) error {
	identity, err := captureProcessIdentity(os.Getpid())
	var attemptIdentity *protocol.AttemptIdentity
	if err == nil {
		attemptIdentity = &protocol.AttemptIdentity{Supervisor: identity}
	}
	return store.SaveWithIdentity(path, result, attemptIdentity)
}

func (store *StateStore) SaveWithIdentity(path string, result protocol.ExecutionResult, identity *protocol.AttemptIdentity) error {
	limit := store.recordLimit()
	cleanPath, directory, directoryInfo, err := validateStatePath(path, true)
	if err != nil {
		return fmt.Errorf("validate run-state path: %w", err)
	}
	directoryHandle, err := openStateDirectory(directory, directoryInfo)
	if err != nil {
		return fmt.Errorf("open validated run-state directory: %w", err)
	}
	defer directoryHandle.Close()
	record := protocol.RunRecord{
		RunStateVersion:   protocol.RunStateVersion,
		SupervisorVersion: protocol.SupervisorVersion,
		StatePath:         cleanPath,
		ProcessIdentity:   identity,
		Execution:         result,
	}
	payload, err := json.MarshalIndent(record, "", "  ")
	if err != nil {
		return fmt.Errorf("encode run state: %w", err)
	}
	if int64(len(payload)+1) > limit {
		return fmt.Errorf("encoded run state exceeds %d bytes", limit)
	}
	payload = append(payload, '\n')

	temporary, temporaryName, err := createStateTemp(directoryHandle)
	if err != nil {
		return fmt.Errorf("create temporary run state: %w", err)
	}
	committed := false
	defer func() {
		if !committed {
			_ = removeStateFile(directoryHandle, temporaryName)
		}
	}()

	if err := temporary.Chmod(0o600); err != nil {
		_ = temporary.Close()
		return fmt.Errorf("restrict temporary run state: %w", err)
	}
	if _, err := temporary.Write(payload); err != nil {
		_ = temporary.Close()
		return fmt.Errorf("write temporary run state: %w", err)
	}
	if err := temporary.Sync(); err != nil {
		_ = temporary.Close()
		return fmt.Errorf("sync temporary run state: %w", err)
	}
	if err := temporary.Close(); err != nil {
		return fmt.Errorf("close temporary run state: %w", err)
	}
	if store.rename != nil {
		err = store.rename(temporary.Name(), cleanPath)
	} else {
		err = renameStateFile(directoryHandle, temporaryName, filepath.Base(cleanPath))
	}
	if err != nil {
		return fmt.Errorf("atomically replace run state: %w", err)
	}
	committed = true
	if err := directoryHandle.Sync(); err != nil {
		return fmt.Errorf("sync run-state directory: %w", err)
	}
	return nil
}

func (store *StateStore) Load(path string) (protocol.RunRecord, error) {
	limit := store.recordLimit()
	cleanPath, directory, directoryInfo, err := validateStatePath(path, false)
	if err != nil {
		return protocol.RunRecord{}, fmt.Errorf("validate run-state path: %w", err)
	}
	directoryHandle, err := openStateDirectory(directory, directoryInfo)
	if err != nil {
		return protocol.RunRecord{}, fmt.Errorf("open validated run-state directory: %w", err)
	}
	defer directoryHandle.Close()
	file, err := openStateFile(directoryHandle, filepath.Base(cleanPath))
	if err != nil {
		return protocol.RunRecord{}, fmt.Errorf("open run state: %w", err)
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return protocol.RunRecord{}, fmt.Errorf("inspect run state: %w", err)
	}
	if !info.Mode().IsRegular() {
		return protocol.RunRecord{}, errors.New("run-state path must be a regular file")
	}
	if info.Size() > limit {
		return protocol.RunRecord{}, fmt.Errorf("run state exceeds %d bytes", limit)
	}

	decoder := json.NewDecoder(io.LimitReader(file, limit+1))
	decoder.DisallowUnknownFields()
	var record protocol.RunRecord
	if err := decoder.Decode(&record); err != nil {
		return protocol.RunRecord{}, fmt.Errorf("decode run state: %w", err)
	}
	var extra any
	if err := decoder.Decode(&extra); !errors.Is(err, io.EOF) {
		if err == nil {
			return protocol.RunRecord{}, errors.New("run state must contain exactly one JSON document")
		}
		return protocol.RunRecord{}, fmt.Errorf("decode trailing run state: %w", err)
	}
	if record.RunStateVersion != protocol.RunStateVersion && record.RunStateVersion != protocol.LegacyRunStateVersion {
		return protocol.RunRecord{}, fmt.Errorf("unsupported run-state version %d", record.RunStateVersion)
	}
	if record.RunStateVersion == protocol.LegacyRunStateVersion && (record.StatePath != "" || record.ProcessIdentity != nil) {
		return protocol.RunRecord{}, errors.New("legacy run state contains version-2 fields")
	}
	if record.RunStateVersion == protocol.RunStateVersion && record.StatePath != cleanPath {
		return protocol.RunRecord{}, errors.New("run state state_path does not match requested path")
	}
	if record.SupervisorVersion == "" || record.Execution.SupervisorVersion == "" || record.SupervisorVersion != record.Execution.SupervisorVersion {
		return protocol.RunRecord{}, errors.New("run state supervisor_version is inconsistent")
	}
	if record.Execution.ProtocolVersion != protocol.ProtocolVersion {
		return protocol.RunRecord{}, fmt.Errorf("unsupported execution protocol version %d", record.Execution.ProtocolVersion)
	}
	if record.ProcessIdentity != nil {
		if err := validateAttemptIdentity(record.ProcessIdentity); err != nil {
			return protocol.RunRecord{}, fmt.Errorf("invalid process identity: %w", err)
		}
	}
	return record, nil
}

func validateAttemptIdentity(identity *protocol.AttemptIdentity) error {
	if err := validateProcessIdentity(identity.Supervisor); err != nil {
		return fmt.Errorf("supervisor: %w", err)
	}
	if identity.Child != nil {
		if err := validateProcessIdentity(*identity.Child); err != nil {
			return fmt.Errorf("child: %w", err)
		}
	}
	return nil
}

func validateProcessIdentity(identity protocol.ProcessIdentity) error {
	if identity.PID <= 0 || identity.StartTimeTicks == 0 || identity.ProcessGroupID <= 0 || identity.SessionID <= 0 || identity.Executable == "" {
		return errors.New("PID, start time, process group, session, and executable are required")
	}
	return nil
}

func (store *StateStore) recordLimit() int64 {
	if store.maximumBytes > 0 {
		return store.maximumBytes
	}
	return maximumPersistedRecordBytes
}

func validateStatePath(path string, createDirectory bool) (string, string, os.FileInfo, error) {
	clean := filepath.Clean(path)
	if !filepath.IsAbs(clean) {
		return "", "", nil, errors.New("run-state path must be absolute")
	}
	if hasRelayStateComponent(clean) {
		return "", "", nil, errors.New("run-state path must not contain a .relay component")
	}
	directory := filepath.Dir(clean)
	directoryInfo, err := validateStateDirectory(directory, createDirectory)
	if err != nil {
		return "", "", nil, err
	}
	info, err := os.Lstat(clean)
	if err == nil {
		if info.Mode()&os.ModeSymlink != 0 {
			return "", "", nil, errors.New("run-state file must not be a symbolic link")
		}
		if !info.Mode().IsRegular() {
			return "", "", nil, errors.New("run-state path must be a regular file")
		}
	} else if !os.IsNotExist(err) {
		return "", "", nil, fmt.Errorf("inspect run-state file: %w", err)
	} else if !createDirectory {
		return "", "", nil, fmt.Errorf("inspect run-state file: %w", err)
	}
	return clean, directory, directoryInfo, nil
}

func validateStateDirectory(directory string, create bool) (os.FileInfo, error) {
	volume := filepath.VolumeName(directory)
	root := volume + string(os.PathSeparator)
	remainder := strings.TrimPrefix(directory, root)
	current := root
	for _, component := range strings.Split(remainder, string(os.PathSeparator)) {
		if component == "" {
			continue
		}
		current = filepath.Join(current, component)
		info, err := os.Lstat(current)
		if os.IsNotExist(err) && create {
			if err := os.Mkdir(current, 0o700); err != nil && !os.IsExist(err) {
				return nil, fmt.Errorf("create run-state directory: %w", err)
			}
			info, err = os.Lstat(current)
		}
		if err != nil {
			return nil, fmt.Errorf("inspect run-state directory: %w", err)
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return nil, errors.New("run-state directory must not contain symbolic links")
		}
		if !info.IsDir() {
			return nil, errors.New("run-state parent must be a directory")
		}
	}
	resolved, err := filepath.EvalSymlinks(directory)
	if err != nil {
		return nil, fmt.Errorf("resolve run-state directory: %w", err)
	}
	if filepath.Clean(resolved) != filepath.Clean(directory) {
		return nil, errors.New("run-state directory must not resolve through symbolic links")
	}
	if hasRelayStateComponent(resolved) {
		return nil, errors.New("resolved run-state directory must not contain a .relay component")
	}
	info, err := os.Lstat(directory)
	if err != nil {
		return nil, fmt.Errorf("inspect validated run-state directory: %w", err)
	}
	return info, nil
}

func hasRelayStateComponent(path string) bool {
	for _, component := range strings.FieldsFunc(filepath.Clean(path), func(r rune) bool {
		return r == '/' || r == '\\'
	}) {
		if component == ".relay" {
			return true
		}
	}
	return false
}
