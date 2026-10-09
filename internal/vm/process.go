package vm

import (
	"bufio"
	"fmt"
	"net"
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

// Start watches a freshly launched QEMU until it is evidently up or has
// died. QEMU that rejects its command line or cannot open a file exits within
// milliseconds; one that comes up has its monitor answering once its setup is
// through and the main loop runs. Past the grace period it is assumed up.
const (
	startGrace         = 3 * time.Second
	startProbeTimeout  = 250 * time.Millisecond
	startProbeInterval = 50 * time.Millisecond
	startErrLines      = 8 // of QEMU's output shown in the start error
)

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
// It fails when the VM wants UEFI firmware and none is installed.
func BuildQEMUArgs(cfg *VMConfig, storagePath string) (string, []string, error) {
	arch := archOf(cfg)
	bin := fmt.Sprintf("qemu-system-%s", arch)
	if err := ValidateDisks(cfg.Disks); err != nil {
		return "", nil, err
	}

	diskPath := DiskPath(storagePath, cfg.Name)
	consolePath := ConsolePath(storagePath, cfg.Name)
	serialSock := SerialSockPath(storagePath, cfg.Name)
	monitorPath := MonitorPath(storagePath, cfg.Name)
	pidPath := PIDPath(storagePath, cfg.Name)

	machine := machineOf(cfg)
	machineOpts := machine

	// UEFI: the firmware code and the VM's own NVRAM copy sit on two pflash
	// units. Secure Boot builds keep the variable store behind SMM, so SMM
	// must be on and flash writes restricted to it.
	var firmwareArgs []string
	if cfg.UEFI() {
		fw, err := FindFirmware(arch, machine, cfg.SecureBoot)
		if err != nil {
			return "", nil, err
		}
		if fw.RequiresSMM {
			machineOpts += ",smm=on"
			firmwareArgs = append(firmwareArgs, "-global", "driver=cfi.pflash01,property=secure,value=on")
		}
		firmwareArgs = append(firmwareArgs,
			"-drive", fmt.Sprintf("if=pflash,format=%s,unit=0,readonly=on,file=%s", fw.CodeFormat, fw.Code),
			"-drive", fmt.Sprintf("if=pflash,format=%s,unit=1,file=%s", fw.VarsFormat, FirmwareVarsPath(storagePath, cfg.Name)),
		)
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
		"-machine", machineOpts,
	}
	args = append(args, firmwareArgs...)
	args = append(args, "-drive", fmt.Sprintf("file=%s,format=qcow2,if=virtio", diskPath))
	// Additional disks, each on its own PCIe root port; the ports are always
	// there so a disk can be hot-plugged into a running VM.
	args = append(args, extraDiskArgs(cfg, storagePath)...)
	args = append(args,
		"-chardev", serialChardev,
		"-serial", "chardev:serial0",
		"-monitor", fmt.Sprintf("unix:%s,server,nowait", monitorPath),
		"-pidfile", pidPath,
		"-display", "none",
	)

	// KVM acceleration when available
	if _, err := os.Stat("/dev/kvm"); err == nil {
		args = append(args, "-enable-kvm", "-cpu", "host")
	}

	// Boot media. The CD-ROM drive is always there, empty without an ISO, so
	// one can be put in while the VM runs. The boot order only steers SeaBIOS;
	// OVMF boots the disk once an OS is installed there and tries the CD
	// before that.
	args = append(args, cdromArgs(machine, cfg.CDROMPath)...)
	if cfg.CDROMPath != "" {
		args = append(args, "-boot", "order=dc")
	}

	// TPM 2.0, backed by the swtpm daemon started alongside QEMU.
	if cfg.TPM {
		args = append(args,
			"-chardev", "socket,id=chrtpm,path="+TPMSockPath(storagePath, cfg.Name),
			"-tpmdev", "emulator,id=tpm0,chardev=chrtpm",
			"-device", tpmDevice(machine)+",tpmdev=tpm0",
		)
	}

	// VNC display (TCP, localhost-only)
	if cfg.VNCPort > 0 {
		args = append(args, "-vnc", fmt.Sprintf("127.0.0.1:%d", cfg.VNCPort))
	}

	// USB: an xHCI controller is always present so host devices can be
	// hot-plugged into a running VM; configured devices are attached at boot.
	// A device that is not connected yet is picked up by QEMU when plugged in.
	args = append(args, "-device", "qemu-xhci,id="+usbControllerID)
	for i, id := range USBDeviceIDs(cfg.USBDevices) {
		args = append(args, "-device", usbHostDevice(cfg.USBDevices[i], id))
	}
	// Disk images attached as USB sticks share that bus; each is a drive plus
	// a usb-storage device on top of it.
	for i, id := range USBImageIDs(cfg.USBImages) {
		args = append(args,
			"-drive", usbImageDrive(cfg.USBImages[i], usbImageDriveID(id)),
			"-device", usbImageDevice(id, usbImageDriveID(id)),
		)
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

	return bin, args, nil
}

// qemuOptEscape makes s safe as a value in a QEMU option string, where a
// comma is the separator and is written as ",,".
func qemuOptEscape(s string) string {
	return strings.ReplaceAll(s, ",", ",,")
}

// Start launches QEMU for the VM. The process is detached so it survives TUI exit.
func Start(storagePath string, cfg *VMConfig) error {
	info, err := Status(storagePath, cfg.Name)
	if err == nil && info.Status == StatusRunning {
		return fmt.Errorf("VM %q is already running (PID %d)", cfg.Name, info.PID)
	}

	for _, d := range cfg.USBDevices {
		if err := d.Validate(); err != nil {
			return err
		}
	}
	// QEMU's own failure to open a USB device is only a warning in its log and
	// the VM would run with the device missing, so check up front.
	if err := CheckUSBAccess(cfg.USBDevices); err != nil {
		return err
	}
	// Likewise QEMU would not start with an image file missing.
	if cfg.CDROMPath != "" {
		if err := checkImage(cfg.CDROMPath); err != nil {
			return fmt.Errorf("boot ISO: %w\nEject it in the ISO hot-plug screen, clear it in the edit form, or put the file back.", err)
		}
	}
	if err := CheckUSBImages(cfg.USBImages); err != nil {
		return err
	}
	if err := CheckExtraDisks(storagePath, cfg); err != nil {
		return err
	}

	// A VM whose vm.yaml was switched to UEFI by hand has no NVRAM yet.
	if err := EnsureFirmwareVars(storagePath, cfg); err != nil {
		return err
	}
	bin, args, err := BuildQEMUArgs(cfg, storagePath)
	if err != nil {
		return err
	}
	if _, err := exec.LookPath(bin); err != nil {
		return fmt.Errorf("%q not found in PATH — is QEMU installed?", bin)
	}

	if cfg.TPM {
		if err := startTPM(storagePath, cfg.Name); err != nil {
			return err
		}
	}
	cmd := exec.Command(bin, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true} // detach from terminal session

	devNull, err := os.Open(os.DevNull)
	if err != nil {
		stopTPM(storagePath, cfg.Name)
		return err
	}
	defer devNull.Close()
	// QEMU's own output goes to a file in the VM directory: it is what tells
	// why a start failed, and it has to outlive Ostrich (a pipe would not).
	logPath := QEMULogPath(storagePath, cfg.Name)
	logFile, err := os.OpenFile(logPath, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0644)
	if err != nil {
		stopTPM(storagePath, cfg.Name)
		return fmt.Errorf("create QEMU log: %w", err)
	}
	defer logFile.Close()
	cmd.Stdin = devNull
	cmd.Stdout = logFile
	cmd.Stderr = logFile

	if err := cmd.Start(); err != nil {
		stopTPM(storagePath, cfg.Name)
		return fmt.Errorf("start QEMU: %w", err)
	}

	// Write PID immediately (QEMU also writes it via -pidfile after forking,
	// but we write it now as a fallback)
	pidPath := PIDPath(storagePath, cfg.Name)
	if err := os.WriteFile(pidPath, []byte(strconv.Itoa(cmd.Process.Pid)), 0644); err != nil {
		_ = cmd.Process.Kill()
		return fmt.Errorf("write PID file: %w", err)
	}

	// QEMU runs on its own (its own session, no pipes to us), but it stays our
	// child: reap it when it exits, or it would linger as a zombie that still
	// answers signals and so would look like a running VM. Should Ostrich exit
	// first, init takes over the reaping.
	exited := make(chan error, 1)
	go func() { exited <- cmd.Wait() }()

	// A QEMU that will not start is gone within moments, so wait for it to
	// either die or come up rather than call the launch a success.
	if err := awaitStartup(exited, MonitorPath(storagePath, cfg.Name), logPath, startGrace); err != nil {
		cleanupPID(storagePath, cfg.Name)
		stopTPM(storagePath, cfg.Name)
		return err
	}
	return nil
}

// awaitStartup waits for a just-launched QEMU to show whether it made it:
// exited delivers its exit status should it die, and its monitor answers
// once it is up. After grace it is taken to be up; should it die later, its
// log still has the reason.
func awaitStartup(exited <-chan error, monitorSock, logPath string, grace time.Duration) error {
	deadline := time.Now().Add(grace)
	for {
		select {
		case err := <-exited:
			return startFailure(err, logPath)
		default:
		}
		if time.Now().After(deadline) {
			return nil
		}
		if monitorAnswers(monitorSock, startProbeTimeout) {
			return nil
		}
		time.Sleep(startProbeInterval)
	}
}

// monitorAnswers reports whether the HMP monitor behind sock accepts a
// connection and prompts within timeout. QEMU creates the listening socket
// while still parsing its command line, so a connection alone proves little;
// the prompt arrives once the main loop runs, that is, once setup is through.
func monitorAnswers(sock string, timeout time.Duration) bool {
	conn, err := net.DialTimeout("unix", sock, timeout)
	if err != nil {
		return false
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(timeout))
	_, err = readUntilPrompt(conn)
	return err == nil
}

// startFailure describes a QEMU that exited during startup, quoting what it
// printed. The TUI gets the last few lines; the log file keeps everything.
func startFailure(waitErr error, logPath string) error {
	how := "QEMU exited during startup"
	if waitErr != nil {
		how += " (" + waitErr.Error() + ")" // "exit status 1", "signal: killed"
	}
	lines, _ := readTail(logPath, startErrLines+1)
	if len(lines) == 0 {
		return fmt.Errorf("%s without any output", how)
	}
	var b strings.Builder
	b.WriteString(how)
	b.WriteString(":")
	truncated := len(lines) > startErrLines
	if truncated {
		lines = lines[1:]
	}
	for _, l := range lines {
		b.WriteString("\n  ")
		b.WriteString(l)
	}
	if truncated {
		fmt.Fprintf(&b, "\n  (full output in %s)", logPath)
	}
	return fmt.Errorf("%s", b.String())
}

// Stop sends SIGTERM to the VM process and waits up to 5 s before SIGKILL.
// The TPM emulator, if any, goes with it.
func Stop(storagePath, name string) error {
	defer stopTPM(storagePath, name)
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
		if !processAlive(info.PID) {
			break // process is gone
		}
		time.Sleep(500 * time.Millisecond)
	}

	// Force-kill if still alive
	if processAlive(info.PID) {
		_ = proc.Signal(syscall.SIGKILL)
		time.Sleep(100 * time.Millisecond)
	}

	cleanupPID(storagePath, name)
	return nil
}

// processAlive reports whether pid names a live process. A zombie, one that
// has exited but whose parent has not reaped it yet, still answers a null
// signal, so on Linux the state in /proc is checked as well.
func processAlive(pid int) bool {
	proc, err := os.FindProcess(pid)
	if err != nil {
		return false
	}
	if err := proc.Signal(syscall.Signal(0)); err != nil {
		return false
	}
	return !isZombie(pid)
}

// isZombie reads the process state from /proc/<pid>/stat. Without procfs
// (macOS) nothing can be told, and a process answering signals counts as alive.
func isZombie(pid int) bool {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return false
	}
	// "pid (comm) S ...": comm may hold spaces and parentheses, so the state
	// is the field after the last ')'.
	s := string(data)
	i := strings.LastIndexByte(s, ')')
	if i < 0 || i+2 >= len(s) {
		return false
	}
	return s[i+2] == 'Z'
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

	if !processAlive(pid) {
		cleanupPID(storagePath, name)
		return ProcessInfo{Status: StatusStopped}, nil
	}

	return ProcessInfo{PID: pid, Status: StatusRunning}, nil
}

// ReadConsoleTail returns the last maxLines lines from the serial console log.
func ReadConsoleTail(storagePath, name string, maxLines int) ([]string, error) {
	return readTail(ConsolePath(storagePath, name), maxLines)
}

// readTail returns the last maxLines lines of a log file; none when there is
// no such file.
func readTail(path string, maxLines int) ([]string, error) {
	f, err := os.Open(path)
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
