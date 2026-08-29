package protocol

import (
	"errors"
	"fmt"
	"io"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
	"unicode/utf8"
)

const (
	DefaultOutputLimitBytes      int64 = 1 << 20
	MaximumOutputLimitBytes      int64 = 8 << 20
	DefaultLogLimitBytes         int64 = 64 << 10
	MaximumLogLimitBytes         int64 = 4 << 20
	MaximumRedactedArguments     int   = 32
	MaximumRedactedArgumentBytes int   = 64 << 10
	DefaultGraceMilliseconds     int64 = 2_000
	MaximumGraceMilliseconds     int64 = 60_000
	MaximumTimeoutMilliseconds   int64 = int64((24 * time.Hour) / time.Millisecond)
)

var runIDPattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`)

type Lifecycle string

const (
	LifecycleAccepted        Lifecycle = "accepted"
	LifecycleStarting        Lifecycle = "starting"
	LifecycleRunning         Lifecycle = "running"
	LifecycleCompleted       Lifecycle = "completed"
	LifecycleFailedToStart   Lifecycle = "failed_to_start"
	LifecycleTimedOut        Lifecycle = "timed_out"
	LifecycleCancelled       Lifecycle = "cancelled"
	LifecycleKilled          Lifecycle = "killed"
	LifecycleProtocolFailure Lifecycle = "protocol_failure"
)

type TerminationReason string

const (
	TerminationExitZero               TerminationReason = "exit_zero"
	TerminationExitNonzero            TerminationReason = "exit_nonzero"
	TerminationStartError             TerminationReason = "start_error"
	TerminationTimeoutTerminated      TerminationReason = "timeout_terminated"
	TerminationCancellationTerminated TerminationReason = "cancelled_terminated"
	TerminationTimeoutKilled          TerminationReason = "timeout_killed"
	TerminationCancellationKilled     TerminationReason = "cancelled_killed"
	TerminationDescendantsTerminated  TerminationReason = "descendants_terminated"
	TerminationDescendantsKilled      TerminationReason = "descendants_killed"
	TerminationProtocolFailure        TerminationReason = "protocol_failure"
)

type EnvironmentVariable struct {
	Name  string `json:"name"`
	Value string `json:"value"`
}

type ExecutionRequest struct {
	ProtocolVersion              uint32                `json:"protocol_version"`
	RunID                        string                `json:"run_id"`
	Program                      string                `json:"program"`
	Arguments                    []string              `json:"arguments"`
	RedactArgumentIndexes        []int                 `json:"redact_argument_indexes"`
	WorkingDirectory             string                `json:"working_directory"`
	Environment                  []EnvironmentVariable `json:"environment"`
	TimeoutMilliseconds          int64                 `json:"timeout_ms"`
	TerminationGraceMilliseconds int64                 `json:"termination_grace_ms"`
	StdoutLimitBytes             int64                 `json:"stdout_limit_bytes"`
	StderrLimitBytes             int64                 `json:"stderr_limit_bytes"`
	LogLimitBytes                int64                 `json:"log_limit_bytes"`
	StatePath                    string                `json:"state_path"`
}

type UnsupportedProtocolVersionError struct {
	Version uint32
}

func (err UnsupportedProtocolVersionError) Error() string {
	return fmt.Sprintf("unsupported protocol version %d (supported: %d)", err.Version, ProtocolVersion)
}

func DecodeExecutionRequest(r io.Reader) (ExecutionRequest, error) {
	var request ExecutionRequest
	if err := decodeSingleJSON(r, &request); err != nil {
		return ExecutionRequest{}, fmt.Errorf("decode execution request: %w", err)
	}
	request.applyDefaults()
	if err := request.Validate(); err != nil {
		return ExecutionRequest{}, err
	}
	return request, nil
}

func (request *ExecutionRequest) applyDefaults() {
	if request.Arguments == nil {
		request.Arguments = []string{}
	}
	if request.RedactArgumentIndexes == nil {
		request.RedactArgumentIndexes = []int{}
	}
	if request.Environment == nil {
		request.Environment = []EnvironmentVariable{}
	}
	if request.TerminationGraceMilliseconds == 0 {
		request.TerminationGraceMilliseconds = DefaultGraceMilliseconds
	}
	if request.StdoutLimitBytes == 0 {
		request.StdoutLimitBytes = DefaultOutputLimitBytes
	}
	if request.StderrLimitBytes == 0 {
		request.StderrLimitBytes = DefaultOutputLimitBytes
	}
	if request.LogLimitBytes == 0 {
		request.LogLimitBytes = DefaultLogLimitBytes
	}
	sort.Ints(request.RedactArgumentIndexes)
}

func (request ExecutionRequest) Validate() error {
	if request.ProtocolVersion != ProtocolVersion {
		return UnsupportedProtocolVersionError{Version: request.ProtocolVersion}
	}
	if !runIDPattern.MatchString(request.RunID) {
		return errors.New("run_id must match [A-Za-z0-9][A-Za-z0-9._-]{0,127}")
	}
	if request.Program == "" || strings.ContainsRune(request.Program, '\x00') {
		return errors.New("program must be a nonempty path without NUL bytes")
	}
	if !utf8.ValidString(request.Program) {
		return errors.New("program must be valid UTF-8")
	}
	if !filepath.IsAbs(request.Program) {
		return errors.New("program must be an absolute path")
	}
	for index, argument := range request.Arguments {
		if strings.ContainsRune(argument, '\x00') {
			return fmt.Errorf("arguments[%d] must not contain a NUL byte", index)
		}
		if !utf8.ValidString(argument) {
			return fmt.Errorf("arguments[%d] must be valid UTF-8", index)
		}
	}
	if len(request.RedactArgumentIndexes) > MaximumRedactedArguments {
		return fmt.Errorf("redact_argument_indexes must contain at most %d entries", MaximumRedactedArguments)
	}
	seenRedactions := make(map[int]struct{}, len(request.RedactArgumentIndexes))
	redactedBytes := 0
	for _, index := range request.RedactArgumentIndexes {
		if index < 0 || index >= len(request.Arguments) {
			return fmt.Errorf("redact_argument_indexes contains out-of-range index %d", index)
		}
		if _, exists := seenRedactions[index]; exists {
			return fmt.Errorf("redact_argument_indexes contains duplicate index %d", index)
		}
		seenRedactions[index] = struct{}{}
		redactedBytes += len(request.Arguments[index])
	}
	if redactedBytes > MaximumRedactedArgumentBytes {
		return fmt.Errorf("redacted argument values must total at most %d bytes", MaximumRedactedArgumentBytes)
	}
	if request.WorkingDirectory == "" || !filepath.IsAbs(request.WorkingDirectory) {
		return errors.New("working_directory must be an absolute path")
	}
	if strings.ContainsRune(request.WorkingDirectory, '\x00') {
		return errors.New("working_directory must not contain a NUL byte")
	}
	if !utf8.ValidString(request.WorkingDirectory) {
		return errors.New("working_directory must be valid UTF-8")
	}
	for index, variable := range request.Environment {
		if variable.Name == "" || strings.ContainsAny(variable.Name, "=\x00") {
			return fmt.Errorf("environment[%d].name must be nonempty and contain neither '=' nor NUL", index)
		}
		if strings.ContainsRune(variable.Value, '\x00') {
			return fmt.Errorf("environment[%d].value must not contain a NUL byte", index)
		}
		if !utf8.ValidString(variable.Name) || !utf8.ValidString(variable.Value) {
			return fmt.Errorf("environment[%d] name and value must be valid UTF-8", index)
		}
	}
	if request.TimeoutMilliseconds < 0 || request.TimeoutMilliseconds > MaximumTimeoutMilliseconds {
		return fmt.Errorf("timeout_ms must be between 0 and %d", MaximumTimeoutMilliseconds)
	}
	if request.TerminationGraceMilliseconds < 1 || request.TerminationGraceMilliseconds > MaximumGraceMilliseconds {
		return fmt.Errorf("termination_grace_ms must be between 1 and %d", MaximumGraceMilliseconds)
	}
	if err := validateLimit("stdout_limit_bytes", request.StdoutLimitBytes, MaximumOutputLimitBytes); err != nil {
		return err
	}
	if err := validateLimit("stderr_limit_bytes", request.StderrLimitBytes, MaximumOutputLimitBytes); err != nil {
		return err
	}
	if err := validateLimit("log_limit_bytes", request.LogLimitBytes, MaximumLogLimitBytes); err != nil {
		return err
	}
	if request.StatePath == "" || !filepath.IsAbs(request.StatePath) {
		return errors.New("state_path must be an absolute path")
	}
	if strings.ContainsRune(request.StatePath, '\x00') {
		return errors.New("state_path must not contain a NUL byte")
	}
	if !utf8.ValidString(request.StatePath) {
		return errors.New("state_path must be valid UTF-8")
	}
	if containsRelayStateComponent(request.StatePath) {
		return errors.New("state_path must not be inside the Rust-owned .relay directory")
	}
	return nil
}

func validateLimit(name string, value, maximum int64) error {
	if value < 1 || value > maximum {
		return fmt.Errorf("%s must be between 1 and %d", name, maximum)
	}
	return nil
}

func containsRelayStateComponent(path string) bool {
	for _, component := range strings.FieldsFunc(filepath.Clean(path), func(r rune) bool {
		return r == '/' || r == '\\'
	}) {
		if component == ".relay" {
			return true
		}
	}
	return false
}

func (request ExecutionRequest) SanitizedArguments() []string {
	arguments := append([]string(nil), request.Arguments...)
	for _, index := range request.RedactArgumentIndexes {
		arguments[index] = "<redacted>"
	}
	return arguments
}

type EnvironmentEntryMetadata struct {
	Name   string `json:"name"`
	Source string `json:"source"`
}

type EnvironmentMetadata struct {
	InheritedAllowlist     []string                   `json:"inherited_allowlist"`
	Effective              []EnvironmentEntryMetadata `json:"effective"`
	BlockedInheritedCount  int                        `json:"blocked_inherited_count"`
	DuplicateUserOverrides int                        `json:"duplicate_user_overrides"`
}

type LifecycleEvent struct {
	Sequence  uint64    `json:"sequence"`
	Lifecycle Lifecycle `json:"lifecycle"`
	Timestamp string    `json:"timestamp"`
}

type OutputObservation struct {
	Sequence uint64 `json:"sequence"`
	Stream   string `json:"stream"`
	Bytes    int    `json:"bytes"`
}

type ObservationLog struct {
	LimitBytes int64               `json:"limit_bytes"`
	BytesUsed  int64               `json:"bytes_used"`
	Truncated  bool                `json:"truncated"`
	Entries    []OutputObservation `json:"entries"`
}

type StreamResult struct {
	Text          string  `json:"text"`
	BytesObserved int64   `json:"bytes_observed"`
	BytesRetained int64   `json:"bytes_retained"`
	Truncated     bool    `json:"truncated"`
	FirstSequence *uint64 `json:"first_sequence"`
	LastSequence  *uint64 `json:"last_sequence"`
}

type TimeoutDetails struct {
	ConfiguredMilliseconds int64 `json:"configured_ms"`
	Triggered              bool  `json:"triggered"`
}

type CancellationDetails struct {
	Requested bool   `json:"requested"`
	Source    string `json:"source"`
}

type TerminationDetails struct {
	GraceMilliseconds int64 `json:"grace_ms"`
	GracefulAttempted bool  `json:"graceful_attempted"`
	Escalated         bool  `json:"escalated"`
}

type ExecutionFailure struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

type ExecutionResult struct {
	RunID                string              `json:"run_id"`
	ProtocolVersion      uint32              `json:"protocol_version"`
	SupervisorVersion    string              `json:"supervisor_version"`
	Program              string              `json:"program"`
	Arguments            []string            `json:"arguments"`
	Environment          EnvironmentMetadata `json:"environment"`
	AcceptedAt           string              `json:"accepted_at"`
	StartedAt            string              `json:"started_at"`
	CompletedAt          string              `json:"completed_at"`
	DurationMilliseconds int64               `json:"duration_ms"`
	Lifecycle            Lifecycle           `json:"lifecycle"`
	TerminationReason    TerminationReason   `json:"termination_reason"`
	ExitCode             *int                `json:"exit_code"`
	Failure              *ExecutionFailure   `json:"failure"`
	Timeout              TimeoutDetails      `json:"timeout"`
	Cancellation         CancellationDetails `json:"cancellation"`
	Termination          TerminationDetails  `json:"termination"`
	Stdout               StreamResult        `json:"stdout"`
	Stderr               StreamResult        `json:"stderr"`
	LifecycleHistory     []LifecycleEvent    `json:"lifecycle_history"`
	OutputObservations   ObservationLog      `json:"output_observations"`
}

type RunRecord struct {
	RunStateVersion   uint32           `json:"run_state_version"`
	SupervisorVersion string           `json:"supervisor_version"`
	StatePath         string           `json:"state_path,omitempty"`
	ProcessIdentity   *AttemptIdentity `json:"process_identity,omitempty"`
	Execution         ExecutionResult  `json:"execution"`
}

type ProcessIdentity struct {
	PID            int    `json:"pid"`
	StartTimeTicks uint64 `json:"start_time_ticks"`
	ProcessGroupID int    `json:"process_group_id"`
	SessionID      int    `json:"session_id"`
	Executable     string `json:"executable"`
}

type AttemptIdentity struct {
	Supervisor ProcessIdentity  `json:"supervisor"`
	Child      *ProcessIdentity `json:"child,omitempty"`
}

type SuccessEnvelope[T any] struct {
	OutputSchemaVersion uint32 `json:"output_schema_version"`
	OK                  bool   `json:"ok"`
	Command             string `json:"command"`
	Data                T      `json:"data"`
}

type ErrorEnvelope struct {
	OutputSchemaVersion uint32      `json:"output_schema_version"`
	OK                  bool        `json:"ok"`
	Command             string      `json:"command"`
	Error               OutputError `json:"error"`
}

type OutputError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func Success(result ExecutionResult) SuccessEnvelope[ExecutionResult] {
	return SuccessEnvelope[ExecutionResult]{
		OutputSchemaVersion: OutputSchemaVersion,
		OK:                  true,
		Command:             SupervisorCommand,
		Data:                result,
	}
}

func Failure(code, message string) ErrorEnvelope {
	return ErrorEnvelope{
		OutputSchemaVersion: OutputSchemaVersion,
		OK:                  false,
		Command:             SupervisorCommand,
		Error:               OutputError{Code: code, Message: message},
	}
}
