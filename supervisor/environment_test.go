package supervisor

import (
	"reflect"
	"strings"
	"testing"

	"relay/protocol"
)

func TestBuildEnvironmentFiltersParentAndUsesDeterministicPrecedence(t *testing.T) {
	parent := []string{
		"PATH=/parent/bin",
		"LANG=C",
		"HOME=/private/home",
		"API_TOKEN=parent-secret",
		"UNRELATED=value",
	}
	user := []protocol.EnvironmentVariable{
		{Name: "PATH", Value: "/user/bin"},
		{Name: "GREETING", Value: "first"},
		{Name: "GREETING", Value: "last"},
		{Name: "RELAY_RUN_ID", Value: "attempted-override"},
	}
	environment, metadata, err := buildEnvironment(parent, user, "run-policy")
	if err != nil {
		t.Fatalf("buildEnvironment: %v", err)
	}
	want := []string{
		"GREETING=last",
		"LANG=C",
		"PATH=/user/bin",
		"RELAY_PROTOCOL_VERSION=1",
		"RELAY_RUN_ID=run-policy",
		"RELAY_SUPERVISOR_VERSION=" + protocol.SupervisorVersion,
	}
	if !reflect.DeepEqual(environment, want) {
		t.Fatalf("environment = %#v, want %#v", environment, want)
	}
	if metadata.DuplicateUserOverrides != 1 {
		t.Fatalf("duplicate override count = %d, want 1", metadata.DuplicateUserOverrides)
	}
	if metadata.BlockedInheritedCount != 3 {
		t.Fatalf("blocked inherited count = %d, want 3", metadata.BlockedInheritedCount)
	}
	for _, entry := range metadata.Effective {
		if strings.Contains(entry.Name, "TOKEN") || entry.Name == "HOME" || entry.Name == "UNRELATED" {
			t.Fatalf("filtered name leaked into metadata: %#v", entry)
		}
	}
	serialized := strings.Join(environment, "\n")
	for _, secret := range []string{"parent-secret", "attempted-override", "/private/home"} {
		if strings.Contains(serialized, secret) {
			t.Fatalf("environment contains blocked value %q", secret)
		}
	}
}

func TestBuildEnvironmentRejectsSensitiveUserVariablesWithoutLeakingValues(t *testing.T) {
	secret := "do-not-repeat-this-value"
	_, _, err := buildEnvironment(nil, []protocol.EnvironmentVariable{{Name: "SERVICE_PASSWORD", Value: secret}}, "run-secret")
	if err == nil {
		t.Fatal("buildEnvironment accepted a sensitive variable")
	}
	if strings.Contains(err.Error(), secret) {
		t.Fatalf("error leaked secret value: %v", err)
	}
	if !strings.Contains(err.Error(), "SERVICE_PASSWORD") {
		t.Fatalf("error does not identify blocked variable name: %v", err)
	}
}

func TestBuildEnvironmentRejectsNonportableNames(t *testing.T) {
	_, _, err := buildEnvironment(nil, []protocol.EnvironmentVariable{{Name: "lowercase", Value: "value"}}, "run-name")
	if err == nil || !strings.Contains(err.Error(), "portable uppercase") {
		t.Fatalf("buildEnvironment error = %v, want portable-name rejection", err)
	}
}
