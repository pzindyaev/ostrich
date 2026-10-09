package vm

import (
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
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

// TestAwaitStartup: the startup watch returns as soon as the monitor answers,
// fails with QEMU's output when QEMU exits, and gives up waiting after the
// grace period.
func TestAwaitStartup(t *testing.T) {
	dir := t.TempDir()
	logPath := filepath.Join(dir, "qemu.log")

	t.Run("monitor answers", func(t *testing.T) {
		sock := filepath.Join(dir, "up.sock")
		fakeHMPSessions(t, sock, func(string) string { return "" })
		exited := make(chan error, 1)
		started := time.Now()
		if err := awaitStartup(exited, sock, logPath, 10*time.Second); err != nil {
			t.Fatalf("awaitStartup = %v", err)
		}
		if time.Since(started) > 2*time.Second {
			t.Error("did not return when the monitor answered")
		}
	})

	t.Run("exits with output", func(t *testing.T) {
		out := "access denied by acl file\nqemu-system-x86_64: -netdev bridge,id=net0,br=br0: bridge helper failed\n"
		if err := os.WriteFile(logPath, []byte(out), 0o644); err != nil {
			t.Fatal(err)
		}
		exited := make(chan error, 1)
		exited <- &exec.ExitError{ProcessState: exitedState(t, 1)}
		err := awaitStartup(exited, filepath.Join(dir, "none.sock"), logPath, 10*time.Second)
		if err == nil {
			t.Fatal("awaitStartup = nil for a QEMU that exited")
		}
		for _, want := range []string{"exit status 1", "access denied by acl file", "bridge helper failed"} {
			if !strings.Contains(err.Error(), want) {
				t.Errorf("error %q lacks %q", err, want)
			}
		}
	})

	t.Run("exits without output", func(t *testing.T) {
		if err := os.WriteFile(logPath, nil, 0o644); err != nil {
			t.Fatal(err)
		}
		exited := make(chan error, 1)
		exited <- &exec.ExitError{ProcessState: exitedState(t, 1)}
		err := awaitStartup(exited, filepath.Join(dir, "none.sock"), logPath, 10*time.Second)
		if err == nil || !strings.Contains(err.Error(), "without any output") {
			t.Errorf("awaitStartup = %v", err)
		}
	})

	t.Run("grace period over", func(t *testing.T) {
		exited := make(chan error, 1)
		started := time.Now()
		if err := awaitStartup(exited, filepath.Join(dir, "none.sock"), logPath, 300*time.Millisecond); err != nil {
			t.Fatalf("awaitStartup = %v", err)
		}
		if d := time.Since(started); d < 300*time.Millisecond || d > 2*time.Second {
			t.Errorf("returned after %v, want about the grace period", d)
		}
	})

	// A listener that never answers, like QEMU's before its main loop runs,
	// does not count as up.
	t.Run("monitor silent", func(t *testing.T) {
		sock := filepath.Join(dir, "silent.sock")
		ln, err := net.Listen("unix", sock)
		if err != nil {
			t.Fatal(err)
		}
		defer ln.Close()
		exited := make(chan error, 1)
		started := time.Now()
		if err := awaitStartup(exited, sock, logPath, 300*time.Millisecond); err != nil {
			t.Fatalf("awaitStartup = %v", err)
		}
		if time.Since(started) < 300*time.Millisecond {
			t.Error("a silent listener counted as a running monitor")
		}
	})
}

// TestStartFailureTruncates: a long QEMU output is cut to its last lines and
// points at the log file.
func TestStartFailureTruncates(t *testing.T) {
	logPath := filepath.Join(t.TempDir(), "qemu.log")
	var lines []string
	for i := 1; i <= startErrLines+5; i++ {
		lines = append(lines, "line "+strconv.Itoa(i))
	}
	if err := os.WriteFile(logPath, []byte(strings.Join(lines, "\n")+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	err := startFailure(nil, logPath)
	if err == nil {
		t.Fatal("startFailure = nil")
	}
	msg := err.Error()
	if strings.Contains(msg, "line 5\n") || !strings.Contains(msg, "line 6\n") || !strings.Contains(msg, "line "+strconv.Itoa(startErrLines+5)) {
		t.Errorf("wrong lines kept:\n%s", msg)
	}
	if !strings.Contains(msg, logPath) {
		t.Errorf("truncated error does not name the log file:\n%s", msg)
	}
	// Short output is quoted whole, without the pointer.
	if err := os.WriteFile(logPath, []byte("one\ntwo\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	msg = startFailure(nil, logPath).Error()
	if !strings.Contains(msg, "\n  one\n  two") || strings.Contains(msg, logPath) {
		t.Errorf("short output: %q", msg)
	}
}

// exitedState is the ProcessState of a child that exited with code.
func exitedState(t *testing.T, code int) *os.ProcessState {
	t.Helper()
	cmd := exec.Command("sh", "-c", "exit "+strconv.Itoa(code))
	_ = cmd.Run()
	if cmd.ProcessState == nil || cmd.ProcessState.ExitCode() != code {
		t.Fatalf("could not produce exit status %d", code)
	}
	return cmd.ProcessState
}

// TestStartReportsQEMUFailure: a QEMU that dies on startup makes Start fail
// with what QEMU printed, and leaves no VM counted as running. Skipped
// without QEMU.
func TestStartReportsQEMUFailure(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	storage := t.TempDir()
	cfg := &VMConfig{Name: "broken", CPU: 1, RAM: 128, DiskSize: 1, Network: NetworkConfig{Type: NetworkNone}}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	// A disk that is not a qcow2 image: QEMU refuses it at once.
	if err := os.WriteFile(DiskPath(storage, cfg.Name), []byte("not an image"), 0o644); err != nil {
		t.Fatal(err)
	}
	err := Start(storage, cfg)
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })
	if err == nil {
		t.Fatal("Start = nil for a QEMU that cannot open its disk")
	}
	t.Logf("Start error:\n%v", err)
	if !strings.Contains(err.Error(), "QEMU exited during startup (exit status 1):") || !strings.Contains(err.Error(), "disk.qcow2") {
		t.Errorf("error does not quote QEMU: %v", err)
	}
	if info, _ := Status(storage, cfg.Name); info.Status != StatusStopped {
		t.Errorf("Status = %+v after a failed start", info)
	}
	if _, err := os.Stat(PIDPath(storage, cfg.Name)); !os.IsNotExist(err) {
		t.Error("pid file left behind by a failed start")
	}
	logged, err := os.ReadFile(QEMULogPath(storage, cfg.Name))
	if err != nil || !strings.Contains(string(logged), "disk.qcow2") {
		t.Errorf("qemu.log = %q, %v", logged, err)
	}
}
