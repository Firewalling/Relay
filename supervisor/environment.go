package supervisor

import (
	"fmt"
	"regexp"
	"sort"
	"strconv"
	"strings"

	"relay/protocol"
)

var portableEnvironmentName = regexp.MustCompile(`^[A-Z_][A-Z0-9_]*$`)

var inheritedEnvironmentAllowlist = []string{
	"LANG",
	"LC_ALL",
	"LC_CTYPE",
	"PATH",
	"SYSTEMROOT",
	"TEMP",
	"TMP",
	"TMPDIR",
	"TZ",
	"WINDIR",
}

var sensitiveEnvironmentFragments = []string{
	"ACCESS_KEY",
	"API_KEY",
	"AUTH_TOKEN",
	"BEARER",
	"COOKIE",
	"CREDENTIAL",
	"PASSWORD",
	"PASSWD",
	"PRIVATE_KEY",
	"SECRET",
	"SESSION_KEY",
	"TOKEN",
}

type environmentValue struct {
	value  string
	source string
}

func buildEnvironment(parent []string, user []protocol.EnvironmentVariable, runID string) ([]string, protocol.EnvironmentMetadata, error) {
	allowed := make(map[string]struct{}, len(inheritedEnvironmentAllowlist))
	for _, name := range inheritedEnvironmentAllowlist {
		allowed[name] = struct{}{}
	}

	parentValues := make(map[string]string)
	blocked := make(map[string]struct{})
	for _, entry := range parent {
		name, value, ok := strings.Cut(entry, "=")
		if !ok || name == "" {
			continue
		}
		if _, ok := allowed[name]; !ok || isSensitiveEnvironmentName(name) {
			blocked[name] = struct{}{}
			continue
		}
		parentValues[name] = value
	}

	effective := make(map[string]environmentValue, len(parentValues)+len(user)+3)
	for name, value := range parentValues {
		effective[name] = environmentValue{value: value, source: "inherited"}
	}

	seenUser := make(map[string]struct{}, len(user))
	duplicateOverrides := 0
	for index, variable := range user {
		if !portableEnvironmentName.MatchString(variable.Name) {
			return nil, protocol.EnvironmentMetadata{}, fmt.Errorf("environment[%d].name must be a portable uppercase environment name", index)
		}
		if isSensitiveEnvironmentName(variable.Name) {
			return nil, protocol.EnvironmentMetadata{}, fmt.Errorf("environment variable %q is blocked by the sensitive-name policy", variable.Name)
		}
		if _, exists := seenUser[variable.Name]; exists {
			duplicateOverrides++
		}
		seenUser[variable.Name] = struct{}{}
		effective[variable.Name] = environmentValue{value: variable.Value, source: "user"}
	}

	required := map[string]string{
		"RELAY_PROTOCOL_VERSION":   strconv.FormatUint(uint64(protocol.ProtocolVersion), 10),
		"RELAY_RUN_ID":             runID,
		"RELAY_SUPERVISOR_VERSION": protocol.SupervisorVersion,
	}
	for name, value := range required {
		effective[name] = environmentValue{value: value, source: "required"}
	}

	names := make([]string, 0, len(effective))
	for name := range effective {
		names = append(names, name)
	}
	sort.Strings(names)
	environment := make([]string, 0, len(names))
	metadataEntries := make([]protocol.EnvironmentEntryMetadata, 0, len(names))
	for _, name := range names {
		entry := effective[name]
		environment = append(environment, name+"="+entry.value)
		metadataEntries = append(metadataEntries, protocol.EnvironmentEntryMetadata{Name: name, Source: entry.source})
	}

	allowlist := append([]string(nil), inheritedEnvironmentAllowlist...)
	metadata := protocol.EnvironmentMetadata{
		InheritedAllowlist:     allowlist,
		Effective:              metadataEntries,
		BlockedInheritedCount:  len(blocked),
		DuplicateUserOverrides: duplicateOverrides,
	}
	return environment, metadata, nil
}

func isSensitiveEnvironmentName(name string) bool {
	upper := strings.ToUpper(name)
	for _, fragment := range sensitiveEnvironmentFragments {
		if strings.Contains(upper, fragment) {
			return true
		}
	}
	return false
}
