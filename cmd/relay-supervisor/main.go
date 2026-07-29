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

	"relay/protocol"
	"relay/supervisor"
)

const maximumRequestBytes int64 = 1 << 20

type executor interface {
	Execute(context.Context, protocol.ExecutionRequest) (protocol.ExecutionResult, error)
}

func main() {
	os.Exit(run(context.Background(), os.Stdin, os.Stdout, os.Stderr, supervisor.NewRunner()))
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
	executionContext, stop := signal.NotifyContext(ctx, terminationSignals()...)
	defer stop()
	executionContext = supervisor.WithCancellationSource(executionContext, "supervisor_signal")
	result, err := runner.Execute(executionContext, request)
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
