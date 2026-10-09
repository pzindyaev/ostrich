package vm

import (
	"bufio"
	"net"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// fakeHMP serves one monitor session the way QEMU's readline HMP does: banner
// and prompt, a per-byte redraw echo of the command, then reply and prompt.
func fakeHMP(t *testing.T, sock string, reply func(cmd string) string) {
	t.Helper()
	ln, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		serveHMPSession(conn, reply)
	}()
}

// fakeHMPSessions is fakeHMP for a client that reconnects per command, as
// MonitorCommand does: it serves one session after another until the test ends.
func fakeHMPSessions(t *testing.T, sock string, reply func(cmd string) string) {
	t.Helper()
	ln, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			serveHMPSession(conn, reply)
		}
	}()
}

// serveHMPSession answers one command on conn the way QEMU's readline HMP
// does, then closes it.
func serveHMPSession(conn net.Conn, reply func(cmd string) string) {
	defer conn.Close()
	conn.Write([]byte("QEMU 9.2.0 monitor - type 'help' for more information\r\n(qemu) "))
	line, err := bufio.NewReader(conn).ReadString('\n')
	if err != nil {
		return
	}
	cmd := strings.TrimRight(line, "\n")
	var echo strings.Builder
	for i := range cmd {
		echo.WriteString(strings.Repeat("\x1b[D", i))
		echo.WriteString(cmd[:i+1])
		echo.WriteString("\x1b[K")
	}
	echo.WriteString("\r\n")
	conn.Write([]byte(echo.String() + reply(cmd) + "(qemu) "))
}

func TestHMPCommand(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "m.sock")
	var seen string
	fakeHMP(t, sock, func(cmd string) string {
		seen = cmd
		return ""
	})
	out, err := hmpCommand(sock, "device_add usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c", time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if out != "" {
		t.Errorf("success must yield empty output, got %q", out)
	}
	if !strings.HasPrefix(seen, "device_add usb-host,id=usb-046d-085c") {
		t.Errorf("monitor received %q", seen)
	}
}

func TestHMPCommandError(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "m.sock")
	fakeHMP(t, sock, func(string) string { return "Error: Bus 'xhci.0' not found\r\n" })
	out, err := hmpCommand(sock, "device_add usb-host,id=x,bus=xhci.0", time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if out != "Error: Bus 'xhci.0' not found" {
		t.Errorf("got %q", out)
	}
}

func TestHMPCommandNoSocket(t *testing.T) {
	_, err := hmpCommand(filepath.Join(t.TempDir(), "missing.sock"), "info usb", time.Second)
	if err == nil {
		t.Fatal("expected connect error")
	}
}

func TestCleanHMPResponseWithoutEcho(t *testing.T) {
	// A monitor without readline echoes nothing; the first line is real output.
	if got := cleanHMPResponse("  Device 0.1, Port 1, Speed 480 Mb/s\r\n", "info usb"); !strings.HasPrefix(got, "Device 0.1") {
		t.Errorf("got %q", got)
	}
}

func TestHMPQuote(t *testing.T) {
	cases := map[string]string{
		`file=/isos/a.iso`:           `"file=/isos/a.iso"`,
		`file=/isos/my disk.iso`:     `"file=/isos/my disk.iso"`,
		`file=/isos/say "hi".iso`:    `"file=/isos/say \"hi\".iso"`,
		`file=C:\isos\a.iso`:         `"file=C:\\isos\\a.iso"`,
		"file=/isos/line\nbreak.iso": `"file=/isos/line\nbreak.iso"`,
	}
	for in, want := range cases {
		if got := hmpQuote(in); got != want {
			t.Errorf("hmpQuote(%q) = %s, want %s", in, got, want)
		}
	}
}
