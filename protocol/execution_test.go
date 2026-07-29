package protocol

import (
	"encoding/json"
	"errors"
	"reflect"
	"strings"
	"testing"
)

const validRequestJSON = `{
  "protocol_version": 1,
  "run_id": "run-123",
  "program": "/usr/bin/example",
  "arguments": ["literal;$(not-a-shell)", "secret-value", "plain"],
  "redact_argument_indexes": [1],
  "working_directory": "/tmp",
  "environment": [{"name":"GREETING","value":"hello"}],
  "timeout_ms": 1000,
  "termination_grace_ms": 100,
  "stdout_limit_bytes": 1024,
  "stderr_limit_bytes": 2048,
  "log_limit_bytes": 4096,
  "state_path": "/tmp/relay-runs/run-123.json"
}`

func TestDecodeExecutionRequestValidatesAndRedactsMetadata(t *testing.T) {
	request, err := DecodeExecutionRequest(strings.NewReader(validRequestJSON))
	if err != nil {
		t.Fatalf("DecodeExecutionRequest: %v", err)
	}
	if got, want := request.Arguments[0], "literal;$(not-a-shell)"; got != want {
		t.Fatalf("literal argument = %q, want %q", got, want)
	}
	if got, want := request.SanitizedArguments(), []string{"literal;$(not-a-shell)", "<redacted>", "plain"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("sanitized arguments = %#v, want %#v", got, want)
	}
	if got := request.Arguments[1]; got != "secret-value" {
		t.Fatalf("execution argument was modified: %q", got)
	}
}

func TestDecodeExecutionRequestRejectsMalformedUnsupportedAndUnsafeInput(t *testing.T) {
	tests := []struct {
		name string
		json string
		want string
	}{
		{name: "malformed", json: `{"protocol_version":`, want: "unexpected EOF"},
		{name: "unknown field", json: strings.Replace(validRequestJSON, `"run_id": "run-123",`, `"run_id": "run-123", "unknown": true,`, 1), want: "unknown field"},
		{name: "multiple documents", json: validRequestJSON + "\n{}", want: "exactly one JSON document"},
		{name: "unsupported", json: strings.Replace(validRequestJSON, `"protocol_version": 1`, `"protocol_version": 9`, 1), want: "unsupported protocol version 9"},
		{name: "relative program", json: strings.Replace(validRequestJSON, `"program": "/usr/bin/example"`, `"program": "example"`, 1), want: "program must be an absolute path"},
		{name: "rust state", json: strings.Replace(validRequestJSON, `/tmp/relay-runs/run-123.json`, `/tmp/.relay/run-123.json`, 1), want: "Rust-owned .relay"},
		{name: "bad redaction index", json: strings.Replace(validRequestJSON, `"redact_argument_indexes": [1]`, `"redact_argument_indexes": [8]`, 1), want: "out-of-range index 8"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			_, err := DecodeExecutionRequest(strings.NewReader(test.json))
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("DecodeExecutionRequest error = %v, want substring %q", err, test.want)
			}
			if test.name == "unsupported" {
				var unsupported UnsupportedProtocolVersionError
				if !errors.As(err, &unsupported) {
					t.Fatalf("error %T does not preserve UnsupportedProtocolVersionError", err)
				}
			}
		})
	}
}

func TestExecutionRequestBoundsRedactionWork(t *testing.T) {
	request, err := DecodeExecutionRequest(strings.NewReader(validRequestJSON))
	if err != nil {
		t.Fatalf("DecodeExecutionRequest: %v", err)
	}
	request.RedactArgumentIndexes = []int{1, 0, 1}
	if err := request.Validate(); err == nil || !strings.Contains(err.Error(), "duplicate index 1") {
		t.Fatalf("duplicate Validate error = %v", err)
	}

	request.Arguments = make([]string, MaximumRedactedArguments+1)
	request.RedactArgumentIndexes = make([]int, MaximumRedactedArguments+1)
	for index := range request.RedactArgumentIndexes {
		request.RedactArgumentIndexes[index] = index
	}
	if err := request.Validate(); err == nil || !strings.Contains(err.Error(), "at most 32 entries") {
		t.Fatalf("count Validate error = %v", err)
	}

	request.Arguments = []string{strings.Repeat("s", MaximumRedactedArgumentBytes+1)}
	request.RedactArgumentIndexes = []int{0}
	if err := request.Validate(); err == nil || !strings.Contains(err.Error(), "total at most") {
		t.Fatalf("byte total Validate error = %v", err)
	}
}

func TestExecutionRequestRejectsInvalidUTF8AtDirectAPIBoundary(t *testing.T) {
	invalid := string([]byte{0xff})
	tests := []struct {
		name   string
		mutate func(*ExecutionRequest)
		want   string
	}{
		{name: "program", mutate: func(request *ExecutionRequest) { request.Program = "/" + invalid }, want: "program"},
		{name: "argument", mutate: func(request *ExecutionRequest) { request.Arguments[0] = invalid }, want: "arguments[0]"},
		{name: "working directory", mutate: func(request *ExecutionRequest) { request.WorkingDirectory = "/" + invalid }, want: "working_directory"},
		{name: "environment", mutate: func(request *ExecutionRequest) { request.Environment[0].Value = invalid }, want: "environment[0]"},
		{name: "state path", mutate: func(request *ExecutionRequest) { request.StatePath = "/" + invalid }, want: "state_path"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			request, err := DecodeExecutionRequest(strings.NewReader(validRequestJSON))
			if err != nil {
				t.Fatalf("DecodeExecutionRequest: %v", err)
			}
			test.mutate(&request)
			if err := request.Validate(); err == nil || !strings.Contains(err.Error(), test.want) || !strings.Contains(err.Error(), "valid UTF-8") {
				t.Fatalf("Validate error = %v, want UTF-8 rejection for %s", err, test.want)
			}
		})
	}
}

func TestExecutionRequestAppliesDeterministicDefaults(t *testing.T) {
	input := `{"protocol_version":1,"run_id":"run-defaults","program":"/bin/example","arguments":[],"redact_argument_indexes":[],"working_directory":"/tmp","environment":[],"timeout_ms":0,"termination_grace_ms":0,"stdout_limit_bytes":0,"stderr_limit_bytes":0,"log_limit_bytes":0,"state_path":"/tmp/relay-runs/defaults.json"}`
	request, err := DecodeExecutionRequest(strings.NewReader(input))
	if err != nil {
		t.Fatalf("DecodeExecutionRequest: %v", err)
	}
	if request.TerminationGraceMilliseconds != DefaultGraceMilliseconds || request.StdoutLimitBytes != DefaultOutputLimitBytes || request.StderrLimitBytes != DefaultOutputLimitBytes || request.LogLimitBytes != DefaultLogLimitBytes {
		t.Fatalf("unexpected defaults: %#v", request)
	}
}

func TestFixedExecutionEnvelopeSerializesDeterministically(t *testing.T) {
	exitCode := 0
	first, last := uint64(4), uint64(5)
	result := ExecutionResult{
		RunID: "fixed-run", ProtocolVersion: 1, SupervisorVersion: SupervisorVersion,
		Program: "/bin/example", Arguments: []string{"literal"},
		Environment: EnvironmentMetadata{InheritedAllowlist: []string{"LANG", "PATH"}, Effective: []EnvironmentEntryMetadata{{Name: "PATH", Source: "inherited"}}, BlockedInheritedCount: 2},
		AcceptedAt:  "2026-01-02T03:04:05Z", StartedAt: "2026-01-02T03:04:06Z", CompletedAt: "2026-01-02T03:04:07Z", DurationMilliseconds: 1000,
		Lifecycle: LifecycleCompleted, TerminationReason: TerminationExitZero, ExitCode: &exitCode,
		Timeout: TimeoutDetails{ConfiguredMilliseconds: 5000}, Cancellation: CancellationDetails{Source: "none"}, Termination: TerminationDetails{GraceMilliseconds: 100},
		Stdout:             StreamResult{Text: "ok\n", BytesObserved: 3, BytesRetained: 3, FirstSequence: &first, LastSequence: &last},
		Stderr:             StreamResult{Text: "", BytesObserved: 0, BytesRetained: 0},
		LifecycleHistory:   []LifecycleEvent{{Sequence: 1, Lifecycle: LifecycleAccepted, Timestamp: "2026-01-02T03:04:05Z"}, {Sequence: 2, Lifecycle: LifecycleStarting, Timestamp: "2026-01-02T03:04:06Z"}, {Sequence: 3, Lifecycle: LifecycleRunning, Timestamp: "2026-01-02T03:04:06Z"}, {Sequence: 6, Lifecycle: LifecycleCompleted, Timestamp: "2026-01-02T03:04:07Z"}},
		OutputObservations: ObservationLog{LimitBytes: 1024, BytesUsed: 73, Entries: []OutputObservation{{Sequence: 4, Stream: "stdout", Bytes: 2}, {Sequence: 5, Stream: "stdout", Bytes: 1}}},
	}
	firstJSON, err := json.Marshal(Success(result))
	if err != nil {
		t.Fatalf("marshal fixed result: %v", err)
	}
	secondJSON, err := json.Marshal(Success(result))
	if err != nil {
		t.Fatalf("marshal fixed result again: %v", err)
	}
	if !reflect.DeepEqual(firstJSON, secondJSON) {
		t.Fatalf("fixed serialization differs:\n%s\n%s", firstJSON, secondJSON)
	}
	if strings.Contains(string(firstJSON), "secret-value") {
		t.Fatal("fixed protocol output contains a secret value")
	}
}
