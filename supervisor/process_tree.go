package supervisor

import (
	"os"
	"os/exec"
)

type processTree interface {
	supported() bool
	configure(*exec.Cmd)
	graceful(*os.Process) error
	kill(*os.Process) error
	alive(*os.Process) bool
}
