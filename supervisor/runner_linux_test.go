//go:build linux

package supervisor

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"relay/protocol"
)

const helperTestArgument = "-test.run=^TestSupervisorHelperProcess$"

func TestSupervisorHelperProcess(t *testing.T) {
	mode := os.Getenv("SUPERVISOR_HELPER_MODE")
	if mode == "" {
		return
	}
	os.Exit(runSupervisorHelper(mode))
}

func runSupervisorHelper(mode string) int {
	switch mode {
	case "echo":
		_, _ = fmt.Fprint(os.Stdout, strings.Join(argumentsAfterDoubleDash(), "\n"))
		return 0
	case "exit":
		code, _ := strconv.Atoi(os.Getenv("SUPERVISOR_HELPER_EXIT_CODE"))
		return code
	case "sleep":
		writeHelperFile("SUPERVISOR_HELPER_PID", strconv.Itoa(os.Getpid()))
		writeHelperFile("SUPERVISOR_HELPER_READY", "ready")
		time.Sleep(10 * time.Minute)
		return 0
	case "graceful":
		channel := make(chan os.Signal, 1)
		signal.Notify(channel, syscall.SIGTERM)
		writeHelperFile("SUPERVISOR_HELPER_READY", "ready")
		<-channel
		writeHelperFile("SUPERVISOR_HELPER_TERMINATED", "terminated")
		return 0
	case "ignore":
		writeHelperFile("SUPERVISOR_HELPER_PID", strconv.Itoa(os.Getpid()))
		signal.Ignore(syscall.SIGTERM)
		writeHelperFile("SUPERVISOR_HELPER_READY", "ready")
		time.Sleep(10 * time.Minute)
		return 0
	case "output":
		stdoutBytes, _ := strconv.Atoi(os.Getenv("SUPERVISOR_HELPER_STDOUT_BYTES"))
		stderrBytes, _ := strconv.Atoi(os.Getenv("SUPERVISOR_HELPER_STDERR_BYTES"))
		if os.Getenv("SUPERVISOR_HELPER_INVALID_UTF8") == "1" {
			_, _ = os.Stdout.Write([]byte{0xff, 'x'})
			return 0
		}
		var wait sync.WaitGroup
		wait.Add(2)
		go func() {
			defer wait.Done()
			writeRepeated(os.Stdout, 'o', stdoutBytes)
		}()
		go func() {
			defer wait.Done()
			writeRepeated(os.Stderr, 'e', stderrBytes)
		}()
		wait.Wait()
		return 0
	case "environment":
		values := []string{
			valueOrUnset("PATH"),
			valueOrUnset("GREETING"),
			valueOrUnset("RELAY_RUN_ID"),
			valueOrUnset("HOME"),
			valueOrUnset("API_TOKEN"),
		}
		_, _ = fmt.Fprint(os.Stdout, strings.Join(values, "|"))
		return 0
	case "spawn-tree":
		signal.Ignore(syscall.SIGTERM)
		command := exec.Command(os.Args[0], helperTestArgument)
		command.Env = replaceEnvironment(os.Environ(), "SUPERVISOR_HELPER_MODE", "grandchild")
		command.Stdout = io.Discard
		command.Stderr = io.Discard
		if err := command.Start(); err != nil {
			_, _ = fmt.Fprintln(os.Stderr, err)
			return 91
		}
		writeHelperFile("SUPERVISOR_HELPER_TREE_PID", strconv.Itoa(command.Process.Pid))
		writeHelperFile("SUPERVISOR_HELPER_READY", "ready")
		time.Sleep(10 * time.Minute)
		return 0
	case "spawn-tree-exit":
		command := exec.Command(os.Args[0], helperTestArgument)
		command.Env = replaceEnvironment(os.Environ(), "SUPERVISOR_HELPER_MODE", "grandchild")
		command.Stdout = io.Discard
		command.Stderr = io.Discard
		if err := command.Start(); err != nil {
			_, _ = fmt.Fprintln(os.Stderr, err)
			return 91
		}
		writeHelperFile("SUPERVISOR_HELPER_TREE_PID", strconv.Itoa(command.Process.Pid))
		return 0
	case "grandchild":
		signal.Ignore(syscall.SIGTERM)
		time.Sleep(10 * time.Minute)
		return 0
	case "supervisor-wrapper":
		return runSupervisorWrapper()
	default:
		_, _ = fmt.Fprintf(os.Stderr, "unknown helper mode %q", mode)
		return 92
	}
}

func TestExecuteSuccessUsesLiteralArgvAndRedactsDeclaredSecrets(t *testing.T) {
	request := testRequest(t, "echo")
	wouldBeShellMarker := filepath.Join(t.TempDir(), "shell-created")
	literal := "; touch " + wouldBeShellMarker + " && $(echo hidden) | cat"
	secret := "argument-secret-value"
	request.Arguments = []string{helperTestArgument, "--", literal, secret}
	request.RedactArgumentIndexes = []int{3}

	result, err := testRunner().Execute(context.Background(), request)
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if result.Lifecycle != protocol.LifecycleCompleted || result.TerminationReason != protocol.TerminationExitZero {
		t.Fatalf("completion = %s/%s", result.Lifecycle, result.TerminationReason)
	}
	if result.ExitCode == nil || *result.ExitCode != 0 {
		t.Fatalf("exit code = %v, want 0", result.ExitCode)
	}
	if got, want := result.Stdout.Text, "<redacted>"; got != want {
		t.Fatalf("stdout = %q, want conservative whole-stream redaction %q", got, want)
	}
	if _, err := os.Stat(wouldBeShellMarker); !os.IsNotExist(err) {
		t.Fatalf("shell metacharacters were evaluated; marker stat error = %v", err)
	}
	assertLifecycle(t, result, protocol.LifecycleAccepted, protocol.LifecycleStarting, protocol.LifecycleRunning, protocol.LifecycleCompleted)
	assertSecretAbsent(t, request.StatePath, result, secret)
}

func TestExecuteRedactsSecretCutByOutputLimit(t *testing.T) {
	request := testRequest(t, "echo")
	secret := "long-secret-argument"
	request.Arguments = []string{helperTestArgument, "--", secret}
	request.RedactArgumentIndexes = []int{2}
	request.StdoutLimitBytes = 5

	result, err := testRunner().Execute(context.Background(), request)
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if result.Stdout.Text != "<redacted>" || !result.Stdout.Truncated {
		t.Fatalf("truncated secret output = %#v", result.Stdout)
	}
	assertSecretAbsent(t, request.StatePath, result, secret)
}

func TestRedactCapturedSecretsIsBoundedForRepeatedAndOverlappingValues(t *testing.T) {
	request := protocol.ExecutionRequest{
		Arguments:             make([]string, 30),
		RedactArgumentIndexes: make([]int, 30),
	}
	for index := range request.Arguments {
		request.Arguments[index] = "e"
		request.RedactArgumentIndexes[index] = index
	}
	stream := protocol.StreamResult{Text: "TOPSECRET-e"}
	redactCapturedSecrets(&stream, request)
	if stream.Text != "<redacted>" {
		t.Fatalf("repeated secret redaction = %q", stream.Text)
	}

	request.Arguments = []string{"X", "TOPSECRET-X"}
	request.RedactArgumentIndexes = []int{0, 1}
	stream.Text = "TOPSECRET-X"
	redactCapturedSecrets(&stream, request)
	if stream.Text != "<redacted>" {
		t.Fatalf("overlapping secret redaction = %q", stream.Text)
	}
}

func TestExecuteNonzeroAndFailedStartAreStructuredResults(t *testing.T) {
	t.Run("nonzero", func(t *testing.T) {
		request := testRequest(t, "exit")
		request.Environment = append(request.Environment, protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_EXIT_CODE", Value: "7"})
		result, err := testRunner().Execute(context.Background(), request)
		if err != nil {
			t.Fatalf("Execute: %v", err)
		}
		if result.Lifecycle != protocol.LifecycleCompleted || result.TerminationReason != protocol.TerminationExitNonzero || result.ExitCode == nil || *result.ExitCode != 7 {
			t.Fatalf("nonzero result = %#v", result)
		}
	})

	t.Run("missing executable", func(t *testing.T) {
		request := testRequest(t, "echo")
		request.Program = filepath.Join(t.TempDir(), "missing-executable")
		result, err := testRunner().Execute(context.Background(), request)
		if err != nil {
			t.Fatalf("Execute: %v", err)
		}
		if result.Lifecycle != protocol.LifecycleFailedToStart || result.TerminationReason != protocol.TerminationStartError || result.ExitCode != nil {
			t.Fatalf("failed-start result = %#v", result)
		}
		if result.Failure == nil || result.Failure.Code != "executable_not_found" {
			t.Fatalf("failed-start detail = %#v", result.Failure)
		}
		assertLifecycle(t, result, protocol.LifecycleAccepted, protocol.LifecycleStarting, protocol.LifecycleFailedToStart)
	})
}

func TestExecuteEnforcesEnvironmentPolicyInChild(t *testing.T) {
	request := testRequest(t, "environment")
	request.Environment = append(request.Environment,
		protocol.EnvironmentVariable{Name: "PATH", Value: "/first"},
		protocol.EnvironmentVariable{Name: "PATH", Value: "/last"},
		protocol.EnvironmentVariable{Name: "GREETING", Value: "hello"},
	)
	runner := testRunner()
	runner.ParentEnvironment = []string{"PATH=/parent", "HOME=/private", "API_TOKEN=parent-secret", "LANG=C"}
	result, err := runner.Execute(context.Background(), request)
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if got, want := result.Stdout.Text, "/last|hello|"+request.RunID+"|<unset>|<unset>"; got != want {
		t.Fatalf("child environment = %q, want %q", got, want)
	}
	encoded, err := json.Marshal(result)
	if err != nil {
		t.Fatalf("marshal result: %v", err)
	}
	if bytes.Contains(encoded, []byte("parent-secret")) || bytes.Contains(encoded, []byte("/private")) {
		t.Fatalf("result leaked filtered environment: %s", encoded)
	}
}

func TestExecuteBoundsLargeIndependentAndSimultaneousStreams(t *testing.T) {
	const large = 2 << 20
	for _, test := range []struct {
		name           string
		stdout, stderr int
	}{
		{name: "large stdout", stdout: large},
		{name: "large stderr", stderr: large},
		{name: "simultaneous", stdout: large, stderr: large},
	} {
		t.Run(test.name, func(t *testing.T) {
			request := testRequest(t, "output")
			request.StdoutLimitBytes = 4096
			request.StderrLimitBytes = 4096
			request.LogLimitBytes = 512
			request.Environment = append(request.Environment,
				protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_STDOUT_BYTES", Value: strconv.Itoa(test.stdout)},
				protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_STDERR_BYTES", Value: strconv.Itoa(test.stderr)},
			)
			result, err := testRunner().Execute(context.Background(), request)
			if err != nil {
				t.Fatalf("Execute: %v", err)
			}
			assertStreamBounds(t, "stdout", result.Stdout, test.stdout, 4096)
			assertStreamBounds(t, "stderr", result.Stderr, test.stderr, 4096)
			if result.OutputObservations.BytesUsed > result.OutputObservations.LimitBytes {
				t.Fatalf("observation log exceeded limit: %#v", result.OutputObservations)
			}
			if test.stdout+test.stderr > 0 && len(result.OutputObservations.Entries) == 0 {
				t.Fatal("output ordering metadata is empty")
			}
		})
	}
}

func TestExecuteRepairsInvalidUTF8(t *testing.T) {
	request := testRequest(t, "output")
	request.Environment = append(request.Environment, protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_INVALID_UTF8", Value: "1"})
	result, err := testRunner().Execute(context.Background(), request)
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if result.Stdout.Text != "\uFFFDx" || result.Stdout.BytesObserved != 2 || result.Stdout.Truncated {
		t.Fatalf("invalid UTF-8 result = %#v", result.Stdout)
	}
	if !json.Valid(mustJSON(t, protocol.Success(result))) {
		t.Fatal("invalid UTF-8 corrupted JSON envelope")
	}
}

func TestExecuteTimeoutCancellationAndEscalation(t *testing.T) {
	t.Run("timeout", func(t *testing.T) {
		request := testRequest(t, "sleep")
		request.TimeoutMilliseconds = 100
		request.TerminationGraceMilliseconds = 500
		result, err := testRunner().Execute(context.Background(), request)
		if err != nil {
			t.Fatalf("Execute: %v", err)
		}
		if result.Lifecycle != protocol.LifecycleTimedOut || result.TerminationReason != protocol.TerminationTimeoutTerminated || !result.Timeout.Triggered || result.Termination.Escalated {
			t.Fatalf("timeout result = %#v", result)
		}
	})

	t.Run("graceful cancellation", func(t *testing.T) {
		request := testRequest(t, "graceful")
		request.TerminationGraceMilliseconds = 2_000
		ready := filepath.Join(t.TempDir(), "ready")
		terminated := filepath.Join(t.TempDir(), "terminated")
		request.Environment = append(request.Environment,
			protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_READY", Value: ready},
			protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_TERMINATED", Value: terminated},
		)
		ctx, cancel := context.WithCancel(WithCancellationSource(context.Background(), "test_cancel"))
		resultChannel := make(chan protocol.ExecutionResult, 1)
		errorChannel := make(chan error, 1)
		go func() {
			result, err := testRunner().Execute(ctx, request)
			resultChannel <- result
			errorChannel <- err
		}()
		waitForFile(t, ready)
		cancel()
		result := <-resultChannel
		if err := <-errorChannel; err != nil {
			t.Fatalf("Execute: %v", err)
		}
		if result.Lifecycle != protocol.LifecycleCancelled || result.TerminationReason != protocol.TerminationCancellationTerminated || result.Cancellation.Source != "test_cancel" || result.Termination.Escalated {
			t.Fatalf("graceful cancellation result = %#v", result)
		}
		waitForFile(t, terminated)
	})

	t.Run("forced cancellation", func(t *testing.T) {
		request := testRequest(t, "ignore")
		ready := filepath.Join(t.TempDir(), "ready")
		request.Environment = append(request.Environment, protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_READY", Value: ready})
		request.TerminationGraceMilliseconds = 50
		ctx, cancel := context.WithCancel(context.Background())
		resultChannel := make(chan protocol.ExecutionResult, 1)
		errorChannel := make(chan error, 1)
		go func() {
			result, err := testRunner().Execute(ctx, request)
			resultChannel <- result
			errorChannel <- err
		}()
		waitForFile(t, ready)
		cancel()
		result := <-resultChannel
		if err := <-errorChannel; err != nil {
			t.Fatalf("Execute: %v", err)
		}
		if result.Lifecycle != protocol.LifecycleKilled || result.TerminationReason != protocol.TerminationCancellationKilled || !result.Termination.Escalated {
			t.Fatalf("forced cancellation result = %#v", result)
		}
	})
}

type blockingStateStore struct {
	delegate *StateStore
	entered  chan struct{}
	release  chan struct{}
	calls    int
}

func (store *blockingStateStore) Save(path string, result protocol.ExecutionResult) error {
	store.calls++
	if store.calls == 3 {
		close(store.entered)
		<-store.release
	}
	return store.delegate.Save(path, result)
}

func TestTimeoutStillTerminatesWhileRunningStatePersistenceIsBlocked(t *testing.T) {
	request := testRequest(t, "ignore")
	request.TimeoutMilliseconds = 50
	request.TerminationGraceMilliseconds = 50
	readyPath := filepath.Join(t.TempDir(), "ready")
	pidPath := filepath.Join(t.TempDir(), "pid")
	request.Environment = append(
		request.Environment,
		protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_READY", Value: readyPath},
		protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_PID", Value: pidPath},
	)
	store := &blockingStateStore{
		delegate: NewStateStore(),
		entered:  make(chan struct{}),
		release:  make(chan struct{}),
	}
	runner := testRunner()
	runner.Store = store
	resultChannel := make(chan protocol.ExecutionResult, 1)
	errorChannel := make(chan error, 1)
	go func() {
		result, err := runner.Execute(context.Background(), request)
		resultChannel <- result
		errorChannel <- err
	}()
	<-store.entered
	waitForFile(t, readyPath)
	pid := waitForPID(t, pidPath)
	waitForProcessExit(t, pid)
	select {
	case <-resultChannel:
		t.Fatal("Execute returned before blocked persistence was released")
	default:
	}
	close(store.release)
	result := <-resultChannel
	if err := <-errorChannel; err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if result.Lifecycle != protocol.LifecycleKilled || result.TerminationReason != protocol.TerminationTimeoutKilled {
		t.Fatalf("blocked-persistence timeout result = %s/%s", result.Lifecycle, result.TerminationReason)
	}
}

func TestExecuteCleansDescendantProcessTree(t *testing.T) {
	request := testRequest(t, "spawn-tree")
	pidPath := filepath.Join(t.TempDir(), "grandchild.pid")
	request.Environment = append(request.Environment, protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_TREE_PID", Value: pidPath})
	request.TerminationGraceMilliseconds = 50
	ctx, cancel := context.WithCancel(context.Background())
	resultChannel := make(chan protocol.ExecutionResult, 1)
	errorChannel := make(chan error, 1)
	go func() {
		result, err := testRunner().Execute(ctx, request)
		resultChannel <- result
		errorChannel <- err
	}()
	grandchildPID := waitForPID(t, pidPath)
	cancel()
	result := <-resultChannel
	if err := <-errorChannel; err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if result.Lifecycle != protocol.LifecycleKilled {
		t.Fatalf("tree cleanup lifecycle = %s", result.Lifecycle)
	}
	if processIsRunning(grandchildPID) {
		t.Fatalf("grandchild %d still runs after Execute returned", grandchildPID)
	}
}

func TestExecuteCleansDescendantsAfterDirectChildExit(t *testing.T) {
	request := testRequest(t, "spawn-tree-exit")
	pidPath := filepath.Join(t.TempDir(), "grandchild.pid")
	request.Environment = append(request.Environment, protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_TREE_PID", Value: pidPath})
	request.TerminationGraceMilliseconds = 50

	result, err := testRunner().Execute(context.Background(), request)
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	grandchildPID := waitForPID(t, pidPath)
	if result.Lifecycle != protocol.LifecycleKilled || result.TerminationReason != protocol.TerminationDescendantsKilled {
		t.Fatalf("descendant cleanup result = %s/%s", result.Lifecycle, result.TerminationReason)
	}
	if !result.Termination.GracefulAttempted || !result.Termination.Escalated {
		t.Fatalf("descendant termination detail = %#v", result.Termination)
	}
	if processIsRunning(grandchildPID) {
		t.Fatalf("grandchild %d still runs after direct child exit", grandchildPID)
	}
}

func TestSupervisorSignalCancelsAndCleansProcessTree(t *testing.T) {
	directory := t.TempDir()
	request := testRequestInDirectory(t, "spawn-tree", directory)
	pidPath := filepath.Join(directory, "grandchild.pid")
	request.Environment = append(request.Environment, protocol.EnvironmentVariable{Name: "SUPERVISOR_HELPER_TREE_PID", Value: pidPath})
	request.TerminationGraceMilliseconds = 50
	requestPath := filepath.Join(directory, "request.json")
	if err := os.WriteFile(requestPath, mustJSON(t, request), 0o600); err != nil {
		t.Fatalf("write wrapper request: %v", err)
	}

	command := exec.Command(testExecutable(t), helperTestArgument)
	command.Env = replaceEnvironment(os.Environ(), "SUPERVISOR_HELPER_MODE", "supervisor-wrapper")
	command.Env = replaceEnvironment(command.Env, "SUPERVISOR_REQUEST_PATH", requestPath)
	var stdout, stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	if err := command.Start(); err != nil {
		t.Fatalf("start wrapper: %v", err)
	}
	grandchildPID := waitForPID(t, pidPath)
	if err := command.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatalf("signal wrapper: %v", err)
	}
	if err := command.Wait(); err != nil {
		t.Fatalf("wrapper exit: %v; stdout=%s stderr=%s", err, stdout.Bytes(), stderr.Bytes())
	}
	if processIsRunning(grandchildPID) {
		t.Fatalf("grandchild %d still runs after supervisor exit", grandchildPID)
	}
	record, err := NewStateStore().Load(request.StatePath)
	if err != nil {
		t.Fatalf("load wrapper run state: %v", err)
	}
	if !record.Execution.Cancellation.Requested || record.Execution.Cancellation.Source != "supervisor_signal" || record.Execution.Lifecycle != protocol.LifecycleKilled {
		t.Fatalf("signal cancellation record = %#v", record.Execution)
	}
}

func runSupervisorWrapper() int {
	file, err := os.Open(os.Getenv("SUPERVISOR_REQUEST_PATH"))
	if err != nil {
		_, _ = fmt.Fprintln(os.Stderr, err)
		return 1
	}
	request, err := protocol.DecodeExecutionRequest(file)
	_ = file.Close()
	if err != nil {
		_, _ = fmt.Fprintln(os.Stderr, err)
		return 1
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	ctx = WithCancellationSource(ctx, "supervisor_signal")
	result, err := NewRunner().Execute(ctx, request)
	if err != nil {
		_, _ = fmt.Fprintln(os.Stderr, err)
		return 1
	}
	if err := json.NewEncoder(os.Stdout).Encode(protocol.Success(result)); err != nil {
		return 1
	}
	return 0
}

func testRequest(t *testing.T, mode string) protocol.ExecutionRequest {
	t.Helper()
	return testRequestInDirectory(t, mode, t.TempDir())
}

func testRequestInDirectory(t *testing.T, mode, directory string) protocol.ExecutionRequest {
	t.Helper()
	return protocol.ExecutionRequest{
		ProtocolVersion:              protocol.ProtocolVersion,
		RunID:                        "test-" + strings.ReplaceAll(t.Name(), "/", "-"),
		Program:                      testExecutable(t),
		Arguments:                    []string{helperTestArgument},
		RedactArgumentIndexes:        []int{},
		WorkingDirectory:             directory,
		Environment:                  []protocol.EnvironmentVariable{{Name: "SUPERVISOR_HELPER_MODE", Value: mode}},
		TimeoutMilliseconds:          0,
		TerminationGraceMilliseconds: 500,
		StdoutLimitBytes:             1 << 20,
		StderrLimitBytes:             1 << 20,
		LogLimitBytes:                64 << 10,
		StatePath:                    filepath.Join(directory, "relay-runs", "state.json"),
	}
}

func testRunner() *Runner {
	runner := NewRunner()
	runner.ParentEnvironment = []string{"PATH=/usr/bin:/bin", "LANG=C", "HOME=/filtered", "API_TOKEN=filtered-secret"}
	return runner
}

func testExecutable(t *testing.T) string {
	t.Helper()
	executable, err := filepath.Abs(os.Args[0])
	if err != nil {
		t.Fatalf("absolute test executable: %v", err)
	}
	return executable
}

func argumentsAfterDoubleDash() []string {
	for index, argument := range os.Args {
		if argument == "--" {
			return os.Args[index+1:]
		}
	}
	return []string{}
}

func writeHelperFile(environmentName, contents string) {
	if path := os.Getenv(environmentName); path != "" {
		_ = os.WriteFile(path, []byte(contents), 0o600)
	}
}

func writeRepeated(writer io.Writer, value byte, count int) {
	chunk := bytes.Repeat([]byte{value}, 32<<10)
	for count > 0 {
		write := len(chunk)
		if write > count {
			write = count
		}
		_, _ = writer.Write(chunk[:write])
		count -= write
	}
}

func valueOrUnset(name string) string {
	if value, ok := os.LookupEnv(name); ok {
		return value
	}
	return "<unset>"
}

func replaceEnvironment(environment []string, name, value string) []string {
	prefix := name + "="
	updated := make([]string, 0, len(environment)+1)
	for _, entry := range environment {
		if !strings.HasPrefix(entry, prefix) {
			updated = append(updated, entry)
		}
	}
	return append(updated, prefix+value)
}

func assertLifecycle(t *testing.T, result protocol.ExecutionResult, want ...protocol.Lifecycle) {
	t.Helper()
	got := make([]protocol.Lifecycle, len(result.LifecycleHistory))
	for index, event := range result.LifecycleHistory {
		got[index] = event.Lifecycle
		if index > 0 && event.Sequence <= result.LifecycleHistory[index-1].Sequence {
			t.Fatalf("lifecycle sequence is not increasing: %#v", result.LifecycleHistory)
		}
	}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("lifecycle history = %v, want %v", got, want)
	}
}

func assertSecretAbsent(t *testing.T, statePath string, result protocol.ExecutionResult, secret string) {
	t.Helper()
	resultJSON := mustJSON(t, result)
	if bytes.Contains(resultJSON, []byte(secret)) {
		t.Fatalf("result leaked secret: %s", resultJSON)
	}
	state, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatalf("read run state: %v", err)
	}
	if bytes.Contains(state, []byte(secret)) {
		t.Fatalf("persisted state leaked secret: %s", state)
	}
}

func assertStreamBounds(t *testing.T, name string, stream protocol.StreamResult, observed, limit int) {
	t.Helper()
	wantRetained := observed
	if wantRetained > limit {
		wantRetained = limit
	}
	if stream.BytesObserved != int64(observed) || stream.BytesRetained != int64(wantRetained) || stream.Truncated != (observed > limit) {
		t.Fatalf("%s metadata = %#v, observed=%d limit=%d", name, stream, observed, limit)
	}
}

func mustJSON(t *testing.T, value any) []byte {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatalf("marshal JSON: %v", err)
	}
	return encoded
}

func waitForFile(t *testing.T, path string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for file %s", path)
}

func waitForPID(t *testing.T, path string) int {
	t.Helper()
	waitForFile(t, path)
	payload, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read PID file: %v", err)
	}
	pid, err := strconv.Atoi(string(payload))
	if err != nil {
		t.Fatalf("parse PID %q: %v", payload, err)
	}
	return pid
}

func waitForProcessExit(t *testing.T, pid int) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if !processIsRunning(pid) {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("process %d remains running", pid)
}

func processIsRunning(pid int) bool {
	err := syscall.Kill(pid, 0)
	if err == syscall.ESRCH {
		return false
	}
	if payload, readErr := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid)); readErr == nil {
		fields := strings.Fields(string(payload))
		if len(fields) > 2 && fields[2] == "Z" {
			return false
		}
	}
	return true
}
