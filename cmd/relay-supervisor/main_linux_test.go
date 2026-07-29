//go:build linux

package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	"relay/protocol"
)

const supervisorMainHelperEnvironment = "RELAY_TEST_SUPERVISOR_MAIN"

func TestSupervisorMainHelper(t *testing.T) {
	if os.Getenv(supervisorMainHelperEnvironment) != "1" {
		return
	}
	signal.Reset(syscall.SIGPIPE)
	main()
}

func TestSupervisorMainTerminatesWhileRequestStdinRemainsOpen(t *testing.T) {
	command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
	command.Env = append(os.Environ(), supervisorMainHelperEnvironment+"=1")
	stdin, err := command.StdinPipe()
	if err != nil {
		t.Fatalf("StdinPipe: %v", err)
	}
	defer stdin.Close()
	var stdout, stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	if err := command.Start(); err != nil {
		t.Fatalf("start helper: %v", err)
	}
	// The old implementation installed handlers before reading the request and
	// consumed SIGTERM forever while this pipe remained open.
	time.Sleep(300 * time.Millisecond)
	if err := command.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatalf("signal helper: %v", err)
	}
	waitDone := make(chan error, 1)
	go func() { waitDone <- command.Wait() }()
	select {
	case err := <-waitDone:
		var exitError *exec.ExitError
		if !errors.As(err, &exitError) || exitError.ProcessState.Sys().(syscall.WaitStatus).Signal() != syscall.SIGTERM {
			t.Fatalf("helper exit = %v, stdout=%q stderr=%q", err, stdout.String(), stderr.String())
		}
	case <-time.After(2 * time.Second):
		_ = command.Process.Kill()
		<-waitDone
		t.Fatal("helper remained blocked on open request stdin after SIGTERM")
	}
}

func TestSupervisorMainHandlesClosedStdoutWithoutSIGPIPEExit(t *testing.T) {
	directory := t.TempDir()
	request := protocol.ExecutionRequest{
		ProtocolVersion:              protocol.ProtocolVersion,
		RunID:                        "closed-stdout",
		Program:                      "/usr/bin/true",
		Arguments:                    []string{},
		RedactArgumentIndexes:        []int{},
		WorkingDirectory:             directory,
		Environment:                  []protocol.EnvironmentVariable{},
		TerminationGraceMilliseconds: 100,
		StdoutLimitBytes:             1024,
		StderrLimitBytes:             1024,
		LogLimitBytes:                1024,
		StatePath:                    filepath.Join(directory, "state.json"),
	}
	payload, err := json.Marshal(request)
	if err != nil {
		t.Fatalf("marshal request: %v", err)
	}
	payload = append(payload, '\n')

	readEnd, writeEnd, err := os.Pipe()
	if err != nil {
		t.Fatalf("os.Pipe: %v", err)
	}
	if err := readEnd.Close(); err != nil {
		t.Fatalf("close read end: %v", err)
	}
	command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
	command.Env = append(os.Environ(), supervisorMainHelperEnvironment+"=1")
	command.Stdin = bytes.NewReader(payload)
	command.Stdout = writeEnd
	var stderr bytes.Buffer
	command.Stderr = &stderr
	if err := command.Start(); err != nil {
		_ = writeEnd.Close()
		t.Fatalf("start helper: %v", err)
	}
	_ = writeEnd.Close()
	if err := command.Wait(); err != nil {
		t.Fatalf("closed-stdout helper exit: %v; stderr=%q", err, stderr.String())
	}
	if stderr.Len() != 0 {
		t.Fatalf("closed-stdout helper stderr = %q", stderr.String())
	}
}
