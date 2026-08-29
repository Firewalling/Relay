package supervisor

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
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

type identifiedRunStateStore interface {
	SaveWithIdentity(string, protocol.ExecutionResult, *protocol.AttemptIdentity) error
}

func (runner *Runner) saveState(path string, result protocol.ExecutionResult, identity *protocol.AttemptIdentity) error {
	if store, ok := runner.Store.(identifiedRunStateStore); ok {
		return store.SaveWithIdentity(path, result, identity)
	}
	return runner.Store.Save(path, result)
}

type Runner struct {
	ParentEnvironment []string
	Now               func() time.Time
	Store             runStateStore
	tree              processTree
	timing            runnerTiming
	hooks             runnerHooks
}

type runnerHooks struct {
	drainStarted         func()
	drainFinished        func()
	beforeCaptureResults func()
}

type runnerTiming struct {
	pollInterval time.Duration
	forcedWait   time.Duration
	drainWait    time.Duration
}

func defaultRunnerTiming() runnerTiming {
	return runnerTiming{
		// A 20ms cadence keeps /proc scans responsive without needlessly walking
		// the process table; scans are necessary to see leader-exited descendants.
		pollInterval: 20 * time.Millisecond,
		forcedWait:   250 * time.Millisecond,
		drainWait:    250 * time.Millisecond,
	}
}
func NewRunner() *Runner {
	return &Runner{
		ParentEnvironment: os.Environ(),
		Now:               time.Now,
		Store:             NewStateStore(),
		tree:              newProcessTree(),
		timing:            defaultRunnerTiming(),
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

	var attemptIdentity *protocol.AttemptIdentity
	if supervisorIdentity, identityErr := captureProcessIdentity(os.Getpid()); identityErr == nil {
		attemptIdentity = &protocol.AttemptIdentity{Supervisor: supervisorIdentity}
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
	if err := runner.saveState(request.StatePath, result, attemptIdentity); err != nil {
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
		if err := runner.saveState(request.StatePath, result, attemptIdentity); err != nil {
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
	stdoutReader, stdoutWriter, err := os.Pipe()
	if err != nil {
		return protocol.ExecutionResult{}, fmt.Errorf("create stdout pipe: %w", err)
	}
	stderrReader, stderrWriter, err := os.Pipe()
	if err != nil {
		_ = stdoutReader.Close()
		_ = stdoutWriter.Close()
		return protocol.ExecutionResult{}, fmt.Errorf("create stderr pipe: %w", err)
	}
	command.Stdout = stdoutWriter
	command.Stderr = stderrWriter

	startedAt := runner.Now()
	result.StartedAt = formatTimestamp(startedAt)
	result.Lifecycle = protocol.LifecycleStarting
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleStarting, startedAt))
	result.OutputObservations = recorder.snapshot()
	if err := runner.saveState(request.StatePath, result, attemptIdentity); err != nil {
		_ = stdoutReader.Close()
		_ = stdoutWriter.Close()
		_ = stderrReader.Close()
		_ = stderrWriter.Close()
		return protocol.ExecutionResult{}, fmt.Errorf("persist starting run state: %w", err)
	}

	if err := command.Start(); err != nil {
		_ = stdoutReader.Close()
		_ = stdoutWriter.Close()
		_ = stderrReader.Close()
		_ = stderrWriter.Close()
		completedAt := runner.Now()
		result.CompletedAt = formatTimestamp(completedAt)
		result.DurationMilliseconds = nonnegativeMilliseconds(completedAt.Sub(startedAt))
		result.Lifecycle = protocol.LifecycleFailedToStart
		result.TerminationReason = protocol.TerminationStartError
		result.Failure = startFailure(err)
		result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleFailedToStart, completedAt))
		result.OutputObservations = recorder.snapshot()
		if saveErr := runner.saveState(request.StatePath, result, attemptIdentity); saveErr != nil {
			return protocol.ExecutionResult{}, fmt.Errorf("persist failed-to-start run state: %w", saveErr)
		}
		return result, nil
	}

	if attemptIdentity != nil {
		if childIdentity, identityErr := captureProcessIdentity(command.Process.Pid); identityErr == nil {
			identity := *attemptIdentity
			identity.Child = &childIdentity
			attemptIdentity = &identity
		}
	}
	runningAt := runner.Now()
	result.Lifecycle = protocol.LifecycleRunning
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(protocol.LifecycleRunning, runningAt))
	result.OutputObservations = recorder.snapshot()

	_ = stdoutWriter.Close()
	_ = stderrWriter.Close()
	drainDone := make(chan error, 2)
	var drainGroup sync.WaitGroup
	for _, drain := range []struct {
		reader  *os.File
		capture *limitedCapture
	}{{stdoutReader, stdoutCapture}, {stderrReader, stderrCapture}} {
		drainGroup.Add(1)
		if runner.hooks.drainStarted != nil {
			runner.hooks.drainStarted()
		}
		go func(reader *os.File, capture *limitedCapture) {
			defer drainGroup.Done()
			if runner.hooks.drainFinished != nil {
				defer runner.hooks.drainFinished()
			}
			drainDone <- capture.consume(reader)
		}(drain.reader, drain.capture)
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

	if err := runner.saveState(request.StatePath, result, attemptIdentity); err != nil {
		stopMonitor()
		outcome := <-completionDone
		drainErr := runner.awaitDrains(drainDone, &drainGroup, stdoutReader, stderrReader)
		if drainErr != nil {
			return protocol.ExecutionResult{}, fmt.Errorf("persist running run state: %v; output drain cleanup: %w", err, drainErr)
		}
		if outcome.infrastructureFailure != nil {
			return protocol.ExecutionResult{}, fmt.Errorf("persist running run state: %v; process cleanup: %w", err, outcome.infrastructureFailure)
		}
		return protocol.ExecutionResult{}, fmt.Errorf("persist running run state: %w", err)
	}

	outcome := <-completionDone
	if drainErr := runner.awaitDrains(drainDone, &drainGroup, stdoutReader, stderrReader); drainErr != nil {
		outcome.infrastructureFailure = errors.Join(outcome.infrastructureFailure, drainErr)
	} else {
		outcome.drainsComplete = true
	}
	if outcome.drainsComplete {
		if runner.hooks.beforeCaptureResults != nil {
			runner.hooks.beforeCaptureResults()
		}
		result.Stdout = stdoutCapture.result()
		result.Stderr = stderrCapture.result()
		redactCapturedSecrets(&result.Stdout, request)
		redactCapturedSecrets(&result.Stderr, request)
		result.OutputObservations = recorder.snapshot()
	}
	result.Termination.GracefulAttempted = outcome.gracefulAttempted
	result.Termination.Escalated = outcome.escalationAttempted
	if outcome.directChildReaped {
		result.ExitCode = availableExitCode(command)
	}
	if outcome.trigger == terminationCancellation {
		result.Cancellation.Source = cancellationSource(ctx)
	}
	if outcome.infrastructureFailure != nil {
		return runner.failProtocol(request.StatePath, result, recorder, "termination_error", outcome.infrastructureFailure, attemptIdentity)
	}
	if !outcome.directChildReaped {
		return runner.failProtocol(request.StatePath, result, recorder, "termination_error", errors.New("direct child was not reaped"), attemptIdentity)
	}
	if outcome.waitErr != nil {
		var exitError *exec.ExitError
		if !errors.As(outcome.waitErr, &exitError) {
			return runner.failProtocol(request.StatePath, result, recorder, "wait_error", outcome.waitErr, attemptIdentity)
		}
	}
	completedAt := runner.Now()
	result.CompletedAt = formatTimestamp(completedAt)
	result.DurationMilliseconds = nonnegativeMilliseconds(completedAt.Sub(startedAt))
	applyTerminationResult(&result, outcome.trigger, outcome.escalationAttempted)
	result.LifecycleHistory = append(result.LifecycleHistory, recorder.lifecycle(result.Lifecycle, completedAt))
	result.OutputObservations = recorder.snapshot()
	if err := runner.saveState(request.StatePath, result, attemptIdentity); err != nil {
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
	trigger               terminationTrigger
	waitErr               error
	directChildReaped     bool
	drainsComplete        bool
	gracefulAttempted     bool
	escalationAttempted   bool
	groupStillAlive       bool
	infrastructureFailure error
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
		return runner.completeDirectWait(command.Process, waitErr, request)
	case <-timeout:
		select {
		case waitErr := <-waitDone:
			return runner.completeDirectWait(command.Process, waitErr, request)
		default:
			return runner.terminateRunningTree(command.Process, waitDone, request, terminationTimeout)
		}
	case <-ctx.Done():
		select {
		case waitErr := <-waitDone:
			return runner.completeDirectWait(command.Process, waitErr, request)
		default:
			return runner.terminateRunningTree(command.Process, waitDone, request, terminationCancellation)
		}
	}
}

func (runner *Runner) completeDirectWait(process *os.Process, waitErr error, request protocol.ExecutionRequest) completionOutcome {
	outcome := completionOutcome{waitErr: waitErr, directChildReaped: true}
	if !runner.tree.alive(process) {
		return outcome
	}
	return runner.cleanupSurvivingTree(process, outcome, request)
}

func (runner *Runner) cleanupSurvivingTree(process *os.Process, outcome completionOutcome, request protocol.ExecutionRequest) completionOutcome {
	outcome.trigger = terminationDescendants
	outcome.gracefulAttempted = true
	if err := runner.tree.graceful(process); err != nil {
		outcome.escalationAttempted = true
		if killErr := runner.tree.kill(process); killErr != nil {
			outcome.infrastructureFailure = fmt.Errorf("graceful descendant termination failed (%v) and forced termination failed: %w", err, killErr)
			return outcome
		}
		outcome.groupStillAlive = !runner.waitForTreeExit(process, runner.forcedWait())
		if outcome.groupStillAlive {
			outcome.infrastructureFailure = errors.New("process tree remained alive after forced termination")
		}
		return outcome
	}
	if runner.waitForTreeGrace(process, request) {
		return outcome
	}
	outcome.escalationAttempted = true
	if err := runner.tree.kill(process); err != nil {
		outcome.infrastructureFailure = fmt.Errorf("force descendant termination: %w", err)
		return outcome
	}
	outcome.groupStillAlive = !runner.waitForTreeExit(process, runner.forcedWait())
	if outcome.groupStillAlive {
		outcome.infrastructureFailure = errors.New("process tree remained alive after forced termination")
	}
	return outcome
}

func (runner *Runner) terminateRunningTree(
	process *os.Process,
	waitDone <-chan error,
	request protocol.ExecutionRequest,
	trigger terminationTrigger,
) completionOutcome {
	outcome := completionOutcome{trigger: trigger, gracefulAttempted: true}
	if err := runner.tree.graceful(process); err != nil {
		outcome.escalationAttempted = true
		if killErr := runner.tree.kill(process); killErr != nil {
			outcome.infrastructureFailure = fmt.Errorf("graceful termination failed (%v) and forced termination failed: %w", err, killErr)
			return outcome
		}
		return runner.awaitForcedTermination(process, waitDone, outcome)
	}
	grace := time.NewTimer(time.Duration(request.TerminationGraceMilliseconds) * time.Millisecond)
	defer grace.Stop()
	if runner.awaitTermination(process, waitDone, &outcome, grace.C) {
		return outcome
	}
	select {
	case waitErr := <-waitDone:
		outcome.waitErr = waitErr
		outcome.directChildReaped = true
	default:
	}
	if outcome.directChildReaped && !runner.tree.alive(process) {
		return outcome
	}
	outcome.escalationAttempted = true
	if err := runner.tree.kill(process); err != nil {
		outcome.infrastructureFailure = fmt.Errorf("force process-tree termination: %w", err)
		return outcome
	}
	return runner.awaitForcedTermination(process, waitDone, outcome)
}

func (runner *Runner) awaitForcedTermination(process *os.Process, waitDone <-chan error, outcome completionOutcome) completionOutcome {
	deadline := time.NewTimer(runner.forcedWait())
	defer deadline.Stop()
	if !runner.awaitTermination(process, waitDone, &outcome, deadline.C) {
		if !outcome.directChildReaped {
			outcome.infrastructureFailure = errors.New("direct child was not reaped after forced termination")
		} else if outcome.groupStillAlive {
			outcome.infrastructureFailure = errors.New("process tree remained alive after forced termination")
		}
	}
	return outcome
}

func (runner *Runner) awaitTermination(process *os.Process, waitDone <-chan error, outcome *completionOutcome, deadline <-chan time.Time) bool {
	ticker := time.NewTicker(runner.pollInterval())
	defer ticker.Stop()
	for {
		outcome.groupStillAlive = runner.tree.alive(process)
		if outcome.directChildReaped && !outcome.groupStillAlive {
			return true
		}
		select {
		case waitErr := <-waitDone:
			outcome.waitErr = waitErr
			outcome.directChildReaped = true
			waitDone = nil
		case <-ticker.C:
		case <-deadline:
			outcome.groupStillAlive = runner.tree.alive(process)
			return false
		}
	}
}

func (runner *Runner) waitForTreeGrace(process *os.Process, request protocol.ExecutionRequest) bool {
	return runner.waitForTreeExit(process, time.Duration(request.TerminationGraceMilliseconds)*time.Millisecond)
}

func (runner *Runner) waitForTreeExit(process *os.Process, wait time.Duration) bool {
	deadline := time.NewTimer(wait)
	defer deadline.Stop()
	ticker := time.NewTicker(runner.pollInterval())
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

func (runner *Runner) pollInterval() time.Duration {
	if runner.timing.pollInterval > 0 {
		return runner.timing.pollInterval
	}
	return defaultRunnerTiming().pollInterval
}

func (runner *Runner) forcedWait() time.Duration {
	if runner.timing.forcedWait > 0 {
		return runner.timing.forcedWait
	}
	return defaultRunnerTiming().forcedWait
}

func (runner *Runner) drainWait() time.Duration {
	if runner.timing.drainWait > 0 {
		return runner.timing.drainWait
	}
	return defaultRunnerTiming().drainWait
}

func (runner *Runner) awaitDrains(done <-chan error, group *sync.WaitGroup, readers ...*os.File) error {
	remaining := len(readers)
	closed := false
	deadline := time.NewTimer(runner.drainWait())
	defer deadline.Stop()
	var drainErr error
	for remaining > 0 {
		select {
		case err := <-done:
			remaining--
			if err != nil {
				drainErr = errors.Join(drainErr, err)
			}
		case <-deadline.C:
			if closed {
				return errors.Join(drainErr, errors.New("output drains did not stop after reader closure"))
			}
			closed = true
			drainErr = errors.Join(drainErr, errors.New("output drains exceeded bounded wait; readers were closed"))
			for _, reader := range readers {
				_ = reader.Close()
			}
			deadline.Reset(runner.forcedWait())
		}
	}
	if !closed {
		for _, reader := range readers {
			_ = reader.Close()
		}
	}
	group.Wait()
	return drainErr
}

func (runner *Runner) failProtocol(path string, result protocol.ExecutionResult, recorder *observationRecorder, code string, failure error, identity *protocol.AttemptIdentity) (protocol.ExecutionResult, error) {
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
	if saveErr := runner.saveState(path, result, identity); saveErr != nil {
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
