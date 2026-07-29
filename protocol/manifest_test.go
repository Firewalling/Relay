package protocol

import (
	"strings"
	"testing"
)

func TestCanonicalManifestMatchesRelayProtocolV1(t *testing.T) {
	manifest, err := CanonicalManifest()
	if err != nil {
		t.Fatalf("decode canonical manifest: %v", err)
	}
	if manifest.ProtocolVersion != ProtocolVersion {
		t.Fatalf("protocol version = %d, want %d", manifest.ProtocolVersion, ProtocolVersion)
	}
	if manifest.Transport.Shell {
		t.Fatal("canonical direct-argv transport unexpectedly enables a shell")
	}
	if manifest.Orchestrator.Owner != "go" || manifest.Orchestrator.Implemented {
		t.Fatalf("orchestrator = %#v, want unimplemented Go owner", manifest.Orchestrator)
	}
	if got := len(manifest.Operations); got != 6 {
		t.Fatalf("operation count = %d, want 6", got)
	}
}

func TestDecodeManifestIsStrictAndVersioned(t *testing.T) {
	tests := []struct {
		name string
		json string
		want string
	}{
		{
			name: "unknown field",
			json: `{"protocol_version":1,"relay_version":"0.2.0","unexpected":true}`,
			want: "unknown field",
		},
		{
			name: "unsupported version",
			json: `{"protocol_version":2}`,
			want: "unsupported protocol version 2",
		},
		{
			name: "multiple documents",
			json: `{}` + "\n" + `{}`,
			want: "exactly one JSON document",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			_, err := DecodeManifest(strings.NewReader(test.json))
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("DecodeManifest error = %v, want substring %q", err, test.want)
			}
		})
	}
}
