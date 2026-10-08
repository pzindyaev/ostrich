package vm

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// tpmBin emulates a TPM 2.0 in software; QEMU talks to it over a Unix socket.
const tpmBin = "swtpm"

// CheckTPM reports whether the TPM emulator is installed.
func CheckTPM() error {
	if _, err := exec.LookPath(tpmBin); err != nil {
		return fmt.Errorf("%s not found — it provides the emulated TPM 2.0. Install it:\n"+
			"  sudo pacman -S swtpm      # Arch\n"+
			"  sudo apt install swtpm    # Debian/Ubuntu\n"+
			"  sudo dnf install swtpm    # Fedora\n"+
			"  brew install swtpm        # macOS\n"+
			"or disable the TPM for this VM", tpmBin)
	}
	return nil
}

// startTPM launches swtpm for the VM as a daemon and waits for its control
// socket. The TPM's state lives in the VM directory, so the guest sees the
// same TPM across restarts. --terminate makes swtpm exit on its own once
// QEMU disconnects; stopTPM is the belt to that braces.
func startTPM(storagePath, name string) error {
	if err := CheckTPM(); err != nil {
		return err
	}
	stopTPM(storagePath, name) // a stale daemon would hold the socket
	stateDir := TPMDir(storagePath, name)
	if err := os.MkdirAll(stateDir, 0700); err != nil {
		return fmt.Errorf("create TPM state directory: %w", err)
	}
	sock := TPMSockPath(storagePath, name)

	cmd := exec.Command(tpmBin, "socket", "--tpm2",
		"--tpmstate", "dir="+stateDir,
		"--ctrl", "type=unixio,path="+sock,
		"--pid", "file="+TPMPIDPath(storagePath, name),
		"--log", "file="+TPMLogPath(storagePath, name),
		"--terminate", "--daemon",
	)
	// The parent returns once the daemon is set up; should the daemon hang on
	// to our stdio pipe, give up waiting for it rather than block forever.
	cmd.WaitDelay = 2 * time.Second
	out, err := cmd.CombinedOutput()
	if err != nil && !errors.Is(err, exec.ErrWaitDelay) {
		return fmt.Errorf("start %s: %w\n%s", tpmBin, err, strings.TrimSpace(string(out)))
	}

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(sock); err == nil {
			return nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	stopTPM(storagePath, name)
	return fmt.Errorf("%s did not create its socket %s", tpmBin, sock)
}

// stopTPM terminates the VM's swtpm if it is still around and removes its
// socket and PID file. Safe to call when nothing is running.
func stopTPM(storagePath, name string) {
	pidPath := TPMPIDPath(storagePath, name)
	if data, err := os.ReadFile(pidPath); err == nil {
		if pid, err := strconv.Atoi(strings.TrimSpace(string(data))); err == nil && pid > 0 {
			if proc, err := os.FindProcess(pid); err == nil && proc.Signal(syscall.SIGTERM) == nil {
				deadline := time.Now().Add(2 * time.Second)
				for time.Now().Before(deadline) && proc.Signal(syscall.Signal(0)) == nil {
					time.Sleep(50 * time.Millisecond)
				}
				if proc.Signal(syscall.Signal(0)) == nil {
					_ = proc.Signal(syscall.SIGKILL)
				}
			}
		}
	}
	_ = os.Remove(pidPath)
	_ = os.Remove(TPMSockPath(storagePath, name))
}

// tpmDevice is the guest-facing TPM interface for the machine type: the ISA
// TIS device on x86, its sysbus variant on the arm "virt" board.
func tpmDevice(machine string) string {
	if machine == "virt" {
		return "tpm-tis-device"
	}
	return "tpm-tis"
}
