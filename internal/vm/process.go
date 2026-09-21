package vm

import (
	"bufio"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// VMStatus represents the running state of a VM.
type VMStatus int

const (
	StatusStopped VMStatus = iota
	StatusRunning
)

// ProcessInfo carries the result of a Status check.
type ProcessInfo struct {
	PID    int
	Status VMStatus
}

func (s VMStatus) String() string {
	if s == StatusRunning {
		return "running"
	}
	return "stopped"
}

// SLIRP (user networking) addressing. Each VM gets its own private SLIRP
// network with a single NIC, so the built-in DHCP server always hands out
// UserNetGuestIP. These are QEMU's defaults, passed explicitly so the address
// shown in the UI is one we set rather than one we assume.
const (
	UserNetCIDR    = "10.0.2.0/24"
	UserNetGuestIP = "10.0.2.15"
	UserNetGateway = "10.0.2.2"
)

// BuildQEMUArgs constructs the QEMU binary name and argument slice for a VM.
func BuildQEMUArgs(cfg *VMConfig, storagePath string) (string, []string) {
	arch := cfg.Arch
	if arch == "" {
		arch = "x86_64"
	}
	bin := fmt.Sprintf("qemu-system-%s", arch)

	diskPath := DiskPath(storagePath, cfg.Name)
	consolePath := ConsolePath(storagePath, cfg.Name)
	serialSock := SerialSockPath(storagePath, cfg.Name)
	monitorPath := MonitorPath(storagePath, cfg.Name)
	pidPath := PIDPath(storagePath, cfg.Name)

	machine := "q35"
	if arch == "aarch64" || arch == "arm64" {
		machine = "virt"
	}

	// Serial console: Unix socket (for interactive access) + logfile (for passive log view).
	// Connect interactively with: socat -,escape=0x1d UNIX-CONNECT:<serial.sock>
	serialChardev := fmt.Sprintf(
		"socket,id=serial0,path=%s,server=on,wait=off,logfile=%s",
		serialSock, consolePath,
	)

	args := []string{
		"-name", cfg.Name,
		"-m", fmt.Sprintf("%dM", cfg.RAM),
		"-smp", strconv.Itoa(cfg.CPU),
		"-machine", machine,
		"-drive", fmt.Sprintf("file=%s,format=qcow2,if=virtio", diskPath),
		"-chardev", serialChardev,
		"-serial", "chardev:serial0",
		"-monitor", fmt.Sprintf("unix:%s,server,nowait", monitorPath),
		"-pidfile", pidPath,
		"-display", "none",
	}

	// KVM acceleration when available
	if _, err := os.Stat("/dev/kvm"); err == nil {
		args = append(args, "-enable-kvm", "-cpu", "host")
	}

	// Boot media
	if cfg.CDROMPath != "" {
		args = append(args, "-cdrom", cfg.CDROMPath, "-boot", "order=dc")
	}

	// VNC display (TCP, localhost-only)
	if cfg.VNCPort > 0 {
		args = append(args, "-vnc", fmt.Sprintf("127.0.0.1:%d", cfg.VNCPort))
	}

	// Networking
	switch cfg.Network.Type {
	case NetworkUser:
		netdev := fmt.Sprintf("user,id=net0,net=%s,dhcpstart=%s", UserNetCIDR, UserNetGuestIP)
		for _, pf := range cfg.Network.PortForwards {
			proto := pf.Proto
			if proto == "" {
				proto = "tcp"
			}
			netdev += fmt.Sprintf(",hostfwd=%s::%d-:%d", proto, pf.Host, pf.Guest)
		}
		args = append(args,
			"-netdev", netdev,
			"-device", fmt.Sprintf("virtio-net-pci,netdev=net0,mac=%s", cfg.Network.MAC),
		)
	case NetworkTap:
		// The bridge backend creates the tap through the setuid qemu-bridge-helper,
		// so no root is needed (plain "tap" would open /dev/net/tun itself).
		args = append(args,
			"-netdev", "bridge,id=net0,br=br0",
			"-device", fmt.Sprintf("virtio-net-pci,netdev=net0,mac=%s", cfg.Network.MAC),
		)
	}
	// NetworkNone: no -netdev/-device args

	return bin, args
}

// Start launches QEMU for the VM. The process is detached so it survives TUI exit.
func Start(storagePath string, cfg *VMConfig) error {
	info, err := Status(storagePath, cfg.Name)
	if err == nil && info.Status == StatusRunning {
		return fmt.Errorf("VM %q is already running (PID %d)", cfg.Name, info.PID)
	}

	bin, args := BuildQEMUArgs(cfg, storagePath)
	if _, err := exec.LookPath(bin); err != nil {
		return fmt.Errorf("%q not found in PATH — is QEMU installed?", bin)
	}

	cmd := exec.Command(bin, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true} // detach from terminal session

	devNull, err := os.Open(os.DevNull)
	if err != nil {
		return err
	}
	defer devNull.Close()
	cmd.Stdin = devNull
	cmd.Stdout = devNull
	cmd.Stderr = devNull

	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start QEMU: %w", err)
	}

	// Write PID immediately (QEMU also writes it via -pidfile after forking,
	// but we write it now as a fallback)
	pidPath := PIDPath(storagePath, cfg.Name)
	if err := os.WriteFile(pidPath, []byte(strconv.Itoa(cmd.Process.Pid)), 0644); err != nil {
		_ = cmd.Process.Kill()
		return fmt.Errorf("write PID file: %w", err)
	}

	// Release: let the process run independently
	_ = cmd.Process.Release()
	return nil
}

// Stop sends SIGTERM to the VM process and waits up to 5 s before SIGKILL.
func Stop(storagePath, name string) error {
	info, err := Status(storagePath, name)
	if err != nil || info.Status == StatusStopped {
		return nil
	}

	proc, err := os.FindProcess(info.PID)
	if err != nil {
		cleanupPID(storagePath, name)
		return nil
	}

	if err := proc.Signal(syscall.SIGTERM); err != nil {
		cleanupPID(storagePath, name)
		return nil
	}

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if err := proc.Signal(syscall.Signal(0)); err != nil {
			break // process is gone
		}
		time.Sleep(500 * time.Millisecond)
	}

	// Force-kill if still alive
	if proc.Signal(syscall.Signal(0)) == nil {
		_ = proc.Signal(syscall.SIGKILL)
		time.Sleep(100 * time.Millisecond)
	}

	cleanupPID(storagePath, name)
	return nil
}

// Status reads qemu.pid and checks whether that process is alive.
func Status(storagePath, name string) (ProcessInfo, error) {
	data, err := os.ReadFile(PIDPath(storagePath, name))
	if err != nil {
		if os.IsNotExist(err) {
			return ProcessInfo{Status: StatusStopped}, nil
		}
		return ProcessInfo{}, err
	}

	pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
	if err != nil {
		cleanupPID(storagePath, name)
		return ProcessInfo{Status: StatusStopped}, nil
	}

	proc, err := os.FindProcess(pid)
	if err != nil {
		cleanupPID(storagePath, name)
		return ProcessInfo{Status: StatusStopped}, nil
	}

	if err := proc.Signal(syscall.Signal(0)); err != nil {
		cleanupPID(storagePath, name)
		return ProcessInfo{Status: StatusStopped}, nil
	}

	return ProcessInfo{PID: pid, Status: StatusRunning}, nil
}

// ReadConsoleTail returns the last maxLines lines from the serial console log.
func ReadConsoleTail(storagePath, name string, maxLines int) ([]string, error) {
	f, err := os.Open(ConsolePath(storagePath, name))
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	defer f.Close()

	var lines []string
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		lines = append(lines, scanner.Text())
	}
	if err := scanner.Err(); err != nil {
		return lines, err
	}

	if len(lines) > maxLines {
		lines = lines[len(lines)-maxLines:]
	}
	return lines, nil
}

func cleanupPID(storagePath, name string) {
	_ = os.Remove(PIDPath(storagePath, name))
}
