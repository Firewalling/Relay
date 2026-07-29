package supervisor

import (
	"bytes"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"relay/protocol"
)

func TestLimitedCaptureTruncatesAndRepairsInvalidUTF8(t *testing.T) {
	recorder := newObservationRecorder(1024)
	capture := newLimitedCapture("stdout", 2, recorder)
	if err := capture.consume(bytes.NewReader([]byte{'a', 0xff, 'b'})); err != nil {
		t.Fatalf("consume: %v", err)
	}
	result := capture.result()
	if result.Text != "a\uFFFD" {
		t.Fatalf("text = %q, want repaired UTF-8", result.Text)
	}
	if result.BytesObserved != 3 || result.BytesRetained != 2 || !result.Truncated {
		t.Fatalf("stream metadata = %#v", result)
	}
	if result.FirstSequence == nil || result.LastSequence == nil || *result.FirstSequence != *result.LastSequence {
		t.Fatalf("sequence metadata = first %v last %v", result.FirstSequence, result.LastSequence)
	}
	if log := recorder.snapshot(); log.Truncated || len(log.Entries) != 1 {
		t.Fatalf("observation log = %#v, want one retained entry", log)
	}
}

func TestObservationLogIsBoundedWhileCaptureKeepsDraining(t *testing.T) {
	recorder := newObservationRecorder(1)
	capture := newLimitedCapture("stderr", 4, recorder)
	reader := &oneByteReader{data: []byte("abcdefghij")}
	if err := capture.consume(reader); err != nil {
		t.Fatalf("consume: %v", err)
	}
	result := capture.result()
	if result.Text != "abcd" || result.BytesObserved != 10 || result.BytesRetained != 4 || !result.Truncated {
		t.Fatalf("stream result = %#v", result)
	}
	log := recorder.snapshot()
	if !log.Truncated || log.BytesUsed > log.LimitBytes || len(log.Entries) != 0 {
		t.Fatalf("bounded observation log = %#v", log)
	}
	if result.FirstSequence == nil || result.LastSequence == nil || *result.FirstSequence >= *result.LastSequence {
		t.Fatalf("missing ordering range: %#v", result)
	}
}

func TestLimitedCapturePropagatesReadErrors(t *testing.T) {
	recorder := newObservationRecorder(1024)
	capture := newLimitedCapture("stdout", 32, recorder)
	want := io.ErrUnexpectedEOF
	err := capture.consume(errorReader{err: want})
	if err != want {
		t.Fatalf("consume error = %v, want %v", err, want)
	}
}

type oneByteReader struct {
	data []byte
}

func (reader *oneByteReader) Read(buffer []byte) (int, error) {
	if len(reader.data) == 0 {
		return 0, io.EOF
	}
	buffer[0] = reader.data[0]
	reader.data = reader.data[1:]
	return 1, nil
}

type errorReader struct {
	err error
}

func (reader errorReader) Read([]byte) (int, error) {
	return 0, reader.err
}

func TestObservationCostIncludesVariableFields(t *testing.T) {
	short := observationCost(protocol.OutputObservation{Sequence: 1, Stream: "stdout", Bytes: 1})
	long := observationCost(protocol.OutputObservation{Sequence: 1000, Stream: strings.Repeat("x", 10), Bytes: 1000})
	if long <= short {
		t.Fatalf("long observation cost = %d, short = %d", long, short)
	}
}

func TestTruncatedOutputFixtureUsesProducerObservationAccounting(t *testing.T) {
	payload, err := os.ReadFile(filepath.Join("..", "protocol", "fixtures", "supervisor-v1", "response-truncated-output.json"))
	if err != nil {
		t.Fatalf("read fixture: %v", err)
	}
	var envelope protocol.SuccessEnvelope[protocol.ExecutionResult]
	if err := json.Unmarshal(payload, &envelope); err != nil {
		t.Fatalf("decode fixture: %v", err)
	}
	var bytesUsed int64
	for _, entry := range envelope.Data.OutputObservations.Entries {
		bytesUsed += observationCost(entry)
	}
	if bytesUsed != envelope.Data.OutputObservations.BytesUsed {
		t.Fatalf("fixture bytes_used = %d, producer accounting = %d", envelope.Data.OutputObservations.BytesUsed, bytesUsed)
	}
}
