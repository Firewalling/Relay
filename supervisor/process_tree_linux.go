//go:build linux

package supervisor

import (
	"errors"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
)

type platformProcessTree struct{}

func newProcessTree() processTree {
	return platformProcessTree{}
}
func (platformProcessTree) supported() bool {
	return true
}

func (platformProcessTree) configure(command *exec.Cmd) {
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
}

func (platformProcessTree) graceful(process *os.Process) error {
	return signalProcessGroup(process.Pid, syscall.SIGTERM)
}

func (platformProcessTree) kill(process *os.Process) error {
	return signalProcessGroup(process.Pid, syscall.SIGKILL)
}

func (platformProcessTree) alive(process *os.Process) bool {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		err = syscall.Kill(-process.Pid, 0)
		return err == nil || errors.Is(err, syscall.EPERM)
	}
	foundGroup := false
	for _, entry := range entries {
		if _, err := strconv.Atoi(entry.Name()); err != nil {
			continue
		}
		stat, err := os.ReadFile("/proc/" + entry.Name() + "/stat")
		if err != nil {
			continue
		}
		statText := string(stat)
		closeParen := strings.LastIndexByte(statText, ')')
		if closeParen < 0 {
			continue
		}
		fields := strings.Fields(statText[closeParen+1:])
		if len(fields) < 3 {
			continue
		}
		processGroup, err := strconv.Atoi(fields[2])
		if err != nil || processGroup != process.Pid {
			continue
		}
		foundGroup = true
		if fields[0] != "Z" {
			return true
		}
	}
	if foundGroup {
		return false
	}
	err = syscall.Kill(-process.Pid, 0)
	return err == nil || errors.Is(err, syscall.EPERM)
}

func signalProcessGroup(pid int, signal syscall.Signal) error {
	if pid <= 0 {
		return errors.New("invalid process group identifier")
	}
	err := syscall.Kill(-pid, signal)
	if errors.Is(err, syscall.ESRCH) {
		return nil
	}
	return err
}
