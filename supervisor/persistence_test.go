package supervisor

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"relay/protocol"
)

func TestStateStoreRoundTripsDeterministicInspectableRecord(t *testing.T) {
	path := filepath.Join(t.TempDir(), "runs", "fixed.json")
	store := NewStateStore()
	result := fixedPersistedResult()
	if err := store.Save(path, result); err != nil {
		t.Fatalf("Save: %v", err)
	}
	first, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read first state: %v", err)
	}
	if !bytes.HasSuffix(first, []byte("\n")) {
		t.Fatal("persisted state is not newline terminated")
	}
	if bytes.Contains(first, []byte("secret-value")) {
		t.Fatalf("persisted state leaked a secret: %s", first)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat state: %v", err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("state mode = %o, want 600", info.Mode().Perm())
	}

	record, err := store.Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if record.RunStateVersion != protocol.RunStateVersion || record.SupervisorVersion != protocol.SupervisorVersion {
		t.Fatalf("record versions = %#v", record)
	}
	if !reflect.DeepEqual(record.Execution, result) {
		t.Fatalf("reloaded execution differs:\n got %#v\nwant %#v", record.Execution, result)
	}

	if err := store.Save(path, result); err != nil {
		t.Fatalf("Save again: %v", err)
	}
	second, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read second state: %v", err)
	}
	if !bytes.Equal(first, second) {
		t.Fatalf("fixed state serialization changed:\n%s\n%s", first, second)
	}
}

func TestStateStoreRenameFailurePreservesPreviousRecordAtomically(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "state.json")
	store := NewStateStore()
	original := fixedPersistedResult()
	if err := store.Save(path, original); err != nil {
		t.Fatalf("initial Save: %v", err)
	}
	before, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read initial state: %v", err)
	}

	failedStore := NewStateStore()
	failedStore.rename = func(string, string) error { return errors.New("injected rename failure") }
	replacement := original
	replacement.Lifecycle = protocol.LifecycleKilled
	replacement.TerminationReason = protocol.TerminationTimeoutKilled
	if err := failedStore.Save(path, replacement); err == nil || !strings.Contains(err.Error(), "injected rename failure") {
		t.Fatalf("Save error = %v, want injected rename failure", err)
	}
	after, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read preserved state: %v", err)
	}
	if !bytes.Equal(before, after) {
		t.Fatalf("failed replacement changed committed bytes:\nbefore %s\nafter %s", before, after)
	}
	entries, err := os.ReadDir(directory)
	if err != nil {
		t.Fatalf("read state directory: %v", err)
	}
	if len(entries) != 1 || entries[0].Name() != "state.json" {
		t.Fatalf("temporary state was not removed: %#v", entries)
	}
}

func TestStateStoreRejectsMalformedUnsupportedAndTrailingRecords(t *testing.T) {
	directory := t.TempDir()
	tests := []struct {
		name    string
		payload string
		want    string
	}{
		{name: "malformed", payload: `{`, want: "decode run state"},
		{name: "unsupported", payload: `{"run_state_version":9,"supervisor_version":"0.1.0","execution":{"protocol_version":1,"supervisor_version":"0.1.0"}}`, want: "unsupported run-state version 9"},
		{name: "unknown", payload: `{"run_state_version":1,"supervisor_version":"0.1.0","execution":{"protocol_version":1,"supervisor_version":"0.1.0"},"unknown":true}`, want: "unknown field"},
		{name: "trailing", payload: `{"run_state_version":1,"supervisor_version":"0.1.0","execution":{"protocol_version":1,"supervisor_version":"0.1.0"}} {}`, want: "exactly one JSON document"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(directory, test.name+".json")
			if err := os.WriteFile(path, []byte(test.payload), 0o600); err != nil {
				t.Fatalf("write fixture: %v", err)
			}
			_, err := NewStateStore().Load(path)
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("Load error = %v, want substring %q", err, test.want)
			}
		})
	}
}

func TestStateStoreRejectsEncodedAndLoadedRecordsOverItsLimit(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "state.json")
	store := NewStateStore()
	store.maximumBytes = 64
	if err := store.Save(path, fixedPersistedResult()); err == nil || !strings.Contains(err.Error(), "encoded run state exceeds 64 bytes") {
		t.Fatalf("Save error = %v, want encoded-size rejection", err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("oversized state was persisted: %v", err)
	}
	if err := os.WriteFile(path, bytes.Repeat([]byte{'x'}, 65), 0o600); err != nil {
		t.Fatalf("write oversized record: %v", err)
	}
	if _, err := store.Load(path); err == nil || !strings.Contains(err.Error(), "run state exceeds 64 bytes") {
		t.Fatalf("Load error = %v, want encoded-size rejection", err)
	}
}

func fixedPersistedResult() protocol.ExecutionResult {
	exitCode := 0
	first, last := uint64(4), uint64(4)
	return protocol.ExecutionResult{
		RunID: "fixed-run", ProtocolVersion: protocol.ProtocolVersion, SupervisorVersion: protocol.SupervisorVersion,
		Program: "/bin/example", Arguments: []string{"--credential", "<redacted>"},
		Environment: protocol.EnvironmentMetadata{
			InheritedAllowlist:    []string{"LANG", "PATH"},
			Effective:             []protocol.EnvironmentEntryMetadata{{Name: "LANG", Source: "inherited"}, {Name: "RELAY_RUN_ID", Source: "required"}},
			BlockedInheritedCount: 3,
		},
		AcceptedAt: "2026-01-02T03:04:05Z", StartedAt: "2026-01-02T03:04:06Z", CompletedAt: "2026-01-02T03:04:07Z", DurationMilliseconds: 1000,
		Lifecycle: protocol.LifecycleCompleted, TerminationReason: protocol.TerminationExitZero, ExitCode: &exitCode,
		Timeout: protocol.TimeoutDetails{ConfiguredMilliseconds: 5000}, Cancellation: protocol.CancellationDetails{Source: "none"}, Termination: protocol.TerminationDetails{GraceMilliseconds: 100},
		Stdout: protocol.StreamResult{Text: "ok\n", BytesObserved: 3, BytesRetained: 3, FirstSequence: &first, LastSequence: &last},
		Stderr: protocol.StreamResult{Text: ""},
		LifecycleHistory: []protocol.LifecycleEvent{
			{Sequence: 1, Lifecycle: protocol.LifecycleAccepted, Timestamp: "2026-01-02T03:04:05Z"},
			{Sequence: 2, Lifecycle: protocol.LifecycleStarting, Timestamp: "2026-01-02T03:04:06Z"},
			{Sequence: 3, Lifecycle: protocol.LifecycleRunning, Timestamp: "2026-01-02T03:04:06Z"},
			{Sequence: 5, Lifecycle: protocol.LifecycleCompleted, Timestamp: "2026-01-02T03:04:07Z"},
		},
		OutputObservations: protocol.ObservationLog{LimitBytes: 1024, BytesUsed: 64, Entries: []protocol.OutputObservation{{Sequence: 4, Stream: "stdout", Bytes: 3}}},
	}
}
