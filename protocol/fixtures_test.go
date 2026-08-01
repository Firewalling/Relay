package protocol

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestSharedSupervisorRequestFixtures(t *testing.T) {
	directory := filepath.Join("fixtures", "supervisor-v1")
	success, err := os.Open(filepath.Join(directory, "request-success.json"))
	if err != nil {
		t.Fatalf("open success request: %v", err)
	}
	request, err := DecodeExecutionRequest(success)
	_ = success.Close()
	if err != nil {
		t.Fatalf("decode success request: %v", err)
	}
	if request.RunID != "golden-success" || request.Arguments[1] != "literal;$(not-a-shell)" {
		t.Fatalf("success request = %#v", request)
	}

	malformed, err := os.Open(filepath.Join(directory, "request-malformed.json"))
	if err != nil {
		t.Fatalf("open malformed request: %v", err)
	}
	_, malformedErr := DecodeExecutionRequest(malformed)
	_ = malformed.Close()
	if malformedErr == nil {
		t.Fatal("malformed request fixture decoded successfully")
	}

	unsupported, err := os.Open(filepath.Join(directory, "request-unsupported-version.json"))
	if err != nil {
		t.Fatalf("open unsupported request: %v", err)
	}
	_, unsupportedErr := DecodeExecutionRequest(unsupported)
	_ = unsupported.Close()
	var versionError UnsupportedProtocolVersionError
	if !errors.As(unsupportedErr, &versionError) || versionError.Version != 2 {
		t.Fatalf("unsupported fixture error = %v", unsupportedErr)
	}
}

func TestSharedSupervisorResponseFixturesAreStrictAndDeterministic(t *testing.T) {
	tests := []struct {
		file      string
		lifecycle Lifecycle
		reason    TerminationReason
		check     func(*testing.T, ExecutionResult)
	}{
		{file: "response-success.json", lifecycle: LifecycleCompleted, reason: TerminationExitZero, check: func(t *testing.T, result ExecutionResult) {
			if len(result.Arguments) != 2 || result.Arguments[1] != "literal;$(not-a-shell)" {
				t.Fatalf("literal argv missing: %#v", result.Arguments)
			}
		}},
		{file: "response-timeout.json", lifecycle: LifecycleTimedOut, reason: TerminationTimeoutTerminated, check: func(t *testing.T, result ExecutionResult) {
			if !result.Timeout.Triggered || !result.Termination.GracefulAttempted {
				t.Fatalf("timeout details = %#v / %#v", result.Timeout, result.Termination)
			}
		}},
		{file: "response-cancellation.json", lifecycle: LifecycleCancelled, reason: TerminationCancellationTerminated, check: func(t *testing.T, result ExecutionResult) {
			if !result.Cancellation.Requested || result.Cancellation.Source != "context" {
				t.Fatalf("cancellation details = %#v", result.Cancellation)
			}
		}},
		{file: "response-truncated-output.json", lifecycle: LifecycleCompleted, reason: TerminationExitZero, check: func(t *testing.T, result ExecutionResult) {
			if !result.Stdout.Truncated || !result.Stderr.Truncated || len(result.OutputObservations.Entries) != 3 {
				t.Fatalf("truncation details = stdout %#v stderr %#v log %#v", result.Stdout, result.Stderr, result.OutputObservations)
			}
		}},
		{file: "response-start-failure.json", lifecycle: LifecycleFailedToStart, reason: TerminationStartError, check: func(t *testing.T, result ExecutionResult) {
			if result.ExitCode != nil || result.Failure == nil || result.Failure.Code != "executable_not_found" {
				t.Fatalf("start failure details = exit %v failure %#v", result.ExitCode, result.Failure)
			}
		}},
	}

	for _, test := range tests {
		t.Run(test.file, func(t *testing.T) {
			path := filepath.Join("fixtures", "supervisor-v1", test.file)
			fixture, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("read fixture: %v", err)
			}
			decoder := json.NewDecoder(bytes.NewReader(fixture))
			decoder.DisallowUnknownFields()
			var envelope SuccessEnvelope[ExecutionResult]
			if err := decoder.Decode(&envelope); err != nil {
				t.Fatalf("decode fixture: %v", err)
			}
			var extra any
			if err := decoder.Decode(&extra); !errors.Is(err, io.EOF) {
				t.Fatalf("fixture contains trailing data: %v", err)
			}
			if envelope.OutputSchemaVersion != OutputSchemaVersion || !envelope.OK || envelope.Command != SupervisorCommand {
				t.Fatalf("envelope = %#v", envelope)
			}
			if envelope.Data.ProtocolVersion != ProtocolVersion || envelope.Data.SupervisorVersion != SupervisorVersion {
				t.Fatalf("data versions = %d/%q", envelope.Data.ProtocolVersion, envelope.Data.SupervisorVersion)
			}
			if envelope.Data.Lifecycle != test.lifecycle || envelope.Data.TerminationReason != test.reason {
				t.Fatalf("outcome = %s/%s, want %s/%s", envelope.Data.Lifecycle, envelope.Data.TerminationReason, test.lifecycle, test.reason)
			}
			if len(envelope.Data.LifecycleHistory) < 2 || envelope.Data.LifecycleHistory[0].Lifecycle != LifecycleAccepted || envelope.Data.LifecycleHistory[len(envelope.Data.LifecycleHistory)-1].Lifecycle != test.lifecycle {
				t.Fatalf("lifecycle history = %#v", envelope.Data.LifecycleHistory)
			}
			test.check(t, envelope.Data)
			encoded, err := json.Marshal(envelope)
			if err != nil {
				t.Fatalf("marshal fixture: %v", err)
			}
			encoded = append(encoded, '\n')
			if !reflect.DeepEqual(encoded, fixture) {
				t.Fatalf("fixture is not canonical deterministic serialization:\n got %s\nwant %s", encoded, fixture)
			}
			if strings.Contains(string(fixture), "secret-value") {
				t.Fatal("fixture contains a secret value")
			}
		})
	}
}

func TestSharedSupervisorErrorFixturesReuseRelayEnvelope(t *testing.T) {
	tests := []struct {
		file string
		code string
	}{
		{file: "error-malformed-request.json", code: "protocol_failure"},
		{file: "error-unsupported-version.json", code: "unsupported_protocol_version"},
	}
	for _, test := range tests {
		t.Run(test.file, func(t *testing.T) {
			path := filepath.Join("fixtures", "supervisor-v1", test.file)
			fixture, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("read fixture: %v", err)
			}
			decoder := json.NewDecoder(bytes.NewReader(fixture))
			decoder.DisallowUnknownFields()
			var envelope ErrorEnvelope
			if err := decoder.Decode(&envelope); err != nil {
				t.Fatalf("decode fixture: %v", err)
			}
			if envelope.OutputSchemaVersion != OutputSchemaVersion || envelope.OK || envelope.Command != SupervisorCommand || envelope.Error.Code != test.code {
				t.Fatalf("error envelope = %#v", envelope)
			}
			encoded, err := json.Marshal(envelope)
			if err != nil {
				t.Fatalf("marshal fixture: %v", err)
			}
			encoded = append(encoded, '\n')
			if !bytes.Equal(encoded, fixture) {
				t.Fatalf("fixture is not deterministic:\n got %s\nwant %s", encoded, fixture)
			}
		})
	}
}

func TestLifecycleVocabularyIncludesEverySupervisorState(t *testing.T) {
	got := []Lifecycle{
		LifecycleAccepted,
		LifecycleStarting,
		LifecycleRunning,
		LifecycleCompleted,
		LifecycleFailedToStart,
		LifecycleTimedOut,
		LifecycleCancelled,
		LifecycleKilled,
		LifecycleProtocolFailure,
	}
	want := []Lifecycle{"accepted", "starting", "running", "completed", "failed_to_start", "timed_out", "cancelled", "killed", "protocol_failure"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("lifecycle vocabulary = %v, want %v", got, want)
	}
}
