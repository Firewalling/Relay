package supervisor

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"time"

	"relay/protocol"
)

type cancellationSourceKey struct{}

func WithCancellationSource(ctx context.Context, source string) context.Context {
	return context.WithValue(ctx, cancellationSourceKey{}, source)
}

type runStateStore interface {
	Save(string, protocol.ExecutionResult) error
}

type Runner struct {
	ParentEnvironment []string
	Now               func() time.Time
	Store             runStateStore
	tree              processTree
}

func NewRunner() *Runner {
	return &Runner{
		ParentEnvironment: os.Environ(),
		Now:               time.Now,
		Store:             NewStateStore(),
		tree:              newProcessTree(),
	}
}

func (runner *Runner) Execute(ctx context.Context, request protocol.ExecutionRequest) (protocol.ExecutionResult, error) {
	if err := request.Validate(); err != nil {
		return protocol.ExecutionResult{}, err
	}
	if runner.Now == nil || runner.Store == nil || runner.tree == nil {
		return protocol.ExecutionResult{}, errors.New("supervisor runner is not fully initialized")
	}
	if !runner.tree.supported() {
		return protocol.ExecutionResult{}, errors.New("process-tree supervision is unsupported on this platform")
	}
	environment, environmentMetadata, err := buildEnvironment(runner.ParentEnvironment, request.Environment, request.RunID)
	if err != nil {
		return protocol.ExecutionResult{}, err
	}

	recorder := newObservationRecorder(request.LogLimitBytes)
	acceptedAt := runner.Now()
	result := protocol.ExecutionResult{
		RunID:             request.RunID,
		ProtocolVersion:   request.ProtocolVersion,
		SupervisorVersion: protocol.SupervisorVersion,
		Program:           request.Program,
		Arguments:         request.SanitizedArguments(),
		Environment:       environmentMetadata,
		AcceptedAt:        formatTimestamp(acceptedAt),
		Lifecycle:         protocol.LifecycleAccepted,
		Timeout: protocol.TimeoutDetails{
			ConfiguredMilliseconds: request.TimeoutMilliseconds,
		},
		Cancellation: protocol.CancellationDetails{Source: "none"},
		Termination: protocol.TerminationDetails{
			GraceMilliseconds: request.TerminationGraceMilliseconds,
		},
		Stdout: protocol.StreamResult{Text: ""},
		Stderr: protocol.StreamResult{Text: ""},
		LifecycleHistory: []protocol.LifecycleEvent{
			recorder.lifecycle(protocol.LifecycleAccepted, acceptedAt),
		},
		OutputObservations: recorder.snapshot(),
	}
	if err := runner.Store.Save(request.StatePath, result); err != nil {
		return protocol.ExecutionResult{}, fmt.Errorf("persist accepted run state: %w", err)
	}

	select {
	case <-ctx.Done():
		completedAt := runner.Now()
		result.StartedAt = result.AcceptedAt
		result.CompletedAt = formatTimestamp(completedAt)
		result.DurationMilliseconds = nonnegativeMilliseconds(completedAt.Sub(acceptedAt))
		result.Lifecycle = protocol.LifecycleCancelled
		result.TerminationReason = protocol.TerminationCancellationTerminated
		result.Cancellation = protocol.CancellationDetails{Requested: true, Source: cancellationSource(ctx)}
		result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleCancelled, completedAt))
		result.OutputObservations = recorder.snapshot()
		if err := runner.Store.Save(request.StatePath, result); err != nil {
			return protocol.ExecutionResult{}, fmt.Errorf("persist cancelled run state: %w", err)
		}
		return result, nil
	default:
	}

	command := exec.Command(request.Program, request.Arguments...)
	command.Dir = request.WorkingDirectory
	command.Env = environment
	runner.tree.configure(command)
	stdoutCapture := newLimitedCapture("stdout", request.StdoutLimitBytes, recorder)
	stderrCapture := newLimitedCapture("stderr", request.StderrLimitBytes, recorder)
	command.Stdout = stdoutCapture
	command.Stderr = stderrCapture

	startedAt := runner.Now()
	result.StartedAt = formatTimestamp(startedAt)
	result.Lifecycle = protocol.LifecycleStarting
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleStarting, startedAt))
	result.OutputObservations = recorder.snapshot()
	if err := runner.Store.Save(request.StatePath, result); err != nil {
		return protocol.ExecutionResult{}, fmt.Errorf("persist starting run state: %w", err)
	}

	if err := command.Start(); err != nil {
		completedAt := runner.Now()
		result.CompletedAt = formatTimestamp(completedAt)
		result.DurationMilliseconds = nonnegativeMilliseconds(completedAt.Sub(startedAt))
		result.Lifecycle = protocol.LifecycleFailedToStart
		result.TerminationReason = protocol.TerminationStartError
		result.Failure = startFailure(err)
		result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleFailedToStart, completedAt))
		result.OutputObservations = recorder.snapshot()
		if saveErr := runner.Store.Save(request.StatePath, result); saveErr != nil {
			return protocol.ExecutionResult{}, fmt.Errorf("persist failed-to-start run state: %w", saveErr)
		}
		return result, nil
	}

	var timeoutTimer *time.Timer
	var timeout <-chan time.Time
	if request.TimeoutMilliseconds > 0 {
		timeoutTimer = time.NewTimer(time.Duration(request.TimeoutMilliseconds) * time.Millisecond)
		timeout = timeoutTimer.C
		defer timeoutTimer.Stop()
	}
	waitDone := make(chan error, 1)
	go func() { waitDone <- command.Wait() }()
	monitorContext, stopMonitor := context.WithCancel(ctx)
	defer stopMonitor()
	completionDone := make(chan completionOutcome, 1)
	go func() {
		completionDone <- runner.waitForCompletion(monitorContext, command, waitDone, request, timeout)
	}()

	runningAt := runner.Now()
	result.Lifecycle = protocol.LifecycleRunning
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleRunning, runningAt))
	result.OutputObservations = recorder.snapshot()
	if err := runner.Store.Save(request.StatePath, result); err != nil {
		stopMonitor()
		outcome := <-completionDone
		if outcome.err != nil {
			return protocol.ExecutionResult{}, fmt.Errorf("persist running run state: %v; process cleanup: %w", err, outcome.err)
		}
		return protocol.ExecutionResult{}, fmt.Errorf("persist running run state: %w", err)
	}

	outcome := <-completionDone
	result.Stdout = stdoutCapture.result()
	result.Stderr = stderrCapture.result()
	redactCapturedSecrets(&result.Stdout, request)
	redactCapturedSecrets(&result.Stderr, request)
	result.OutputObservations = recorder.snapshot()
	if outcome.err != nil {
		return runner.failProtocol(request.StatePath, result, recorder, "termination_error", outcome.err)
	}
	if outcome.waitErr != nil {
		var exitError *exec.ExitError
		if !errors.As(outcome.waitErr, &exitError) {
			return runner.failProtocol(request.StatePath, result, recorder, "wait_error", outcome.waitErr)
		}
	}

	completedAt := runner.Now()
	result.CompletedAt = formatTimestamp(completedAt)
	result.DurationMilliseconds = nonnegativeMilliseconds(completedAt.Sub(startedAt))
	result.ExitCode = availableExitCode(command)
	result.Termination.GracefulAttempted = outcome.trigger != terminationNone
	result.Termination.Escalated = outcome.escalated
	if outcome.trigger == terminationCancellation {
		result.Cancellation.Source = cancellationSource(ctx)
	}
	applyTerminationResult(&result, outcome.trigger, outcome.escalated)
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(result.Lifecycle, completedAt))
	result.OutputObservations = recorder.snapshot()
	if err := runner.Store.Save(request.StatePath, result); err != nil {
		return protocol.ExecutionResult{}, fmt.Errorf("persist completed run state: %w", err)
	}
	return result, nil
}

type terminationTrigger uint8

const (
	terminationNone terminationTrigger = iota
	terminationTimeout
	terminationCancellation
	terminationDescendants
)

type completionOutcome struct {
	trigger   terminationTrigger
	waitErr   error
	escalated bool
	err       error
}

func (runner *Runner) waitForCompletion(
	ctx context.Context,
	command *exec.Cmd,
	waitDone <-chan error,
	request protocol.ExecutionRequest,
	timeout <-chan time.Time,
) completionOutcome {
	select {
	case waitErr := <-waitDone:
		if !runner.tree.alive(command.Process) {
			return completionOutcome{waitErr: waitErr}
		}
		return runner.cleanupSurvivingTree(command.Process, waitErr, request)
	case <-timeout:
		return runner.terminateRunningTree(command.Process, waitDone, request, terminationTimeout)
	case <-ctx.Done():
		return runner.terminateRunningTree(command.Process, waitDone, request, terminationCancellation)
	}
}

func (runner *Runner) cleanupSurvivingTree(process *os.Process, waitErr error, request protocol.ExecutionRequest) completionOutcome {
	outcome := completionOutcome{trigger: terminationDescendants, waitErr: waitErr}
	if err := runner.tree.graceful(process); err != nil {
		outcome.escalated = true
		if killErr := runner.tree.kill(process); killErr != nil {
			outcome.err = fmt.Errorf("graceful descendant termination failed (%v) and forced termination failed: %w", err, killErr)
			return outcome
		}
		outcome.err = runner.waitForTreeExit(process, request)
		return outcome
	}
	if runner.waitForTreeGrace(process, request) {
		return outcome
	}
	outcome.escalated = true
	if err := runner.tree.kill(process); err != nil {
		outcome.err = fmt.Errorf("force descendant termination: %w", err)
		return outcome
	}
	outcome.err = runner.waitForTreeExit(process, request)
	return outcome
}

func (runner *Runner) terminateRunningTree(
	process *os.Process,
	waitDone <-chan error,
	request protocol.ExecutionRequest,
	trigger terminationTrigger,
) completionOutcome {
	outcome := completionOutcome{trigger: trigger}
	if err := runner.tree.graceful(process); err != nil {
		outcome.escalated = true
		if killErr := runner.tree.kill(process); killErr != nil {
			outcome.err = fmt.Errorf("graceful termination failed (%v) and forced termination failed: %w", err, killErr)
			return outcome
		}
		outcome.waitErr = <-waitDone
		outcome.err = runner.waitForTreeExit(process, request)
		return outcome
	}

	grace := time.NewTimer(time.Duration(request.TerminationGraceMilliseconds) * time.Millisecond)
	defer grace.Stop()
	ticker := time.NewTicker(5 * time.Millisecond)
	defer ticker.Stop()
	waitChannel := waitDone
	waited := false
	for {
		if waited && !runner.tree.alive(process) {
			return outcome
		}
		select {
		case waitErr := <-waitChannel:
			outcome.waitErr = waitErr
			waited = true
			waitChannel = nil
		case <-ticker.C:
		case <-grace.C:
			outcome.escalated = true
			if err := runner.tree.kill(process); err != nil {
				outcome.err = fmt.Errorf("force process-tree termination: %w", err)
				return outcome
			}
			if !waited {
				outcome.waitErr = <-waitChannel
			}
			_ = runner.tree.kill(process)
			outcome.err = runner.waitForTreeExit(process, request)
			return outcome
		}
	}
}

func (runner *Runner) waitForTreeGrace(process *os.Process, request protocol.ExecutionRequest) bool {
	deadline := time.NewTimer(time.Duration(request.TerminationGraceMilliseconds) * time.Millisecond)
	defer deadline.Stop()
	ticker := time.NewTicker(5 * time.Millisecond)
	defer ticker.Stop()
	for runner.tree.alive(process) {
		select {
		case <-deadline.C:
			return false
		case <-ticker.C:
		}
	}
	return true
}

func (runner *Runner) waitForTreeExit(process *os.Process, request protocol.ExecutionRequest) error {
	wait := time.Duration(request.TerminationGraceMilliseconds) * time.Millisecond
	if wait < 250*time.Millisecond {
		wait = 250 * time.Millisecond
	}
	deadline := time.NewTimer(wait)
	defer deadline.Stop()
	ticker := time.NewTicker(5 * time.Millisecond)
	defer ticker.Stop()
	for runner.tree.alive(process) {
		select {
		case <-deadline.C:
			return errors.New("process tree remained alive after forced termination")
		case <-ticker.C:
		}
	}
	return nil
}

func (runner *Runner) failProtocol(path string, result protocol.ExecutionResult, recorder *observationRecorder, code string, failure error) (protocol.ExecutionResult, error) {
	completedAt := runner.Now()
	if result.StartedAt == "" {
		result.StartedAt = result.AcceptedAt
	}
	result.CompletedAt = formatTimestamp(completedAt)
	result.Lifecycle = protocol.LifecycleProtocolFailure
	result.TerminationReason = protocol.TerminationProtocolFailure
	result.Failure = &protocol.ExecutionFailure{Code: code, Message: failure.Error()}
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleProtocolFailure, completedAt))
	result.OutputObservations = recorder.snapshot()
	if saveErr := runner.Store.Save(path, result); saveErr != nil {
		return protocol.ExecutionResult{}, fmt.Errorf("%s: %v; persist protocol-failure state: %w", code, failure, saveErr)
	}
	return protocol.ExecutionResult{}, fmt.Errorf("%s: %w", code, failure)
}

func startFailure(err error) *protocol.ExecutionFailure {
	code := "start_error"
	if errors.Is(err, os.ErrNotExist) {
		code = "executable_not_found"
	} else if errors.Is(err, os.ErrPermission) {
		code = "executable_not_executable"
	}
	return &protocol.ExecutionFailure{Code: code, Message: err.Error()}
}

func availableExitCode(command *exec.Cmd) *int {
	if command.ProcessState == nil {
		return nil
	}
	code := command.ProcessState.ExitCode()
	if code < 0 {
		return nil
	}
	return &code
}

func applyTerminationResult(result *protocol.ExecutionResult, trigger terminationTrigger, escalated bool) {
	switch trigger {
	case terminationNone:
		result.Lifecycle = protocol.LifecycleCompleted
		if result.ExitCode != nil && *result.ExitCode == 0 {
			result.TerminationReason = protocol.TerminationExitZero
		} else {
			result.TerminationReason = protocol.TerminationExitNonzero
		}
	case terminationTimeout:
		result.Timeout.Triggered = true
		if escalated {
			result.Lifecycle = protocol.LifecycleKilled
			result.TerminationReason = protocol.TerminationTimeoutKilled
		} else {
			result.Lifecycle = protocol.LifecycleTimedOut
			result.TerminationReason = protocol.TerminationTimeoutTerminated
		}
	case terminationCancellation:
		result.Cancellation.Requested = true
		if result.Cancellation.Source == "none" {
			result.Cancellation.Source = "context"
		}
		if escalated {
			result.Lifecycle = protocol.LifecycleKilled
			result.TerminationReason = protocol.TerminationCancellationKilled
		} else {
			result.Lifecycle = protocol.LifecycleCancelled
			result.TerminationReason = protocol.TerminationCancellationTerminated
		}
	case terminationDescendants:
		if escalated {
			result.Lifecycle = protocol.LifecycleKilled
			result.TerminationReason = protocol.TerminationDescendantsKilled
		} else {
			result.Lifecycle = protocol.LifecycleCompleted
			result.TerminationReason = protocol.TerminationDescendantsTerminated
		}
	}
}

func redactCapturedSecrets(stream *protocol.StreamResult, request protocol.ExecutionRequest) {
	for _, index := range request.RedactArgumentIndexes {
		secret := request.Arguments[index]
		if secret == "" {
			continue
		}
		if strings.Contains(stream.Text, secret) || truncatedSecretPrefix(stream.Text, secret, stream.Truncated) {
			stream.Text = "<redacted>"
			return
		}
	}
}

func truncatedSecretPrefix(text string, secret string, truncated bool) bool {
	if !truncated {
		return false
	}
	maximumPrefix := len(secret) - 1
	if maximumPrefix > len(text) {
		maximumPrefix = len(text)
	}
	for length := maximumPrefix; length > 0; length-- {
		if strings.HasSuffix(text, secret[:length]) {
			return true
		}
	}
	return false
}

func cancellationSource(ctx context.Context) string {
	if source, ok := ctx.Value(cancellationSourceKey{}).(string); ok && source != "" {
		return source
	}
	return "context"
}

func nonnegativeMilliseconds(duration time.Duration) int64 {
	if duration < 0 {
		return 0
	}
	return duration.Milliseconds()
}
