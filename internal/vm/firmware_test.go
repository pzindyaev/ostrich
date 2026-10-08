package vm

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// fakeFirmwareDirs points the descriptor lookup at test directories and
// disables the well-known-path fallback. It returns the directories, highest
// priority first, created empty.
func fakeFirmwareDirs(t *testing.T, n int) []string {
	t.Helper()
	var dirs []string
	for i := 0; i < n; i++ {
		dirs = append(dirs, t.TempDir())
	}
	oldDirs, oldKnown := firmwareDirs, knownFirmware
	firmwareDirs = func() []string { return dirs }
	knownFirmware = nil
	t.Cleanup(func() { firmwareDirs, knownFirmware = oldDirs, oldKnown })
	return dirs
}

// writeDescriptor writes a descriptor plus the (empty) firmware files it names.
func writeDescriptor(t *testing.T, dir, name, arch, machineGlob string, features []string, withFiles bool) (code, vars string) {
	t.Helper()
	code = filepath.Join(dir, name+"-code.fd")
	vars = filepath.Join(dir, name+"-vars.fd")
	if withFiles {
		os.WriteFile(code, []byte("code"), 0644)
		os.WriteFile(vars, []byte("vars-template"), 0644)
	}
	json := `{
  "description": "` + name + `",
  "interface-types": ["uefi"],
  "mapping": {"device": "flash",
    "executable": {"filename": "` + code + `", "format": "raw"},
    "nvram-template": {"filename": "` + vars + `", "format": "raw"}},
  "targets": [{"architecture": "` + arch + `", "machines": ["` + machineGlob + `"]}],
  "features": ["` + strings.Join(features, `","`) + `"]
}`
	if err := os.WriteFile(filepath.Join(dir, name+".json"), []byte(json), 0644); err != nil {
		t.Fatal(err)
	}
	return code, vars
}

func TestFindFirmwareFromDescriptors(t *testing.T) {
	dirs := fakeFirmwareDirs(t, 1)
	// File names sort the candidates: the Secure Boot one comes first, as on real hosts.
	sbCode, _ := writeDescriptor(t, dirs[0], "50-secure", "x86_64", "pc-q35-*", []string{"secure-boot", "requires-smm"}, true)
	plainCode, _ := writeDescriptor(t, dirs[0], "60-plain", "x86_64", "pc-q35-*", nil, true)
	writeDescriptor(t, dirs[0], "60-i440fx", "x86_64", "pc-i440fx-*", nil, true)
	writeDescriptor(t, dirs[0], "60-arm", "aarch64", "virt-*", nil, true)

	fw, err := FindFirmware("x86_64", "q35", false)
	if err != nil {
		t.Fatal(err)
	}
	if fw.Code != plainCode || fw.SecureBoot || fw.RequiresSMM {
		t.Errorf("plain UEFI should pick the non-Secure-Boot image, got %+v", fw)
	}

	fw, err = FindFirmware("x86_64", "q35", true)
	if err != nil {
		t.Fatal(err)
	}
	if fw.Code != sbCode || !fw.SecureBoot || !fw.RequiresSMM || fw.EnrolledKeys {
		t.Errorf("Secure Boot should pick the secboot image, got %+v", fw)
	}
	if fw.CodeFormat != "raw" || fw.VarsFormat != "raw" {
		t.Errorf("formats not carried over: %+v", fw)
	}

	if _, err := FindFirmware("x86_64", "pc", true); err == nil {
		t.Error("i440fx has no Secure Boot descriptor, expected an error")
	}
	if fw, err := FindFirmware("aarch64", "virt", false); err != nil || !strings.Contains(fw.Code, "60-arm") {
		t.Errorf("aarch64 lookup: %+v, %v", fw, err)
	}
	if _, err := FindFirmware("riscv64", "virt", false); err == nil || !strings.Contains(err.Error(), "riscv64") {
		t.Errorf("missing arch should fail with an install hint, got %v", err)
	}
}

func TestFindFirmwarePrefersEnrolledKeysAndSkipsMissingFiles(t *testing.T) {
	dirs := fakeFirmwareDirs(t, 1)
	writeDescriptor(t, dirs[0], "40-secure-gone", "x86_64", "pc-q35-*", []string{"secure-boot", "enrolled-keys"}, false)
	writeDescriptor(t, dirs[0], "50-secure", "x86_64", "pc-q35-*", []string{"secure-boot", "requires-smm"}, true)
	enrolledCode, _ := writeDescriptor(t, dirs[0], "55-secure-enrolled", "x86_64", "pc-q35-*", []string{"secure-boot", "requires-smm", "enrolled-keys"}, true)

	fw, err := FindFirmware("x86_64", "q35", true)
	if err != nil {
		t.Fatal(err)
	}
	if fw.Code != enrolledCode || !fw.EnrolledKeys {
		t.Errorf("expected the enrolled-keys firmware, got %+v", fw)
	}
	// With only Secure Boot images around, plain UEFI falls back to one of them.
	if fw, err := FindFirmware("x86_64", "q35", false); err != nil || !fw.SecureBoot {
		t.Errorf("plain UEFI fallback: %+v, %v", fw, err)
	}
}

func TestFirmwareDescriptorPriorityAndMasking(t *testing.T) {
	dirs := fakeFirmwareDirs(t, 2) // dirs[0] overrides dirs[1]
	lowCode, _ := writeDescriptor(t, dirs[1], "60-ovmf", "x86_64", "pc-q35-*", nil, true)
	highCode, _ := writeDescriptor(t, dirs[0], "60-ovmf", "x86_64", "pc-q35-*", nil, true)
	fw, err := FindFirmware("x86_64", "q35", false)
	if err != nil {
		t.Fatal(err)
	}
	if fw.Code != highCode || fw.Code == lowCode {
		t.Errorf("higher-priority directory should win, got %s", fw.Code)
	}

	// An empty file in the top directory masks the descriptor below.
	os.WriteFile(filepath.Join(dirs[0], "60-ovmf.json"), nil, 0644)
	if _, err := FindFirmware("x86_64", "q35", false); err == nil {
		t.Error("masked descriptor still found")
	}
}

func TestFindFirmwareKnownPaths(t *testing.T) {
	fakeFirmwareDirs(t, 1)
	dir := t.TempDir()
	code, vars := filepath.Join(dir, "OVMF_CODE.secboot.fd"), filepath.Join(dir, "OVMF_VARS.fd")
	os.WriteFile(code, []byte("c"), 0644)
	os.WriteFile(vars, []byte("v"), 0644)
	knownFirmware = map[string][]Firmware{"x86_64": {
		{Code: filepath.Join(dir, "missing.fd"), VarsTemplate: vars, SecureBoot: true, EnrolledKeys: true},
		{Code: code, VarsTemplate: vars, SecureBoot: true, RequiresSMM: true},
	}}
	fw, err := FindFirmware("x86_64", "q35", true)
	if err != nil {
		t.Fatal(err)
	}
	if fw.Code != code || !fw.RequiresSMM || fw.CodeFormat != "raw" {
		t.Errorf("got %+v", fw)
	}
}

func TestEnsureFirmwareVars(t *testing.T) {
	dirs := fakeFirmwareDirs(t, 1)
	_, tmpl := writeDescriptor(t, dirs[0], "50-secure", "x86_64", "pc-q35-*", []string{"secure-boot", "requires-smm"}, true)
	storage := t.TempDir()
	os.MkdirAll(VMDir(storage, "vm"), 0755)

	bios := &VMConfig{Name: "vm"}
	if err := EnsureFirmwareVars(storage, bios); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(FirmwareVarsPath(storage, "vm")); err == nil {
		t.Error("BIOS VM should not get an NVRAM store")
	}

	uefi := &VMConfig{Name: "vm", Firmware: FirmwareUEFI}
	if err := EnsureFirmwareVars(storage, uefi); err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(FirmwareVarsPath(storage, "vm"))
	if string(got) != "vars-template" {
		t.Errorf("NVRAM should be a copy of the template, got %q", got)
	}
	// An existing store is left alone.
	os.WriteFile(FirmwareVarsPath(storage, "vm"), []byte("guest-state"), 0644)
	if err := EnsureFirmwareVars(storage, uefi); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(FirmwareVarsPath(storage, "vm")); string(got) != "guest-state" {
		t.Errorf("existing NVRAM overwritten: %q", got)
	}
	os.Remove(FirmwareVarsPath(storage, "vm"))

	// Secure Boot without enrolled keys needs virt-fw-vars: fail clearly without it...
	t.Setenv("PATH", t.TempDir())
	sb := &VMConfig{Name: "vm", SecureBoot: true}
	err := EnsureFirmwareVars(storage, sb)
	if err == nil || !strings.Contains(err.Error(), enrollTool) {
		t.Fatalf("expected a %s hint, got %v", enrollTool, err)
	}
	if _, err := os.Stat(FirmwareVarsPath(storage, "vm") + ".tmp"); err == nil {
		t.Error("temp file left behind")
	}

	// ...and call it with the template and the enrolment flags when present.
	bin := t.TempDir()
	fake := filepath.Join(bin, enrollTool)
	script := "#!/bin/sh\necho \"$@\" > " + filepath.Join(bin, "args") + "\n" +
		"while [ $# -gt 0 ]; do [ \"$1\" = --output ] && printf enrolled > \"$2\"; shift; done\n"
	os.WriteFile(fake, []byte(script), 0755)
	t.Setenv("PATH", bin)
	if err := EnsureFirmwareVars(storage, sb); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(FirmwareVarsPath(storage, "vm")); string(got) != "enrolled" {
		t.Errorf("NVRAM should be the tool's output, got %q", got)
	}
	args, _ := os.ReadFile(filepath.Join(bin, "args"))
	for _, want := range []string{"--input " + tmpl, "--enroll-generate", "--microsoft-kek all", "--microsoft-db all", "--secure-boot"} {
		if !strings.Contains(string(args), want) {
			t.Errorf("%s not called with %q: %s", enrollTool, want, args)
		}
	}

	// A template that already has the keys is simply copied.
	os.Remove(FirmwareVarsPath(storage, "vm"))
	os.Remove(filepath.Join(bin, "args"))
	writeDescriptor(t, dirs[0], "40-enrolled", "x86_64", "pc-q35-*", []string{"secure-boot", "enrolled-keys"}, true)
	if err := EnsureFirmwareVars(storage, sb); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(bin, "args")); err == nil {
		t.Error("enrolled template should not go through the enroll tool")
	}
}

func TestBuildQEMUArgsFirmwareAndTPM(t *testing.T) {
	dirs := fakeFirmwareDirs(t, 1)
	sbCode, _ := writeDescriptor(t, dirs[0], "50-secure", "x86_64", "pc-q35-*", []string{"secure-boot", "requires-smm"}, true)
	plainCode, _ := writeDescriptor(t, dirs[0], "60-plain", "x86_64", "pc-q35-*", nil, true)
	storage := t.TempDir()

	_, args, err := BuildQEMUArgs(&VMConfig{Name: "t", CPU: 1, RAM: 128}, storage)
	if err != nil {
		t.Fatal(err)
	}
	joined := strings.Join(args, " ")
	if strings.Contains(joined, "pflash") || strings.Contains(joined, "smm=on") || strings.Contains(joined, "tpm") {
		t.Errorf("BIOS VM got firmware/TPM args: %s", joined)
	}

	_, args, err = BuildQEMUArgs(&VMConfig{Name: "t", CPU: 1, RAM: 128, Firmware: FirmwareUEFI}, storage)
	if err != nil {
		t.Fatal(err)
	}
	joined = strings.Join(args, " ")
	if !strings.Contains(joined, "-machine q35 ") || strings.Contains(joined, "smm=on") {
		t.Errorf("plain UEFI should not enable SMM: %s", joined)
	}
	if !strings.Contains(joined, "-drive if=pflash,format=raw,unit=0,readonly=on,file="+plainCode) ||
		!strings.Contains(joined, "-drive if=pflash,format=raw,unit=1,file="+FirmwareVarsPath(storage, "t")) {
		t.Errorf("pflash drives missing: %s", joined)
	}

	_, args, err = BuildQEMUArgs(&VMConfig{Name: "t", CPU: 1, RAM: 128, SecureBoot: true, TPM: true}, storage)
	if err != nil {
		t.Fatal(err)
	}
	joined = strings.Join(args, " ")
	for _, want := range []string{
		"-machine q35,smm=on",
		"-global driver=cfi.pflash01,property=secure,value=on",
		"unit=0,readonly=on,file=" + sbCode,
		"-chardev socket,id=chrtpm,path=" + TPMSockPath(storage, "t"),
		"-tpmdev emulator,id=tpm0,chardev=chrtpm",
		"-device tpm-tis,tpmdev=tpm0",
	} {
		if !strings.Contains(joined, want) {
			t.Errorf("missing %q: %s", want, joined)
		}
	}

	knownFirmware = nil
	os.Remove(sbCode)
	if _, _, err := BuildQEMUArgs(&VMConfig{Name: "t", CPU: 1, RAM: 128, SecureBoot: true}, storage); err == nil {
		t.Error("missing firmware should fail")
	}
}

func TestFirmwareLabel(t *testing.T) {
	for _, tc := range []struct {
		cfg  VMConfig
		want string
	}{
		{VMConfig{}, "BIOS"},
		{VMConfig{Firmware: FirmwareBIOS, TPM: true}, "BIOS, TPM 2.0"},
		{VMConfig{Firmware: FirmwareUEFI}, "UEFI"},
		{VMConfig{Firmware: FirmwareUEFI, SecureBoot: true}, "UEFI + Secure Boot"},
		{VMConfig{SecureBoot: true, TPM: true}, "UEFI + Secure Boot, TPM 2.0"},
	} {
		if got := tc.cfg.FirmwareLabel(); got != tc.want {
			t.Errorf("%+v: got %q, want %q", tc.cfg, got, tc.want)
		}
		if tc.cfg.SecureBoot && !tc.cfg.UEFI() {
			t.Errorf("%+v: Secure Boot should imply UEFI", tc.cfg)
		}
	}
}

func TestManagerUpdateFirmware(t *testing.T) {
	dirs := fakeFirmwareDirs(t, 1)
	writeDescriptor(t, dirs[0], "50-secure", "x86_64", "pc-q35-*", []string{"secure-boot", "requires-smm", "enrolled-keys"}, true)
	writeDescriptor(t, dirs[0], "60-plain", "x86_64", "pc-q35-*", nil, true)
	if _, err := exec.LookPath("qemu-img"); err != nil {
		t.Skip("qemu-img not installed")
	}
	storage := t.TempDir()
	m := NewManager(storage)
	cfg := &VMConfig{Name: "fw", CPU: 1, RAM: 128, DiskSize: 1, Network: NetworkConfig{Type: NetworkNone}}
	if err := m.Create(cfg); err != nil {
		t.Fatal(err)
	}
	vars := FirmwareVarsPath(storage, "fw")
	if _, err := os.Stat(vars); err == nil {
		t.Fatal("BIOS VM should have no NVRAM")
	}

	// BIOS → UEFI creates the store.
	edited := *cfg
	edited.Firmware = FirmwareUEFI
	if err := m.Update("fw", &edited); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(vars); err != nil {
		t.Fatal("UEFI VM should have an NVRAM store")
	}
	os.WriteFile(vars, []byte("guest-state"), 0644)

	// UEFI → UEFI + Secure Boot rebuilds it with keys.
	edited.SecureBoot = true
	if err := m.Update("fw", &edited); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(vars); string(got) != "vars-template" {
		t.Errorf("enabling Secure Boot should rebuild NVRAM, got %q", got)
	}
	os.WriteFile(vars, []byte("guest-state"), 0644)

	// Turning Secure Boot off keeps it, and so does going back to BIOS.
	edited.SecureBoot = false
	if err := m.Update("fw", &edited); err != nil {
		t.Fatal(err)
	}
	edited.Firmware = FirmwareBIOS
	if err := m.Update("fw", &edited); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(vars); string(got) != "guest-state" {
		t.Errorf("NVRAM should survive disabling Secure Boot/UEFI, got %q", got)
	}
	saved, _ := LoadConfig(storage, "fw")
	if saved.Firmware != FirmwareBIOS || saved.SecureBoot {
		t.Errorf("saved config: %+v", saved)
	}

	// TPM needs swtpm on the host.
	t.Setenv("PATH", t.TempDir())
	edited.TPM = true
	if err := m.Update("fw", &edited); err == nil || !strings.Contains(err.Error(), tpmBin) {
		t.Errorf("expected a %s hint, got %v", tpmBin, err)
	}
}
