//go:build !aix && !darwin && !dragonfly && !freebsd && !linux && !netbsd && !openbsd && !solaris

package main

import "os"

func terminationSignals() []os.Signal {
	return []os.Signal{os.Interrupt}
}

func isBrokenPipe(error) bool {
	return false
}

func prepareOutputWrite() {}
