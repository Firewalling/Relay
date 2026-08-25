//go:build linux

package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
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

func TestSupervisorMainWaitsForAcknowledgementBeforeProcessingRequest(t *testing.T) {
	directory := t.TempDir()
	childMarker := filepath.Join(directory, "child-started")
	request := protocol.ExecutionRequest{
		ProtocolVersion:              protocol.ProtocolVersion,
		RunID:                        "ready-before-signal",
		Program:                      "/usr/bin/touch",
		Arguments:                    []string{childMarker},
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

	socketDescriptors, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		t.Fatalf("create readiness socketpair: %v", err)
	}
	launcherSocket := os.NewFile(uintptr(socketDescriptors[0]), "launcher-readiness")
	supervisorSocket := os.NewFile(uintptr(socketDescriptors[1]), "supervisor-readiness")
	defer launcherSocket.Close()

	command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
	command.Env = append(os.Environ(),
		supervisorMainHelperEnvironment+"=1",
		supervisorReadyEnvironment+"=3",
	)
	command.ExtraFiles = []*os.File{supervisorSocket}
	command.Stdin = bytes.NewReader(payload)
	var stdout, stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	if err := command.Start(); err != nil {
		_ = supervisorSocket.Close()
		t.Fatalf("start helper: %v", err)
	}
	defer func() {
		if command.ProcessState == nil {
			_ = command.Process.Kill()
			_ = command.Wait()
		}
	}()
	if err := supervisorSocket.Close(); err != nil {
		t.Fatalf("close parent supervisor socket: %v", err)
	}
	waitDone := make(chan error, 1)
	go func() { waitDone <- command.Wait() }()

	var readiness [1]byte
	if _, err := io.ReadFull(launcherSocket, readiness[:]); err != nil {
		t.Fatalf("read readiness: %v", err)
	}
	if readiness[0] != supervisorReadyByte {
		t.Fatalf("readiness byte = %q, want %q", readiness[0], supervisorReadyByte)
	}

	select {
	case err := <-waitDone:
		t.Fatalf("helper exited before acknowledgement: %v; stdout=%q stderr=%q", err, stdout.String(), stderr.String())
	case <-time.After(100 * time.Millisecond):
	}
	if _, err := os.Stat(request.StatePath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("request was processed before acknowledgement; state stat error = %v", err)
	}
	if _, err := os.Stat(childMarker); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("child started before acknowledgement; marker stat error = %v", err)
	}

	if err := command.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatalf("signal helper: %v", err)
	}
	time.Sleep(100 * time.Millisecond)
	if _, err := os.Stat(request.StatePath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("request was processed before post-signal acknowledgement; state stat error = %v", err)
	}
	if _, err := launcherSocket.Write([]byte{supervisorAcknowledgementByte}); err != nil {
		t.Fatalf("write readiness acknowledgement: %v", err)
	}
	if err := launcherSocket.Close(); err != nil {
		t.Fatalf("close launcher readiness socket: %v", err)
	}

	select {
	case err := <-waitDone:
		if err != nil {
			t.Fatalf("helper exit = %v, stdout=%q stderr=%q", err, stdout.String(), stderr.String())
		}
	case <-time.After(2 * time.Second):
		_ = command.Process.Kill()
		<-waitDone
		t.Fatal("helper did not consume SIGTERM after acknowledgement")
	}
	if stderr.Len() != 0 {
		t.Fatalf("helper stderr = %q", stderr.String())
	}
	var envelope protocol.SuccessEnvelope[protocol.ExecutionResult]
	if err := json.Unmarshal(stdout.Bytes(), &envelope); err != nil {
		t.Fatalf("decode success envelope: %v; stdout=%q", err, stdout.String())
	}
	if !envelope.OK || envelope.Command != protocol.SupervisorCommand {
		t.Fatalf("success envelope = %#v", envelope)
	}
	if envelope.Data.Lifecycle != protocol.LifecycleCancelled ||
		!envelope.Data.Cancellation.Requested ||
		envelope.Data.Cancellation.Source != "supervisor_signal" {
		t.Fatalf("early-signal result = %#v", envelope.Data)
	}
	if envelope.Data.StartedAt != envelope.Data.AcceptedAt ||
		len(envelope.Data.LifecycleHistory) != 2 ||
		envelope.Data.LifecycleHistory[0].Lifecycle != protocol.LifecycleAccepted ||
		envelope.Data.LifecycleHistory[1].Lifecycle != protocol.LifecycleCancelled {
		t.Fatalf("early-signal lifecycle history = %#v", envelope.Data.LifecycleHistory)
	}
	if _, err := os.Stat(childMarker); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("cancelled request started child; marker stat error = %v", err)
	}
	recordPayload, err := os.ReadFile(request.StatePath)
	if err != nil {
		t.Fatalf("read persisted cancellation: %v", err)
	}
	var record protocol.RunRecord
	if err := json.Unmarshal(recordPayload, &record); err != nil {
		t.Fatalf("decode persisted cancellation: %v", err)
	}
	if record.Execution.Lifecycle != protocol.LifecycleCancelled ||
		record.Execution.Cancellation.Source != "supervisor_signal" {
		t.Fatalf("persisted early-signal result = %#v", record.Execution)
	}
}

func TestSupervisorMainFailsClosedForInvalidReadinessConfiguration(t *testing.T) {
	tests := []struct {
		name           string
		descriptor     string
		unwritableFile bool
	}{
		{name: "malformed", descriptor: "not-a-descriptor"},
		{name: "standard stream", descriptor: "2"},
		{name: "unavailable", descriptor: "3"},
		{name: "write failure", descriptor: "3", unwritableFile: true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
			command.Env = append(os.Environ(),
				supervisorMainHelperEnvironment+"=1",
				supervisorReadyEnvironment+"="+test.descriptor,
			)
			if test.unwritableFile {
				readOnly, err := os.Open(os.DevNull)
				if err != nil {
					t.Fatalf("open read-only readiness descriptor: %v", err)
				}
				defer readOnly.Close()
				command.ExtraFiles = []*os.File{readOnly}
			}
			var stdout, stderr bytes.Buffer
			command.Stdout = &stdout
			command.Stderr = &stderr
			err := command.Run()
			if err == nil {
				t.Fatalf("helper unexpectedly succeeded; stdout=%q stderr=%q", stdout.String(), stderr.String())
			}
			var exitError *exec.ExitError
			if !errors.As(err, &exitError) {
				t.Fatalf("helper error = %T %v", err, err)
			}
			if stdout.Len() != 0 {
				t.Fatalf("helper stdout = %q", stdout.String())
			}
			if bytes.Count(stderr.Bytes(), []byte("\n")) != 1 {
				t.Fatalf("helper stderr framing = %q", stderr.String())
			}
			var envelope protocol.ErrorEnvelope
			if err := json.Unmarshal(stderr.Bytes(), &envelope); err != nil {
				t.Fatalf("decode error envelope: %v; stderr=%q", err, stderr.String())
			}
			if envelope.OK ||
				envelope.Command != protocol.SupervisorCommand ||
				envelope.Error.Code != "protocol_failure" ||
				!strings.Contains(envelope.Error.Message, "supervisor readiness failed") {
				t.Fatalf("readiness error envelope = %#v", envelope)
			}
		})
	}
}

func TestSupervisorMainFailsClosedForInvalidReadinessAcknowledgement(t *testing.T) {
	tests := []struct {
		name            string
		acknowledgement []byte
	}{
		{name: "EOF before acknowledgement"},
		{name: "wrong acknowledgement", acknowledgement: []byte{'X'}},
		{name: "trailing data", acknowledgement: []byte{supervisorAcknowledgementByte, 'X'}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			directory := t.TempDir()
			childMarker := filepath.Join(directory, "child-started")
			request := protocol.ExecutionRequest{
				ProtocolVersion:              protocol.ProtocolVersion,
				RunID:                        "invalid-readiness-acknowledgement",
				Program:                      "/usr/bin/touch",
				Arguments:                    []string{childMarker},
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

			socketDescriptors, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
			if err != nil {
				t.Fatalf("create readiness socketpair: %v", err)
			}
			launcherSocket := os.NewFile(uintptr(socketDescriptors[0]), "launcher-readiness")
			supervisorSocket := os.NewFile(uintptr(socketDescriptors[1]), "supervisor-readiness")
			command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
			command.Env = append(os.Environ(),
				supervisorMainHelperEnvironment+"=1",
				supervisorReadyEnvironment+"=3",
			)
			command.ExtraFiles = []*os.File{supervisorSocket}
			command.Stdin = bytes.NewReader(append(payload, '\n'))
			var stdout, stderr bytes.Buffer
			command.Stdout = &stdout
			command.Stderr = &stderr
			if err := command.Start(); err != nil {
				_ = launcherSocket.Close()
				_ = supervisorSocket.Close()
				t.Fatalf("start helper: %v", err)
			}
			defer launcherSocket.Close()
			defer func() {
				if command.ProcessState == nil {
					_ = command.Process.Kill()
					_ = command.Wait()
				}
			}()
			if err := supervisorSocket.Close(); err != nil {
				_ = launcherSocket.Close()
				_ = command.Process.Kill()
				_ = command.Wait()
				t.Fatalf("close parent supervisor socket: %v", err)
			}

			var readiness [1]byte
			if _, err := io.ReadFull(launcherSocket, readiness[:]); err != nil {
				_ = launcherSocket.Close()
				_ = command.Process.Kill()
				_ = command.Wait()
				t.Fatalf("read readiness: %v", err)
			}
			if readiness[0] != supervisorReadyByte {
				t.Fatalf("readiness byte = %q, want %q", readiness[0], supervisorReadyByte)
			}
			if len(test.acknowledgement) > 0 {
				if _, err := launcherSocket.Write(test.acknowledgement); err != nil {
					t.Fatalf("write invalid acknowledgement: %v", err)
				}
			}
			if err := launcherSocket.Close(); err != nil {
				t.Fatalf("close launcher readiness socket: %v", err)
			}

			err = command.Wait()
			if err == nil {
				t.Fatalf("helper unexpectedly succeeded; stdout=%q stderr=%q", stdout.String(), stderr.String())
			}
			var exitError *exec.ExitError
			if !errors.As(err, &exitError) {
				t.Fatalf("helper error = %T %v", err, err)
			}
			if stdout.Len() != 0 {
				t.Fatalf("helper stdout = %q", stdout.String())
			}
			var envelope protocol.ErrorEnvelope
			if err := json.Unmarshal(stderr.Bytes(), &envelope); err != nil {
				t.Fatalf("decode error envelope: %v; stderr=%q", err, stderr.String())
			}
			if envelope.OK ||
				envelope.Command != protocol.SupervisorCommand ||
				envelope.Error.Code != "protocol_failure" ||
				!strings.Contains(envelope.Error.Message, "supervisor readiness failed") {
				t.Fatalf("readiness error envelope = %#v", envelope)
			}
			if _, err := os.Stat(request.StatePath); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("invalid acknowledgement processed request; state stat error = %v", err)
			}
			if _, err := os.Stat(childMarker); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("invalid acknowledgement started child; marker stat error = %v", err)
			}
		})
	}
}

func TestSupervisorMainAllowsDirectInvocationWithoutReadinessDescriptor(t *testing.T) {
	directory := t.TempDir()
	request := protocol.ExecutionRequest{
		ProtocolVersion:              protocol.ProtocolVersion,
		RunID:                        "direct-invocation",
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

	command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
	command.Env = []string{supervisorMainHelperEnvironment + "=1"}
	command.Stdin = bytes.NewReader(payload)
	var stdout, stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	if err := command.Run(); err != nil {
		t.Fatalf("direct helper exit = %v; stdout=%q stderr=%q", err, stdout.String(), stderr.String())
	}
	if stderr.Len() != 0 {
		t.Fatalf("direct helper stderr = %q", stderr.String())
	}
	var envelope protocol.SuccessEnvelope[protocol.ExecutionResult]
	if err := json.Unmarshal(stdout.Bytes(), &envelope); err != nil {
		t.Fatalf("decode direct success envelope: %v; stdout=%q", err, stdout.String())
	}
	if !envelope.OK ||
		envelope.Command != protocol.SupervisorCommand ||
		envelope.Data.Lifecycle != protocol.LifecycleCompleted {
		t.Fatalf("direct success envelope = %#v", envelope)
	}
}

func TestSupervisorMainDirectInvocationExitsOnSignalWhileRequestIsIncomplete(t *testing.T) {
	directory := t.TempDir()
	childMarker := filepath.Join(directory, "child-started")
	statePath := filepath.Join(directory, "state.json")
	request := protocol.ExecutionRequest{
		ProtocolVersion:              protocol.ProtocolVersion,
		RunID:                        "direct-incomplete-request",
		Program:                      "/usr/bin/touch",
		Arguments:                    []string{childMarker, strings.Repeat("x", 256<<10)},
		RedactArgumentIndexes:        []int{},
		WorkingDirectory:             directory,
		Environment:                  []protocol.EnvironmentVariable{},
		TerminationGraceMilliseconds: 100,
		StdoutLimitBytes:             1024,
		StderrLimitBytes:             1024,
		LogLimitBytes:                1024,
		StatePath:                    statePath,
	}
	payload, err := json.Marshal(request)
	if err != nil {
		t.Fatalf("marshal request: %v", err)
	}
	payload = payload[:len(payload)-1]

	socketDescriptors, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		t.Fatalf("create stdin socketpair: %v", err)
	}
	if err := syscall.SetsockoptInt(socketDescriptors[0], syscall.SOL_SOCKET, syscall.SO_SNDBUF, 1024); err != nil {
		t.Fatalf("limit stdin socket send buffer: %v", err)
	}
	launcherInput := os.NewFile(uintptr(socketDescriptors[0]), "launcher-stdin")
	supervisorInput := os.NewFile(uintptr(socketDescriptors[1]), "supervisor-stdin")
	defer launcherInput.Close()

	command := exec.Command(os.Args[0], "-test.run=^TestSupervisorMainHelper$")
	command.Env = []string{supervisorMainHelperEnvironment + "=1"}
	command.Stdin = supervisorInput
	var stdout, stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	if err := command.Start(); err != nil {
		_ = supervisorInput.Close()
		t.Fatalf("start helper: %v", err)
	}
	defer func() {
		if command.ProcessState == nil {
			_ = command.Process.Kill()
			_ = command.Wait()
		}
	}()
	if err := supervisorInput.Close(); err != nil {
		t.Fatalf("close parent supervisor stdin: %v", err)
	}

	writeDone := make(chan error, 1)
	go func() {
		_, err := launcherInput.Write(payload)
		writeDone <- err
	}()
	select {
	case err := <-writeDone:
		if err != nil {
			t.Fatalf("write incomplete request: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("helper did not begin reading the incomplete request")
	}

	if err := command.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatalf("signal helper: %v", err)
	}
	waitDone := make(chan error, 1)
	go func() { waitDone <- command.Wait() }()
	select {
	case err := <-waitDone:
		var exitError *exec.ExitError
		if !errors.As(err, &exitError) {
			t.Fatalf("helper exit = %v; stdout=%q stderr=%q", err, stdout.String(), stderr.String())
		}
		status, ok := exitError.ProcessState.Sys().(syscall.WaitStatus)
		if !ok || !status.Signaled() || status.Signal() != syscall.SIGTERM {
			t.Fatalf("helper exit status = %v, want SIGTERM; stdout=%q stderr=%q", status, stdout.String(), stderr.String())
		}
	case <-time.After(2 * time.Second):
		_ = command.Process.Kill()
		<-waitDone
		t.Fatal("direct helper did not exit after SIGTERM")
	}
	if stdout.Len() != 0 {
		t.Fatalf("helper stdout = %q", stdout.String())
	}
	if stderr.Len() != 0 {
		t.Fatalf("helper stderr = %q", stderr.String())
	}
	if _, err := os.Stat(statePath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("incomplete request created execution state; state stat error = %v", err)
	}
	if _, err := os.Stat(childMarker); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("incomplete request started child; marker stat error = %v", err)
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
