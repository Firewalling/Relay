package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"strconv"

	"relay/protocol"
	"relay/supervisor"
)

const maximumRequestBytes int64 = 1 << 20

const (
	supervisorReadyEnvironment    = "RELAY_SUPERVISOR_READY_FD"
	supervisorReadyByte           = byte('R')
	supervisorAcknowledgementByte = byte('A')
)

type executor interface {
	Execute(context.Context, protocol.ExecutionRequest) (protocol.ExecutionResult, error)
}

func main() {
	executionContext, stop := signal.NotifyContext(context.Background(), terminationSignals()...)
	executionContext = supervisor.WithCancellationSource(executionContext, "supervisor_signal")
	if err := emitSupervisorReady(); err != nil {
		exitCode := emitError(os.Stderr, "protocol_failure", fmt.Sprintf("supervisor readiness failed: %v", err))
		stop()
		os.Exit(exitCode)
	}
	exitCode := run(executionContext, os.Stdin, os.Stdout, os.Stderr, supervisor.NewRunner())
	stop()
	os.Exit(exitCode)
}

func emitSupervisorReady() (resultErr error) {
	value, configured := os.LookupEnv(supervisorReadyEnvironment)
	if !configured {
		return nil
	}
	if value == "" {
		return fmt.Errorf("%s must be a decimal file descriptor", supervisorReadyEnvironment)
	}
	for _, character := range value {
		if character < '0' || character > '9' {
			return fmt.Errorf("%s must be a decimal file descriptor", supervisorReadyEnvironment)
		}
	}
	descriptor, err := strconv.ParseUint(value, 10, 0)
	if err != nil || descriptor < 3 {
		return fmt.Errorf("%s must name an inherited file descriptor of 3 or greater", supervisorReadyEnvironment)
	}
	readySocket := os.NewFile(uintptr(descriptor), "relay-supervisor-ready")
	if readySocket == nil {
		return fmt.Errorf("%s names an invalid file descriptor", supervisorReadyEnvironment)
	}
	defer func() {
		if closeErr := readySocket.Close(); resultErr == nil && closeErr != nil {
			resultErr = fmt.Errorf("close readiness file descriptor: %w", closeErr)
		}
	}()

	written, err := readySocket.Write([]byte{supervisorReadyByte})
	if err != nil {
		return fmt.Errorf("write readiness byte: %w", err)
	}
	if written != 1 {
		return fmt.Errorf("write readiness byte: wrote %d bytes", written)
	}

	var acknowledgement [1]byte
	if _, err := io.ReadFull(readySocket, acknowledgement[:]); err != nil {
		return fmt.Errorf("read readiness acknowledgement: %w", err)
	}
	if acknowledgement[0] != supervisorAcknowledgementByte {
		return fmt.Errorf("readiness acknowledgement is %q, want %q", acknowledgement[0], supervisorAcknowledgementByte)
	}
	var trailing [1]byte
	if _, err := io.ReadFull(readySocket, trailing[:]); err == nil {
		return fmt.Errorf("unexpected trailing readiness byte %q", trailing[0])
	} else if !errors.Is(err, io.EOF) {
		return fmt.Errorf("finish readiness acknowledgement: %w", err)
	}
	return nil
}

func run(ctx context.Context, stdin io.Reader, stdout, stderr io.Writer, runner executor) int {
	if _, err := protocol.CanonicalManifest(); err != nil {
		return emitError(stderr, "protocol_failure", fmt.Sprintf("invalid embedded Relay protocol manifest: %v", err))
	}
	payload, err := io.ReadAll(io.LimitReader(stdin, maximumRequestBytes+1))
	if err != nil {
		return emitError(stderr, "protocol_failure", "could not read execution request")
	}
	if int64(len(payload)) > maximumRequestBytes {
		return emitError(stderr, "protocol_failure", fmt.Sprintf("execution request exceeds %d bytes", maximumRequestBytes))
	}
	request, err := protocol.DecodeExecutionRequest(bytes.NewReader(payload))
	if err != nil {
		var unsupported protocol.UnsupportedProtocolVersionError
		if errors.As(err, &unsupported) {
			return emitError(stderr, "unsupported_protocol_version", err.Error())
		}
		return emitError(stderr, "protocol_failure", err.Error())
	}
	result, err := runner.Execute(ctx, request)
	if err != nil {
		return emitError(stderr, "protocol_failure", err.Error())
	}
	if err := writeJSON(stdout, protocol.Success(result)); err != nil && !isBrokenPipe(err) {
		return 1
	}
	return 0
}

func emitError(stderr io.Writer, code, message string) int {
	if err := writeJSON(stderr, protocol.Failure(code, message)); err != nil && !isBrokenPipe(err) {
		return 1
	}
	return 1
}

func writeJSON(writer io.Writer, value any) error {
	prepareOutputWrite()
	payload, err := json.Marshal(value)
	if err != nil {
		return err
	}
	payload = append(payload, '\n')
	_, err = writer.Write(payload)
	return err
}
