package supervisor

import (
	"errors"
	"io"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"relay/protocol"
)

const outputReadBufferBytes = 32 << 10

type observationRecorder struct {
	mu        sync.Mutex
	next      uint64
	limit     int64
	bytesUsed int64
	truncated bool
	entries   []protocol.OutputObservation
}

func newObservationRecorder(limit int64) *observationRecorder {
	return &observationRecorder{limit: limit, entries: []protocol.OutputObservation{}}
}

func (recorder *observationRecorder) lifecycle(state protocol.Lifecycle, timestamp time.Time) protocol.LifecycleEvent {
	recorder.mu.Lock()
	defer recorder.mu.Unlock()
	recorder.next++
	return protocol.LifecycleEvent{
		Sequence:  recorder.next,
		Lifecycle: state,
		Timestamp: formatTimestamp(timestamp),
	}
}

func (recorder *observationRecorder) output(stream string, bytesRead int) uint64 {
	recorder.mu.Lock()
	defer recorder.mu.Unlock()
	recorder.next++
	sequence := recorder.next
	entry := protocol.OutputObservation{Sequence: sequence, Stream: stream, Bytes: bytesRead}
	cost := observationCost(entry)
	if recorder.bytesUsed+cost <= recorder.limit {
		recorder.entries = append(recorder.entries, entry)
		recorder.bytesUsed += cost
	} else {
		recorder.truncated = true
	}
	return sequence
}

func (recorder *observationRecorder) snapshot() protocol.ObservationLog {
	recorder.mu.Lock()
	defer recorder.mu.Unlock()
	entries := append([]protocol.OutputObservation(nil), recorder.entries...)
	return protocol.ObservationLog{
		LimitBytes: recorder.limit,
		BytesUsed:  recorder.bytesUsed,
		Truncated:  recorder.truncated,
		Entries:    entries,
	}
}

func observationCost(entry protocol.OutputObservation) int64 {
	// This conservative accounting includes fixed JSON keys/delimiters plus the
	// decimal and stream payloads. It bounds retained metadata without allocating
	// a temporary encoding for every pipe read.
	return int64(48 + len(entry.Stream) + len(strconv.FormatUint(entry.Sequence, 10)) + len(strconv.Itoa(entry.Bytes)))
}

type limitedCapture struct {
	stream   string
	limit    int64
	recorder *observationRecorder
	data     []byte
	observed int64
	first    *uint64
	last     *uint64
}

func newLimitedCapture(stream string, limit int64, recorder *observationRecorder) *limitedCapture {
	capacity := limit
	if capacity > outputReadBufferBytes {
		capacity = outputReadBufferBytes
	}
	return &limitedCapture{
		stream:   stream,
		limit:    limit,
		recorder: recorder,
		data:     make([]byte, 0, int(capacity)),
	}
}

func (capture *limitedCapture) Write(data []byte) (int, error) {
	if len(data) > 0 {
		capture.record(data)
	}
	return len(data), nil
}

func (capture *limitedCapture) record(data []byte) {
	sequence := capture.recorder.output(capture.stream, len(data))
	if capture.first == nil {
		first := sequence
		capture.first = &first
	}
	last := sequence
	capture.last = &last
	capture.observed += int64(len(data))
	remaining := capture.limit - int64(len(capture.data))
	if remaining <= 0 {
		return
	}
	retained := int64(len(data))
	if retained > remaining {
		retained = remaining
	}
	capture.data = append(capture.data, data[:int(retained)]...)
}

func (capture *limitedCapture) consume(reader io.Reader) error {
	buffer := make([]byte, outputReadBufferBytes)
	for {
		count, err := reader.Read(buffer)
		if count > 0 {
			capture.record(buffer[:count])
		}
		if err != nil {
			if errors.Is(err, io.EOF) || errors.Is(err, os.ErrClosed) {
				return nil
			}
			return err
		}
	}
}

func (capture *limitedCapture) result() protocol.StreamResult {
	return protocol.StreamResult{
		Text:          strings.ToValidUTF8(string(capture.data), "\uFFFD"),
		BytesObserved: capture.observed,
		BytesRetained: int64(len(capture.data)),
		Truncated:     capture.observed > int64(len(capture.data)),
		FirstSequence: cloneSequence(capture.first),
		LastSequence:  cloneSequence(capture.last),
	}
}

func cloneSequence(sequence *uint64) *uint64 {
	if sequence == nil {
		return nil
	}
	value := *sequence
	return &value
}

func formatTimestamp(timestamp time.Time) string {
	return timestamp.UTC().Format(time.RFC3339Nano)
}
