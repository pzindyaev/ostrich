package vm

import (
	"os"
	"os/exec"
	"runtime"
	"strconv"
	"syscall"
	"testing"
	"time"
)

// TestStatusIgnoresZombie: a child that has exited but not been reaped still
// answers a null signal, and must not count as a running VM.
func TestStatusIgnoresZombie(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("zombie detection reads /proc")
	}
	cmd := exec.Command("true")
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cmd.Wait() })
	pid := cmd.Process.Pid
	// It exits at once; without a Wait it stays a zombie.
	deadline := time.Now().Add(5 * time.Second)
	for !isZombie(pid) && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if !isZombie(pid) {
		t.Fatal("child did not become a zombie")
	}
	if err := syscall.Kill(pid, 0); err != nil {
		t.Fatalf("a zombie should still answer kill -0, got %v", err)
	}
	if processAlive(pid) {
		t.Error("processAlive(zombie) = true")
	}

	storage := t.TempDir()
	if err := os.MkdirAll(VMDir(storage, "z"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(PIDPath(storage, "z"), []byte(strconv.Itoa(pid)), 0o644); err != nil {
		t.Fatal(err)
	}
	info, err := Status(storage, "z")
	if err != nil || info.Status != StatusStopped {
		t.Errorf("Status = %+v, %v; want stopped", info, err)
	}
	if _, err := os.Stat(PIDPath(storage, "z")); !os.IsNotExist(err) {
		t.Error("stale pid file not cleaned up")
	}

	// A live process still counts as running.
	if err := os.WriteFile(PIDPath(storage, "z"), []byte(strconv.Itoa(os.Getpid())), 0o644); err != nil {
		t.Fatal(err)
	}
	if info, _ := Status(storage, "z"); info.Status != StatusRunning || info.PID != os.Getpid() {
		t.Errorf("Status of a live process = %+v", info)
	}
}

// TestStartReapsQEMU: a QEMU that dies is reaped by the Ostrich that started
// it, so it neither lingers as a zombie nor counts as running. Skipped
// without QEMU.
func TestStartReapsQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	storage := t.TempDir()
	cfg := &VMConfig{Name: "reap", CPU: 1, RAM: 128, DiskSize: 1, Network: NetworkConfig{Type: NetworkNone}}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })
	waitForMonitor(t, storage, cfg.Name)
	info, _ := Status(storage, cfg.Name)
	if info.Status != StatusRunning {
		t.Fatalf("Status = %+v after Start", info)
	}

	// Kill it the way a crash would, then it must be gone for good.
	if err := syscall.Kill(info.PID, syscall.SIGKILL); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if err := syscall.Kill(info.PID, 0); err == syscall.ESRCH {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if err := syscall.Kill(info.PID, 0); err != syscall.ESRCH {
		t.Errorf("QEMU %d not reaped after it died (kill -0: %v)", info.PID, err)
	}
	if info, _ := Status(storage, cfg.Name); info.Status != StatusStopped {
		t.Errorf("Status = %+v after the VM died", info)
	}
}
