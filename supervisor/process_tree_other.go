//go:build !linux

package supervisor

import (
	"os"
	"os/exec"
)

type platformProcessTree struct{}

func newProcessTree() processTree {
	return platformProcessTree{}
}
func (platformProcessTree) supported() bool {
	return false
}

func (platformProcessTree) configure(*exec.Cmd) {}

func (platformProcessTree) graceful(process *os.Process) error {
	return process.Signal(os.Interrupt)
}

func (platformProcessTree) kill(process *os.Process) error {
	return process.Kill()
}

func (platformProcessTree) alive(*os.Process) bool {
	return false
}
