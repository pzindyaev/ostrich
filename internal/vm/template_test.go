package vm

import (
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

// newTestVM creates a VM directory with a small qcow2 disk and vm.yaml, the
// way Manager.Create would, without touching firmware or TPM packages.
func newTestVM(t *testing.T, mgr *Manager, cfg *VMConfig) {
	t.Helper()
	if _, err := exec.LookPath(qemuImgBin); err != nil {
		t.Skipf("%s not installed", qemuImgBin)
	}
	if err := os.MkdirAll(VMDir(mgr.StoragePath, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	out, err := exec.Command(qemuImgBin, "create", "-f", "qcow2", DiskPath(mgr.StoragePath, cfg.Name), "64M").CombinedOutput()
	if err != nil {
		t.Fatalf("qemu-img create: %v\n%s", err, out)
	}
	if err := SaveConfig(mgr.StoragePath, cfg); err != nil {
		t.Fatal(err)
	}
}

// virtualSize returns the image's virtual size in bytes, per qemu-img info.
// The top-level object is the image; newer qemu-img also nests its backing
// file's info, with a virtual-size of its own, so the JSON is parsed properly.
func virtualSize(t *testing.T, path string) string {
	t.Helper()
	out, err := exec.Command(qemuImgBin, "info", "--output=json", path).Output()
	if err != nil {
		t.Fatalf("qemu-img info %s: %v", path, err)
	}
	var info struct {
		VirtualSize int64 `json:"virtual-size"`
	}
	if err := json.Unmarshal(out, &info); err != nil || info.VirtualSize == 0 {
		t.Fatalf("no virtual-size in %s (%v)", out, err)
	}
	return strconv.FormatInt(info.VirtualSize, 10)
}

func TestCreateTemplateAndVMFromIt(t *testing.T) {
	mgr := NewManager(t.TempDir())
	src := &VMConfig{
		Name: "win11", CPU: 4, RAM: 8192, DiskSize: 64, Arch: "x86_64",
		CDROMPath: "/isos/win11.iso", Firmware: FirmwareUEFI, SecureBoot: true, TPM: true,
		Network: NetworkConfig{Type: NetworkUser, MAC: "52:54:00:00:00:01",
			PortForwards: []PortForward{{Host: 3389, Guest: 3389, Proto: "tcp"}}},
		VNCPort:    1,
		USBDevices: []USBDevice{{VendorID: "046d", ProductID: "085c"}},
		USBImages:  []USBImage{{Path: "/isos/virtio-win.iso"}},
	}
	newTestVM(t, mgr, src)
	// Stand-ins for the UEFI NVRAM and TPM state the VM would have.
	if err := os.WriteFile(FirmwareVarsPath(mgr.StoragePath, src.Name), []byte("nvram"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(TPMDir(mgr.StoragePath, src.Name), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(TPMDir(mgr.StoragePath, src.Name), "tpm2-00.permall"), []byte("tpm"), 0o600); err != nil {
		t.Fatal(err)
	}
	// Runtime leftovers that must not end up in the template.
	if err := os.WriteFile(ConsolePath(mgr.StoragePath, src.Name), []byte("boot log"), 0o644); err != nil {
		t.Fatal(err)
	}

	if err := mgr.CreateTemplate("win11", "win11-base", "Windows 11 with updates"); err != nil {
		t.Fatalf("CreateTemplate: %v", err)
	}

	tpl, err := LoadTemplate(mgr.StoragePath, "win11-base")
	if err != nil {
		t.Fatal(err)
	}
	want := Template{
		Name: "win11-base", Description: "Windows 11 with updates", SourceVM: "win11",
		CPU: 4, RAM: 8192, DiskSize: 64, Arch: "x86_64",
		Firmware: FirmwareUEFI, SecureBoot: true, TPM: true, Network: NetworkUser, VNC: true,
	}
	got := *tpl
	got.CreatedAt = want.CreatedAt
	if got != want {
		t.Errorf("template = %+v\nwant %+v", got, want)
	}
	if tpl.CreatedAt.IsZero() {
		t.Error("created_at not set")
	}
	if tpl.FirmwareLabel() != "UEFI + Secure Boot, TPM 2.0" {
		t.Errorf("FirmwareLabel = %q", tpl.FirmwareLabel())
	}
	// Per-VM and host-bound settings are not written into template.yaml.
	raw, _ := os.ReadFile(TemplateFilePath(mgr.StoragePath, "win11-base"))
	for _, forbidden := range []string{"mac", "port_forwards", "cdrom", "usb", "vnc_port", "3389", "52:54:00"} {
		if strings.Contains(string(raw), forbidden) {
			t.Errorf("template.yaml carries %q:\n%s", forbidden, raw)
		}
	}

	// The files that make up the machine are copied; runtime files are not.
	if got, want := virtualSize(t, TemplateDiskPath(mgr.StoragePath, "win11-base")), virtualSize(t, DiskPath(mgr.StoragePath, "win11")); got != want {
		t.Errorf("template disk virtual size %s, source %s", got, want)
	}
	if TemplateDiskUsage(mgr.StoragePath, "win11-base") == 0 {
		t.Error("TemplateDiskUsage = 0")
	}
	if b, err := os.ReadFile(TemplateFirmwareVarsPath(mgr.StoragePath, "win11-base")); err != nil || string(b) != "nvram" {
		t.Errorf("NVRAM not copied: %q %v", b, err)
	}
	if b, err := os.ReadFile(filepath.Join(TemplateTPMDir(mgr.StoragePath, "win11-base"), "tpm2-00.permall")); err != nil || string(b) != "tpm" {
		t.Errorf("TPM state not copied: %q %v", b, err)
	}
	if _, err := os.Stat(filepath.Join(TemplateDir(mgr.StoragePath, "win11-base"), "console.log")); !os.IsNotExist(err) {
		t.Error("console.log copied into the template")
	}

	// Templates are listed apart from VMs, and the VM list is not confused
	// by the templates directory.
	tpls, err := mgr.ListTemplates()
	if err != nil || len(tpls) != 1 || tpls[0].Name != "win11-base" {
		t.Fatalf("ListTemplates = %+v, %v", tpls, err)
	}
	vms, err := mgr.List()
	if err != nil || len(vms) != 1 || vms[0].Name != "win11" {
		t.Fatalf("List = %+v, %v", vms, err)
	}
	if !mgr.TemplateExists("win11-base") || mgr.TemplateExists("nope") || mgr.Exists("win11-base") {
		t.Error("existence checks mixed up VMs and templates")
	}
	if err := mgr.CreateTemplate("win11", "win11-base", ""); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Errorf("duplicate template: %v", err)
	}

	// A new VM from the template: the user's choices on top of the
	// template's machine. The source VM's display 1 is taken, so the clone
	// gets display 2.
	if n := mgr.FreeVNCDisplay(); n != 2 {
		t.Errorf("FreeVNCDisplay = %d, want 2", n)
	}
	cfg := tpl.NewVMConfig()
	cfg.Name = "win11-test"
	cfg.CPU, cfg.RAM = 2, 4096
	cfg.Network = NetworkConfig{Type: NetworkTap}
	cfg.VNCPort = mgr.FreeVNCDisplay()
	if err := mgr.CreateFromTemplate("win11-base", cfg); err != nil {
		t.Fatalf("CreateFromTemplate: %v", err)
	}
	clone, err := LoadConfig(mgr.StoragePath, "win11-test")
	if err != nil {
		t.Fatal(err)
	}
	if clone.CPU != 2 || clone.RAM != 4096 || clone.Network.Type != NetworkTap || clone.VNCPort != 2 {
		t.Errorf("user's choices lost: %+v", clone)
	}
	if clone.DiskSize != 64 || clone.Arch != "x86_64" || clone.Firmware != FirmwareUEFI || !clone.SecureBoot || !clone.TPM {
		t.Errorf("template's machine lost: %+v", clone)
	}
	if clone.Network.MAC == "" || clone.Network.MAC == src.Network.MAC {
		t.Errorf("MAC = %q, want a new one", clone.Network.MAC)
	}
	if clone.CDROMPath != "" || len(clone.Network.PortForwards) != 0 || len(clone.USBDevices) != 0 || len(clone.USBImages) != 0 {
		t.Errorf("per-VM settings leaked into the clone: %+v", clone)
	}
	if clone.CreatedAt.IsZero() {
		t.Error("created_at not set on the clone")
	}
	if got, want := virtualSize(t, DiskPath(mgr.StoragePath, "win11-test")), virtualSize(t, DiskPath(mgr.StoragePath, "win11")); got != want {
		t.Errorf("clone disk virtual size %s, source %s", got, want)
	}
	if b, err := os.ReadFile(FirmwareVarsPath(mgr.StoragePath, "win11-test")); err != nil || string(b) != "nvram" {
		t.Errorf("NVRAM not copied to the clone: %q %v", b, err)
	}
	if b, err := os.ReadFile(filepath.Join(TPMDir(mgr.StoragePath, "win11-test"), "tpm2-00.permall")); err != nil || string(b) != "tpm" {
		t.Errorf("TPM state not copied to the clone: %q %v", b, err)
	}
	if err := mgr.CreateFromTemplate("win11-base", cfg); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Errorf("duplicate VM: %v", err)
	}
	// The second clone gets the next free display.
	if n := mgr.FreeVNCDisplay(); n != 3 {
		t.Errorf("FreeVNCDisplay = %d, want 3", n)
	}

	// Deleting the template leaves the clone, a full copy, alone.
	if err := mgr.DeleteTemplate("win11-base"); err != nil {
		t.Fatal(err)
	}
	if mgr.TemplateExists("win11-base") {
		t.Error("template still there")
	}
	if _, err := os.Stat(DiskPath(mgr.StoragePath, "win11-test")); err != nil {
		t.Errorf("clone disk gone with the template: %v", err)
	}
	if err := mgr.DeleteTemplate("win11-base"); err == nil {
		t.Error("deleting a missing template should fail")
	}
}

func TestCreateTemplateRefusesRunningVM(t *testing.T) {
	mgr := NewManager(t.TempDir())
	cfg := &VMConfig{Name: "deb", CPU: 1, RAM: 512, DiskSize: 8}
	newTestVM(t, mgr, cfg)
	// Our own PID is as alive as a VM's would be.
	if err := os.WriteFile(PIDPath(mgr.StoragePath, "deb"), []byte(strconv.Itoa(os.Getpid())), 0o644); err != nil {
		t.Fatal(err)
	}
	err := mgr.CreateTemplate("deb", "deb-base", "")
	if err == nil || !strings.Contains(err.Error(), "running") || !strings.Contains(err.Error(), "shut it down") {
		t.Fatalf("running VM accepted: %v", err)
	}
	if _, err := os.Stat(TemplateDir(mgr.StoragePath, "deb-base")); !os.IsNotExist(err) {
		t.Error("a template directory was left behind")
	}
	if tpls, _ := mgr.ListTemplates(); len(tpls) != 0 {
		t.Errorf("ListTemplates = %+v", tpls)
	}
}

func TestCreateTemplateLeavesNothingOnFailure(t *testing.T) {
	if _, err := exec.LookPath(qemuImgBin); err != nil {
		t.Skipf("%s not installed", qemuImgBin)
	}
	mgr := NewManager(t.TempDir())
	cfg := &VMConfig{Name: "nodisk", CPU: 1, RAM: 512, DiskSize: 8}
	if err := os.MkdirAll(VMDir(mgr.StoragePath, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := SaveConfig(mgr.StoragePath, cfg); err != nil {
		t.Fatal(err)
	}
	err := mgr.CreateTemplate("nodisk", "broken", "")
	if err == nil || !strings.Contains(err.Error(), "disk image") {
		t.Fatalf("missing disk accepted: %v", err)
	}
	if _, err := os.Stat(TemplateDir(mgr.StoragePath, "broken")); !os.IsNotExist(err) {
		t.Error("a template directory was left behind")
	}
}

func TestTemplateDefaultsAndBIOS(t *testing.T) {
	mgr := NewManager(t.TempDir())
	// A hand-written vm.yaml with the optional fields left out.
	cfg := &VMConfig{Name: "min", CPU: 1, RAM: 512, DiskSize: 8}
	newTestVM(t, mgr, cfg)
	if err := mgr.CreateTemplate("min", "min-base", ""); err != nil {
		t.Fatal(err)
	}
	tpl, err := LoadTemplate(mgr.StoragePath, "min-base")
	if err != nil {
		t.Fatal(err)
	}
	if tpl.Arch != "x86_64" || tpl.Firmware != FirmwareBIOS || tpl.Network != NetworkUser || tpl.VNC || tpl.UEFI() {
		t.Errorf("defaults not filled in: %+v", tpl)
	}
	if tpl.FirmwareLabel() != "BIOS" {
		t.Errorf("FirmwareLabel = %q", tpl.FirmwareLabel())
	}
	for _, p := range []string{TemplateFirmwareVarsPath(mgr.StoragePath, "min-base"), TemplateTPMDir(mgr.StoragePath, "min-base")} {
		if _, err := os.Stat(p); !os.IsNotExist(err) {
			t.Errorf("%s made for a BIOS VM without TPM", p)
		}
	}

	clone := tpl.NewVMConfig()
	clone.Name = "min-2"
	if err := mgr.CreateFromTemplate("min-base", clone); err != nil {
		t.Fatal(err)
	}
	got, err := LoadConfig(mgr.StoragePath, "min-2")
	if err != nil {
		t.Fatal(err)
	}
	if got.CPU != 1 || got.RAM != 512 || got.Network.Type != NetworkUser || got.VNCPort != 0 || got.Firmware != FirmwareBIOS {
		t.Errorf("clone = %+v", got)
	}
	if _, err := os.Stat(FirmwareVarsPath(mgr.StoragePath, "min-2")); !os.IsNotExist(err) {
		t.Error("NVRAM made for a BIOS clone")
	}
}

func TestCopyDirIfExists(t *testing.T) {
	src, dst := t.TempDir(), filepath.Join(t.TempDir(), "copy")
	if err := os.MkdirAll(filepath.Join(src, "sub"), 0o700); err != nil {
		t.Fatal(err)
	}
	for name, content := range map[string]string{"a": "A", "sub/b": "B", ".lock": ""} {
		if err := os.WriteFile(filepath.Join(src, name), []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if err := copyDirIfExists(src, dst); err != nil {
		t.Fatal(err)
	}
	for name, content := range map[string]string{"a": "A", "sub/b": "B", ".lock": ""} {
		if b, err := os.ReadFile(filepath.Join(dst, name)); err != nil || string(b) != content {
			t.Errorf("%s: %q %v", name, b, err)
		}
	}
	if err := copyDirIfExists(filepath.Join(src, "missing"), dst); err != nil {
		t.Errorf("missing source should be fine: %v", err)
	}
	if err := copyDirIfExists(filepath.Join(src, "a"), dst); err == nil {
		t.Error("a file as source should fail")
	}
}
