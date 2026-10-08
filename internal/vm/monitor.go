package vm

import (
	"bytes"
	"fmt"
	"io"
	"net"
	"regexp"
	"strings"
	"time"
)

// The HMP monitor (-monitor unix:...) talks a human-oriented protocol: a banner
// and a "(qemu) " prompt on connect, then for each command the readline-style
// echo of what was typed, the command's output, and the prompt again.
const (
	hmpPrompt  = "(qemu) "
	hmpTimeout = 5 * time.Second
)

var ansiEscape = regexp.MustCompile(`\x1b\[[0-9;]*[A-Za-z]`)

// MonitorCommand sends one HMP command to the VM's monitor socket and returns
// its output, with the echo and prompt removed. Commands such as device_add
// return "" on success and an "Error: ..." line on failure.
func MonitorCommand(storagePath, name, command string) (string, error) {
	return hmpCommand(MonitorPath(storagePath, name), command, hmpTimeout)
}

func hmpCommand(sockPath, command string, timeout time.Duration) (string, error) {
	conn, err := net.DialTimeout("unix", sockPath, timeout)
	if err != nil {
		return "", fmt.Errorf("connect to QEMU monitor: %w", err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(timeout))

	if _, err := readUntilPrompt(conn); err != nil {
		return "", fmt.Errorf("QEMU monitor: %w", err)
	}
	if _, err := io.WriteString(conn, command+"\n"); err != nil {
		return "", fmt.Errorf("QEMU monitor: %w", err)
	}
	raw, err := readUntilPrompt(conn)
	if err != nil {
		return "", fmt.Errorf("QEMU monitor: %w", err)
	}
	return cleanHMPResponse(raw, command), nil
}

// readUntilPrompt accumulates monitor output until the prompt arrives, and
// returns everything before it.
func readUntilPrompt(r io.Reader) (string, error) {
	var buf []byte
	tmp := make([]byte, 4096)
	for {
		n, err := r.Read(tmp)
		buf = append(buf, tmp[:n]...)
		if bytes.HasSuffix(buf, []byte(hmpPrompt)) {
			return string(buf[:len(buf)-len(hmpPrompt)]), nil
		}
		if err != nil {
			if err == io.EOF {
				err = fmt.Errorf("connection closed (is another client attached to the monitor?)")
			}
			return string(buf), err
		}
	}
}

// cleanHMPResponse strips terminal escapes, CRs and the echoed command line.
// Readline redraws the whole input after every byte, so the echo line is a
// jumble of prefixes that ends with the full command.
func cleanHMPResponse(raw, command string) string {
	s := ansiEscape.ReplaceAllString(raw, "")
	s = strings.ReplaceAll(s, "\r", "")
	if first, rest, ok := strings.Cut(s, "\n"); ok && strings.HasSuffix(first, command) {
		s = rest
	}
	return strings.TrimSpace(s)
}
