//go:build linux

package supervisor

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"relay/protocol"
)

func captureProcessIdentity(pid int) (protocol.ProcessIdentity, error) {
	if pid <= 0 {
		return protocol.ProcessIdentity{}, fmt.Errorf("invalid process PID %d", pid)
	}
	stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return protocol.ProcessIdentity{}, fmt.Errorf("read process stat: %w", err)
	}
	statText := string(stat)
	closeParen := strings.LastIndexByte(statText, ')')
	if closeParen < 0 {
		return protocol.ProcessIdentity{}, fmt.Errorf("process stat has no command terminator")
	}
	fields := strings.Fields(statText[closeParen+1:])
	if len(fields) <= 19 {
		return protocol.ProcessIdentity{}, fmt.Errorf("process stat has too few fields")
	}
	processGroupID, err := strconv.Atoi(fields[2])
	if err != nil || processGroupID <= 0 {
		return protocol.ProcessIdentity{}, fmt.Errorf("process stat has invalid process group")
	}
	sessionID, err := strconv.Atoi(fields[3])
	if err != nil || sessionID <= 0 {
		return protocol.ProcessIdentity{}, fmt.Errorf("process stat has invalid session")
	}
	startTimeTicks, err := strconv.ParseUint(fields[19], 10, 64)
	if err != nil || startTimeTicks == 0 {
		return protocol.ProcessIdentity{}, fmt.Errorf("process stat has invalid start time")
	}
	executable, err := os.Readlink(fmt.Sprintf("/proc/%d/exe", pid))
	if err != nil {
		return protocol.ProcessIdentity{}, fmt.Errorf("read process executable: %w", err)
	}
	if executable == "" {
		return protocol.ProcessIdentity{}, fmt.Errorf("process executable is empty")
	}
	return protocol.ProcessIdentity{
		PID:            pid,
		StartTimeTicks: startTimeTicks,
		ProcessGroupID: processGroupID,
		SessionID:      sessionID,
		Executable:     executable,
	}, nil
}
