package protocol

import (
	"bytes"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"reflect"
)

const (
	ProtocolVersion     uint32 = 1
	OutputSchemaVersion uint32 = 1
	RunStateVersion     uint32 = 1
	SupervisorVersion          = "0.1.0"
	SupervisorCommand          = "supervise"
)

//go:embed manifest-v1.json
var manifestV1 []byte

type Manifest struct {
	ProtocolVersion uint32              `json:"protocol_version"`
	RelayVersion    string              `json:"relay_version"`
	Transport       ManifestTransport   `json:"transport"`
	State           ManifestState       `json:"state"`
	Orchestrator    ManifestComponent   `json:"orchestrator"`
	Workflow        ManifestComponent   `json:"workflow"`
	Operations      []ManifestOperation `json:"operations"`
}

type ManifestTransport struct {
	Kind                   string   `json:"kind"`
	Invocation             string   `json:"invocation"`
	Framing                string   `json:"framing"`
	Shell                  bool     `json:"shell"`
	InvocationPrefix       []string `json:"invocation_prefix"`
	SuccessStream          string   `json:"success_stream"`
	ErrorStream            string   `json:"error_stream"`
	OutputSchemaVersion    uint32   `json:"output_schema_version"`
	OneDocument            bool     `json:"one_document"`
	SuccessExitCode        uint32   `json:"success_exit_code"`
	FailureExitCodeNonzero bool     `json:"failure_exit_code_nonzero"`
}

type ManifestState struct {
	Owner         string `json:"owner"`
	SchemaVersion uint32 `json:"schema_version"`
	Root          string `json:"root"`
}

type ManifestComponent struct {
	Owner       string `json:"owner"`
	Implemented bool   `json:"implemented"`
}

type ManifestOperation struct {
	Name         string   `json:"name"`
	ArgvTemplate []string `json:"argv_template"`
	Mutating     bool     `json:"mutating"`
	Stdin        string   `json:"stdin"`
}

func CanonicalManifest() (Manifest, error) {
	return DecodeManifest(bytes.NewReader(manifestV1))
}

func DecodeManifest(r io.Reader) (Manifest, error) {
	var manifest Manifest
	if err := decodeSingleJSON(r, &manifest); err != nil {
		return Manifest{}, fmt.Errorf("decode protocol manifest: %w", err)
	}
	if err := manifest.Validate(); err != nil {
		return Manifest{}, err
	}
	return manifest, nil
}

func (manifest Manifest) Validate() error {
	if manifest.ProtocolVersion != ProtocolVersion {
		return fmt.Errorf("unsupported protocol version %d (supported: %d)", manifest.ProtocolVersion, ProtocolVersion)
	}
	if manifest.RelayVersion == "" {
		return errors.New("protocol manifest relay_version must not be empty")
	}
	expectedTransport := ManifestTransport{
		Kind:                   "process",
		Invocation:             "direct_argv",
		Framing:                "one_json_document_per_process",
		Shell:                  false,
		InvocationPrefix:       []string{"--json", "--scope", "current"},
		SuccessStream:          "stdout",
		ErrorStream:            "stderr",
		OutputSchemaVersion:    OutputSchemaVersion,
		OneDocument:            true,
		SuccessExitCode:        0,
		FailureExitCodeNonzero: true,
	}
	if !reflect.DeepEqual(manifest.Transport, expectedTransport) {
		return errors.New("protocol manifest transport does not match Relay protocol v1")
	}
	if manifest.State != (ManifestState{Owner: "rust", SchemaVersion: 1, Root: ".relay"}) {
		return errors.New("protocol manifest state ownership does not match Relay protocol v1")
	}
	if manifest.Orchestrator != (ManifestComponent{Owner: "go", Implemented: false}) {
		return errors.New("protocol manifest orchestrator ownership does not match Relay protocol v1")
	}
	if manifest.Workflow != (ManifestComponent{Owner: "lua", Implemented: false}) {
		return errors.New("protocol manifest workflow ownership does not match Relay protocol v1")
	}
	expectedOperations := []ManifestOperation{
		{Name: "status", ArgvTemplate: []string{"status"}, Stdin: "none"},
		{Name: "context", ArgvTemplate: []string{"context", "<options...>"}, Stdin: "none"},
		{Name: "note", ArgvTemplate: []string{"note", "<category>", "--stdin"}, Mutating: true, Stdin: "utf8_note"},
		{Name: "checkpoint", ArgvTemplate: []string{"checkpoint", "[message]"}, Mutating: true, Stdin: "none"},
		{Name: "handoff", ArgvTemplate: []string{"handoff", "<agent>", "<detail-options...>"}, Mutating: true, Stdin: "none"},
		{Name: "finish", ArgvTemplate: []string{"finish"}, Mutating: true, Stdin: "none"},
	}
	if !reflect.DeepEqual(manifest.Operations, expectedOperations) {
		return errors.New("protocol manifest operations do not match Relay protocol v1")
	}
	return nil
}

func decodeSingleJSON(r io.Reader, value any) error {
	decoder := json.NewDecoder(r)
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(value); err != nil {
		return err
	}
	var extra any
	if err := decoder.Decode(&extra); !errors.Is(err, io.EOF) {
		if err == nil {
			return errors.New("expected exactly one JSON document")
		}
		return err
	}
	return nil
}
