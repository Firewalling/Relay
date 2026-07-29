package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"relay/protocol"
)

type fakeExecutor struct {
	result  protocol.ExecutionResult
	err     error
	request protocol.ExecutionRequest
}

func (executor *fakeExecutor) Execute(_ context.Context, request protocol.ExecutionRequest) (protocol.ExecutionResult, error) {
	executor.request = request
	return executor.result, executor.err
}

func TestRunReturnsOneStructuredSuccessDocument(t *testing.T) {
	exitCode := 0
	executor := &fakeExecutor{result: protocol.ExecutionResult{
		RunID: "cli-success", ProtocolVersion: 1, SupervisorVersion: protocol.SupervisorVersion,
		Program: "/bin/example", Arguments: []string{},
		Environment: protocol.EnvironmentMetadata{InheritedAllowlist: []string{}, Effective: []protocol.EnvironmentEntryMetadata{}},
		AcceptedAt:  "2026-01-02T03:04:05Z", StartedAt: "2026-01-02T03:04:05Z", CompletedAt: "2026-01-02T03:04:05Z",
		Lifecycle: protocol.LifecycleCompleted, TerminationReason: protocol.TerminationExitZero, ExitCode: &exitCode,
		Cancellation: protocol.CancellationDetails{Source: "none"}, LifecycleHistory: []protocol.LifecycleEvent{},
		OutputObservations: protocol.ObservationLog{Entries: []protocol.OutputObservation{}},
	}}
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), strings.NewReader(validCLIRequest()), &stdout, &stderr, executor)
	if code != 0 || stderr.Len() != 0 {
		t.Fatalf("run code=%d stderr=%q", code, stderr.String())
	}
	if bytes.Count(stdout.Bytes(), []byte("\n")) != 1 || !json.Valid(bytes.TrimSpace(stdout.Bytes())) {
		t.Fatalf("stdout is not one JSON document: %q", stdout.String())
	}
	var envelope protocol.SuccessEnvelope[protocol.ExecutionResult]
	if err := json.Unmarshal(stdout.Bytes(), &envelope); err != nil {
		t.Fatalf("decode success envelope: %v", err)
	}
	if !envelope.OK || envelope.Command != protocol.SupervisorCommand || envelope.Data.RunID != "cli-success" {
		t.Fatalf("success envelope = %#v", envelope)
	}
	if executor.request.Arguments[0] != "literal;not-a-shell" {
		t.Fatalf("request argument changed: %#v", executor.request.Arguments)
	}
}

func TestRunReturnsStructuredProtocolErrorsOnStderr(t *testing.T) {
	tests := []struct {
		name     string
		input    string
		wantCode string
	}{
		{name: "malformed", input: `{"protocol_version":`, wantCode: "protocol_failure"},
		{name: "unsupported", input: strings.Replace(validCLIRequest(), `"protocol_version":1`, `"protocol_version":2`, 1), wantCode: "unsupported_protocol_version"},
		{name: "unknown", input: strings.Replace(validCLIRequest(), `"run_id":"cli-run",`, `"run_id":"cli-run","extra":true,`, 1), wantCode: "protocol_failure"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			code := run(context.Background(), strings.NewReader(test.input), &stdout, &stderr, &fakeExecutor{})
			if code == 0 || stdout.Len() != 0 {
				t.Fatalf("run code=%d stdout=%q", code, stdout.String())
			}
			if bytes.Count(stderr.Bytes(), []byte("\n")) != 1 {
				t.Fatalf("stderr is not one document: %q", stderr.String())
			}
			var envelope protocol.ErrorEnvelope
			if err := json.Unmarshal(stderr.Bytes(), &envelope); err != nil {
				t.Fatalf("decode error envelope: %v", err)
			}
			if envelope.OK || envelope.Command != protocol.SupervisorCommand || envelope.Error.Code != test.wantCode {
				t.Fatalf("error envelope = %#v", envelope)
			}
		})
	}
}

func TestRunReturnsStructuredExecutorFailure(t *testing.T) {
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), strings.NewReader(validCLIRequest()), &stdout, &stderr, &fakeExecutor{err: errors.New("persistence unavailable")})
	if code == 0 || stdout.Len() != 0 || !strings.Contains(stderr.String(), `"code":"protocol_failure"`) {
		t.Fatalf("run code=%d stdout=%q stderr=%q", code, stdout.String(), stderr.String())
	}
}

func TestRunRejectsOversizedRequestsBeforeDecode(t *testing.T) {
	var stdout, stderr bytes.Buffer
	input := strings.Repeat(" ", int(maximumRequestBytes)+1)
	code := run(context.Background(), strings.NewReader(input), &stdout, &stderr, &fakeExecutor{})
	if code == 0 || stdout.Len() != 0 || !strings.Contains(stderr.String(), "exceeds") {
		t.Fatalf("run code=%d stdout=%q stderr=%q", code, stdout.String(), stderr.String())
	}
}

func validCLIRequest() string {
	return `{"protocol_version":1,"run_id":"cli-run","program":"/bin/example","arguments":["literal;not-a-shell"],"redact_argument_indexes":[],"working_directory":"/tmp","environment":[],"timeout_ms":100,"termination_grace_ms":100,"stdout_limit_bytes":1024,"stderr_limit_bytes":1024,"log_limit_bytes":1024,"state_path":"/tmp/relay-runs/cli-run.json"}`
}
