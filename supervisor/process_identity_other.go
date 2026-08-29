//go:build !linux

package supervisor

import (
	"errors"

	"relay/protocol"
)

func captureProcessIdentity(int) (protocol.ProcessIdentity, error) {
	return protocol.ProcessIdentity{}, errors.New("process identity is unsupported on this platform")
}
