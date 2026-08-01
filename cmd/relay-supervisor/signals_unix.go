//go:build aix || darwin || dragonfly || freebsd || linux || netbsd || openbsd || solaris

package main

import (
	"errors"
	"os"
	"os/signal"
	"syscall"
)

func terminationSignals() []os.Signal {
	return []os.Signal{os.Interrupt, syscall.SIGTERM}
}

func isBrokenPipe(err error) bool {
	return errors.Is(err, syscall.EPIPE)
}

func prepareOutputWrite() {
	signal.Ignore(syscall.SIGPIPE)
}
