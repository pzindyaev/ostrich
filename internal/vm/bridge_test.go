package vm

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeFile(t *testing.T, path, content string, mode os.FileMode) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), mode); err != nil {
		t.Fatal(err)
	}
	// Writing as a non-root user strips the setuid bit, so set it afterwards.
	if err := os.Chmod(path, mode); err != nil {
		t.Fatal(err)
	}
}

// TestBridgeAllowed: the ACL is read the way qemu-bridge-helper reads it.
func TestBridgeAllowed(t *testing.T) {
	dir := t.TempDir()
	extra := filepath.Join(dir, "extra.conf")
	writeFile(t, extra, "allow br1\n", 0o644)
	cases := []struct {
		conf string
		want bool
	}{
		{"allow br0\n", true},
		{"allow all\n", true},
		{"allow virbr0\n", false},
		{"", false},
		{"# allow br0\n", false},
		{"allow br0 # ours\n", true},
		{"allow br0\ndeny br0\n", false},
		{"allow all\ndeny br0\n", false},
		{"allow br0\ndeny all\n", false},
		{"  allow   br0  \n", true},
		{"include " + extra + "\n", false},
		{"include " + extra + "\nallow br0\n", true},
		{"include " + filepath.Join(dir, "missing.conf") + "\nallow br0\n", true},
	}
	for _, c := range cases {
		path := filepath.Join(dir, "bridge.conf")
		writeFile(t, path, c.conf, 0o644)
		got, err := bridgeAllowed(path, "br0")
		if err != nil || got != c.want {
			t.Errorf("bridgeAllowed(%q) = %v, %v; want %v", c.conf, got, err, c.want)
		}
	}
	if _, err := bridgeAllowed(filepath.Join(dir, "nope.conf"), "br0"); err == nil {
		t.Error("a missing ACL file read without error")
	}
}

// fakeBridgeHost lays out a host under dir: a bridge when bridge is set, an
// ACL file with acl (none when ""), a helper with the given mode (none when
// 0), ufw on or off, nmcli there or not.
func fakeBridgeHost(t *testing.T, dir, bridge, acl string, helperMode os.FileMode, ufw, nmcli bool) bridgeHost {
	t.Helper()
	sys := filepath.Join(dir, "sys")
	if err := os.MkdirAll(filepath.Join(sys, "lo"), 0o755); err != nil {
		t.Fatal(err)
	}
	if bridge != "" {
		if err := os.MkdirAll(filepath.Join(sys, bridge, "bridge"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	aclPath := filepath.Join(dir, "etc", "qemu", "bridge.conf")
	if acl != "" {
		writeFile(t, aclPath, acl, 0o644)
	}
	helper := filepath.Join(dir, "lib", "qemu-bridge-helper")
	if helperMode != 0 {
		writeFile(t, helper, "#!/bin/sh\n", helperMode)
	}
	ufwConf := filepath.Join(dir, "etc", "ufw", "ufw.conf")
	if ufw {
		writeFile(t, ufwConf, "LOGLEVEL=low\nENABLED=yes\n", 0o644)
	}
	return bridgeHost{
		sysClassNet: sys,
		aclPaths:    []string{aclPath},
		helperPaths: []string{helper},
		ufwConf:     ufwConf,
		lookPath: func(name string) (string, error) {
			if nmcli && name == "nmcli" {
				return "/usr/bin/nmcli", nil
			}
			return "", os.ErrNotExist
		},
	}
}

// TestBridgeHint: a ready host gets no hint; a bare one gets the problems and
// the commands that fix each, tailored to what the host has.
func TestBridgeHint(t *testing.T) {
	t.Run("ready", func(t *testing.T) {
		h := fakeBridgeHost(t, t.TempDir(), "br0", "allow br0\n", 0o755|os.ModeSetuid, true, true)
		s := inspectBridge(h, "br0")
		if !s.ok() || s.hint() != "" {
			t.Errorf("ready host: ok=%v hint=%q (%+v)", s.ok(), s.hint(), s)
		}
	})

	t.Run("nothing there, NetworkManager and ufw", func(t *testing.T) {
		h := fakeBridgeHost(t, t.TempDir(), "", "allow virbr0\n", 0o755|os.ModeSetuid, true, true)
		hint := inspectBridge(h, "br0").hint()
		for _, want := range []string{
			"the host has no bridge br0",
			"bridge.conf does not allow it",
			"sudo nmcli con add type bridge ifname br0 con-name br0 \\",
			"ipv4.method shared ipv4.addresses 192.168.76.1/24",
			"sudo nmcli con up br0",
			"sudo ufw allow in on br0 to any port 67 proto udp",
			"sudo ufw route allow in on br0",
			"echo 'allow br0' | sudo tee -a " + h.aclPaths[0],
		} {
			if !strings.Contains(hint, want) {
				t.Errorf("hint lacks %q:\n%s", want, hint)
			}
		}
		if strings.Contains(hint, "ip link add") || strings.Contains(hint, "setuid") || strings.Contains(hint, "install -d") {
			t.Errorf("hint has commands the host does not need:\n%s", hint)
		}
	})

	t.Run("no NetworkManager, no ufw, no ACL file", func(t *testing.T) {
		h := fakeBridgeHost(t, t.TempDir(), "", "", 0o755|os.ModeSetuid, false, false)
		hint := inspectBridge(h, "br0").hint()
		for _, want := range []string{
			"sudo ip link add br0 type bridge",
			"sudo ip link set br0 up",
			"there is no " + h.aclPaths[0] + " allowing it",
			"sudo install -d " + filepath.Dir(h.aclPaths[0]),
			"echo 'allow br0' | sudo tee -a " + h.aclPaths[0],
		} {
			if !strings.Contains(hint, want) {
				t.Errorf("hint lacks %q:\n%s", want, hint)
			}
		}
		// (the temp paths carry the subtest's name, so look for the commands)
		if strings.Contains(hint, "sudo nmcli") || strings.Contains(hint, "sudo ufw") {
			t.Errorf("hint has commands the host does not need:\n%s", hint)
		}
	})

	t.Run("bridge there, not allowed", func(t *testing.T) {
		h := fakeBridgeHost(t, t.TempDir(), "br0", "allow virbr0\n", 0o755|os.ModeSetuid, true, true)
		hint := inspectBridge(h, "br0").hint()
		if strings.Contains(hint, "no bridge") || strings.Contains(hint, "sudo nmcli") || strings.Contains(hint, "sudo ufw") {
			t.Errorf("hint complains about the bridge that is there:\n%s", hint)
		}
		if !strings.Contains(hint, "echo 'allow br0' | sudo tee -a") {
			t.Errorf("hint lacks the ACL line:\n%s", hint)
		}
	})

	t.Run("helper", func(t *testing.T) {
		h := fakeBridgeHost(t, t.TempDir(), "br0", "allow br0\n", 0o755, true, true)
		hint := inspectBridge(h, "br0").hint()
		if !strings.Contains(hint, "is not setuid root") || !strings.Contains(hint, "sudo chmod u+s "+h.helperPaths[0]) {
			t.Errorf("helper without setuid:\n%s", hint)
		}
		h = fakeBridgeHost(t, t.TempDir(), "br0", "allow br0\n", 0, true, true)
		if hint := inspectBridge(h, "br0").hint(); !strings.Contains(hint, "qemu-bridge-helper is not installed") {
			t.Errorf("no helper:\n%s", hint)
		}
	})

	t.Run("interface that is no bridge", func(t *testing.T) {
		dir := t.TempDir()
		h := fakeBridgeHost(t, dir, "", "allow br0\n", 0o755|os.ModeSetuid, false, true)
		if err := os.MkdirAll(filepath.Join(h.sysClassNet, "br0"), 0o755); err != nil {
			t.Fatal(err)
		}
		hint := inspectBridge(h, "br0").hint()
		if !strings.Contains(hint, "br0 is not a bridge") || strings.Contains(hint, "sudo nmcli") {
			t.Errorf("hint:\n%s", hint)
		}
	})

	// Without sysfs (macOS) and with an ACL only root may read, nothing can
	// be told, and nothing is claimed.
	t.Run("unknowable", func(t *testing.T) {
		dir := t.TempDir()
		h := fakeBridgeHost(t, dir, "", "allow virbr0\n", 0o755|os.ModeSetuid, false, true)
		h.sysClassNet = filepath.Join(dir, "no-sysfs")
		if err := os.Chmod(h.aclPaths[0], 0o000); err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = os.Chmod(h.aclPaths[0], 0o644) })
		if os.Getuid() == 0 {
			t.Skip("root reads everything")
		}
		s := inspectBridge(h, "br0")
		if !s.unknown || !s.aclUnread || s.hint() != "" {
			t.Errorf("unknowable host: %+v\n%s", s, s.hint())
		}
	})
}

// TestStartChecksBridge: a tap VM on a host without the bridge is refused
// before QEMU is launched, with the setup hint.
func TestStartChecksBridge(t *testing.T) {
	saved := defaultBridgeHost
	defaultBridgeHost = fakeBridgeHost(t, t.TempDir(), "", "allow virbr0\n", 0o755|os.ModeSetuid, false, true)
	t.Cleanup(func() { defaultBridgeHost = saved })

	storage := t.TempDir()
	cfg := &VMConfig{Name: "tapvm", CPU: 1, RAM: 128, DiskSize: 1, Network: NetworkConfig{Type: NetworkTap, MAC: "52:54:00:00:00:02"}}
	if err := os.MkdirAll(VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	err := Start(storage, cfg)
	if err == nil {
		_ = Stop(storage, cfg.Name)
		t.Fatal("Start = nil without a bridge")
	}
	if !strings.Contains(err.Error(), "the host has no bridge br0") || !strings.Contains(err.Error(), "sudo nmcli con up br0") {
		t.Errorf("Start error lacks the hint: %v", err)
	}
	if _, err := os.Stat(QEMULogPath(storage, cfg.Name)); !os.IsNotExist(err) {
		t.Error("QEMU was launched despite the missing bridge")
	}
}
